//! Typed HostBundle process-environment construction.

use crate::{
    BundleConfig, BundleLocation, EnvironmentOperation, EnvironmentRule,
    EnvironmentRuleValidationError, ResolvedTool, ToolResolutionError,
};
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Origin of the final value of one process environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentOrigin {
    /// The value was inherited unchanged from the launching process.
    Host,
    /// A common bundle rule produced the final value.
    Common(EnvironmentOperation),
    /// A per-tool rule produced the final value.
    Tool(EnvironmentOperation),
}

/// Fully evaluated process environment, including inherited host values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEnvironment {
    values: BTreeMap<OsString, OsString>,
    traces: BTreeMap<OsString, Vec<EnvironmentOrigin>>,
}

impl ProcessEnvironment {
    /// Returns the final value of `name`, or `None` when it is unset.
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.values.get(name.as_ref()).map(OsString::as_os_str)
    }

    /// Returns the number of variables in the final environment.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns whether the final environment is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Iterates over final variable names and values in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
    }

    /// Returns where the final value of `name` came from.
    pub fn origin(&self, name: impl AsRef<OsStr>) -> Option<EnvironmentOrigin> {
        self.traces.get(name.as_ref())?.last().copied()
    }

    /// Returns all sources that contributed to the final value, in order.
    pub fn trace(&self, name: impl AsRef<OsStr>) -> Option<&[EnvironmentOrigin]> {
        self.traces.get(name.as_ref()).map(Vec::as_slice)
    }
}

/// Invalid environment policy or an unsafe configured environment path.
#[derive(Debug)]
pub enum EnvironmentError {
    /// A variable name is empty or cannot be passed to `execve`.
    InvalidVariableName(String),
    /// A rule combines fields that are meaningless for its operation.
    InvalidRule {
        /// Variable whose rule is invalid.
        variable: String,
        /// Human-readable semantic failure.
        reason: &'static str,
    },
    /// A configured path has an unsafe lexical form.
    InvalidPath(ToolResolutionError),
    /// A configured path could not be resolved.
    ResolvePath {
        /// Variable whose rule contains the path.
        variable: String,
        /// Host path attempted by the resolver.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// A configured path resolves outside the HostBundle.
    PathEscapesBundle {
        /// Variable whose rule contains the path.
        variable: String,
        /// Canonical HostBundle root.
        bundle_root: PathBuf,
        /// Canonical escaped path.
        path: PathBuf,
    },
    /// A path contains the platform path-list separator.
    PathContainsSeparator {
        /// Variable whose rule contains the path.
        variable: String,
        /// Rejected path.
        path: PathBuf,
    },
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVariableName(name) => {
                write!(formatter, "invalid environment variable name '{name}'")
            }
            Self::InvalidRule { variable, reason } => {
                write!(
                    formatter,
                    "invalid environment rule for {variable}: {reason}"
                )
            }
            Self::InvalidPath(error) => error.fmt(formatter),
            Self::ResolvePath {
                variable,
                path,
                source,
            } => write!(
                formatter,
                "failed to resolve environment path for {variable} ('{}'): {source}",
                path.display()
            ),
            Self::PathEscapesBundle {
                variable,
                bundle_root,
                path,
            } => write!(
                formatter,
                "environment path for {variable} ('{}') resolves outside bundle root '{}'",
                path.display(),
                bundle_root.display()
            ),
            Self::PathContainsSeparator { variable, path } => write!(
                formatter,
                "environment path for {variable} ('{}') contains ':'",
                path.display()
            ),
        }
    }
}

impl Error for EnvironmentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPath(error) => Some(error),
            Self::ResolvePath { source, .. } => Some(source),
            Self::InvalidVariableName(_)
            | Self::InvalidRule { .. }
            | Self::PathEscapesBundle { .. }
            | Self::PathContainsSeparator { .. } => None,
        }
    }
}

/// Applies common and per-tool environment policy to a captured host environment.
pub fn build_environment(
    location: &BundleLocation,
    bundle: &BundleConfig,
    tool: &ResolvedTool,
    host: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<ProcessEnvironment, EnvironmentError> {
    let mut values = host
        .into_iter()
        .filter(|(name, _)| !is_loader_sensitive(name))
        .collect::<BTreeMap<_, _>>();
    let mut traces = values
        .keys()
        .cloned()
        .map(|name| (name, vec![EnvironmentOrigin::Host]))
        .collect::<BTreeMap<_, _>>();
    let bundle_root =
        fs::canonicalize(location.root()).map_err(|source| EnvironmentError::ResolvePath {
            variable: "<bundle-root>".to_string(),
            path: location.root().to_path_buf(),
            source,
        })?;

    apply_rules(
        &mut values,
        &mut traces,
        &bundle_root,
        &bundle.environment,
        RuleScope::Common,
    )?;
    apply_rules(
        &mut values,
        &mut traces,
        &bundle_root,
        &tool.config().environment,
        RuleScope::Tool,
    )?;

    Ok(ProcessEnvironment { values, traces })
}

#[derive(Clone, Copy)]
enum RuleScope {
    Common,
    Tool,
}

impl RuleScope {
    fn origin(self, operation: EnvironmentOperation) -> EnvironmentOrigin {
        match self {
            Self::Common => EnvironmentOrigin::Common(operation),
            Self::Tool => EnvironmentOrigin::Tool(operation),
        }
    }
}

fn apply_rules(
    values: &mut BTreeMap<OsString, OsString>,
    traces: &mut BTreeMap<OsString, Vec<EnvironmentOrigin>>,
    bundle_root: &Path,
    rules: &BTreeMap<String, EnvironmentRule>,
    scope: RuleScope,
) -> Result<(), EnvironmentError> {
    for (variable, rule) in rules {
        rule.validate(variable).map_err(map_rule_validation_error)?;
        let configured_values = configured_values(bundle_root, variable, rule)?;
        let name = OsString::from(variable);

        match rule.operation {
            EnvironmentOperation::Unset => {
                values.remove(&name);
                traces.remove(&name);
            }
            EnvironmentOperation::Replace => {
                values.insert(
                    name.clone(),
                    join_list(configured_values.iter().map(OsString::as_os_str)),
                );
                traces.insert(name, vec![scope.origin(rule.operation)]);
            }
            EnvironmentOperation::Prepend | EnvironmentOperation::Append => {
                let inherited = if rule.inherit {
                    values
                        .get(&name)
                        .cloned()
                        .or_else(|| host_default_value(&rule.host_default))
                } else {
                    None
                };
                let inherited_contributes =
                    inherited.as_ref().is_some_and(|value| !value.is_empty());
                let bundle_value = join_list(configured_values.iter().map(OsString::as_os_str));
                let combined = match (rule.operation, inherited) {
                    (EnvironmentOperation::Prepend, Some(host)) => join_pair(&bundle_value, &host),
                    (EnvironmentOperation::Append, Some(host)) => join_pair(&host, &bundle_value),
                    (_, None) => bundle_value,
                    _ => unreachable!("operation was restricted to prepend or append"),
                };
                values.insert(name.clone(), combined);
                let origin = scope.origin(rule.operation);
                if inherited_contributes {
                    traces.entry(name).or_default().push(origin);
                } else {
                    traces.insert(name, vec![origin]);
                }
            }
            EnvironmentOperation::Default => {
                if !values.contains_key(&name) {
                    let value = if configured_values.is_empty() {
                        host_default_value(&rule.host_default).unwrap_or_default()
                    } else {
                        join_list(configured_values.iter().map(OsString::as_os_str))
                    };
                    values.insert(name.clone(), value);
                    traces.insert(name, vec![scope.origin(rule.operation)]);
                }
            }
        }
    }
    Ok(())
}

fn map_rule_validation_error(error: EnvironmentRuleValidationError) -> EnvironmentError {
    match error {
        EnvironmentRuleValidationError::InvalidVariableName(name) => {
            EnvironmentError::InvalidVariableName(name)
        }
        EnvironmentRuleValidationError::InvalidRule { variable, reason } => {
            EnvironmentError::InvalidRule { variable, reason }
        }
    }
}

fn configured_values(
    bundle_root: &Path,
    variable: &str,
    rule: &EnvironmentRule,
) -> Result<Vec<OsString>, EnvironmentError> {
    if !rule.paths.is_empty() {
        resolve_bundle_values(bundle_root, variable, &rule.paths)
    } else {
        Ok(rule.values.iter().map(OsString::from).collect())
    }
}

fn resolve_bundle_values(
    bundle_root: &Path,
    variable: &str,
    paths: &[String],
) -> Result<Vec<OsString>, EnvironmentError> {
    paths
        .iter()
        .map(|relative| {
            let field = format!("environment.{variable}.paths");
            crate::tool::validate_relative_path(&field, relative)
                .map_err(EnvironmentError::InvalidPath)?;
            let candidate = bundle_root.join(relative);
            let resolved =
                fs::canonicalize(&candidate).map_err(|source| EnvironmentError::ResolvePath {
                    variable: variable.to_string(),
                    path: candidate,
                    source,
                })?;
            if !resolved.starts_with(bundle_root) {
                return Err(EnvironmentError::PathEscapesBundle {
                    variable: variable.to_string(),
                    bundle_root: bundle_root.to_path_buf(),
                    path: resolved,
                });
            }
            if resolved.as_os_str().as_bytes().contains(&b':') {
                return Err(EnvironmentError::PathContainsSeparator {
                    variable: variable.to_string(),
                    path: resolved,
                });
            }
            Ok(resolved.into_os_string())
        })
        .collect()
}

fn host_default_value(defaults: &[String]) -> Option<OsString> {
    (!defaults.is_empty()).then(|| join_list(defaults.iter().map(OsStr::new)))
}

fn join_list<'a>(values: impl IntoIterator<Item = &'a OsStr>) -> OsString {
    let mut output = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        if index != 0 {
            output.push(b':');
        }
        output.extend_from_slice(value.as_bytes());
    }
    OsString::from_vec(output)
}

fn join_pair(first: &OsStr, second: &OsStr) -> OsString {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => OsString::new(),
        (true, false) => second.to_os_string(),
        (false, true) => first.to_os_string(),
        (false, false) => join_list([first, second]),
    }
}

fn is_loader_sensitive(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.starts_with(b"LD_") || bytes == b"GLIBC_TUNABLES"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BUNDLE_FORMAT_V2, locate_bundle_from_launcher, resolve_tool};
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _temp: tempfile::TempDir,
        location: BundleLocation,
        config: BundleConfig,
        tool: ResolvedTool,
    }

    impl Fixture {
        fn new(common: &str, per_tool: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("bundle");
            fs::create_dir_all(root.join("libexec/wrapped-bin")).unwrap();
            fs::create_dir_all(root.join("root/usr/bin")).unwrap();
            fs::create_dir_all(root.join("root/usr/share")).unwrap();
            fs::create_dir_all(root.join("overrides")).unwrap();
            let target = root.join("root/usr/bin/demo");
            fs::write(&target, b"fixture").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
            let location =
                locate_bundle_from_launcher(&root.join("libexec/bobr-bundle-launcher")).unwrap();
            let config = BundleConfig::parse(&format!(
                r#"
format = "{BUNDLE_FORMAT_V2}"
payload_root = "root"
policy = "strict"
[platform]
os = "linux"
arch = "x86_64"
min_kernel = "4.19"
[loader]
kind = "glibc"
library_dirs = []
inhibit_cache = true
{common}
[tools.demo]
path = "root/usr/bin/demo"
argv0 = "demo"
visibility = "public"
{per_tool}
"#
            ))
            .unwrap();
            let tool = resolve_tool(&location, &config, "demo").unwrap();
            Self {
                _temp: temp,
                location,
                config,
                tool,
            }
        }

        fn environment(
            &self,
            host: impl IntoIterator<Item = (OsString, OsString)>,
        ) -> Result<ProcessEnvironment, EnvironmentError> {
            build_environment(&self.location, &self.config, &self.tool, host)
        }
    }

    fn host(values: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        values
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn replace_uses_absolute_bundle_paths() {
        let fixture = Fixture::new(
            r#"
[environment.XDG_DATA_DIRS]
operation = "replace"
paths = ["root/usr/share"]
"#,
            "",
        );

        let environment = fixture
            .environment(host(&[("XDG_DATA_DIRS", "/host/share")]))
            .unwrap();

        assert_eq!(
            environment.get("XDG_DATA_DIRS"),
            Some(fixture.location.root().join("root/usr/share").as_os_str())
        );
    }

    #[test]
    fn prepend_uses_host_default_only_when_host_is_absent() {
        let fixture = Fixture::new(
            r#"
[environment.XDG_DATA_DIRS]
operation = "prepend"
paths = ["root/usr/share"]
inherit = true
host_default = ["/usr/local/share", "/usr/share"]
"#,
            "",
        );
        let prefix = fixture.location.root().join("root/usr/share");

        let absent = fixture.environment(host(&[])).unwrap();
        let present = fixture
            .environment(host(&[("XDG_DATA_DIRS", "/host/share")]))
            .unwrap();
        let empty = fixture.environment(host(&[("XDG_DATA_DIRS", "")])).unwrap();

        assert_eq!(
            absent.get("XDG_DATA_DIRS"),
            Some(OsStr::new(&format!(
                "{}:/usr/local/share:/usr/share",
                prefix.display()
            )))
        );
        assert_eq!(
            present.get("XDG_DATA_DIRS"),
            Some(OsStr::new(&format!("{}:/host/share", prefix.display())))
        );
        assert_eq!(empty.get("XDG_DATA_DIRS"), Some(prefix.as_os_str()));
        assert_eq!(
            empty.trace("XDG_DATA_DIRS"),
            Some([EnvironmentOrigin::Common(EnvironmentOperation::Prepend)].as_slice())
        );
    }

    #[test]
    fn append_places_bundle_path_after_host_value() {
        let fixture = Fixture::new(
            r#"
[environment.PATH]
operation = "append"
paths = ["libexec/wrapped-bin"]
inherit = true
"#,
            "",
        );

        let environment = fixture.environment(host(&[("PATH", "/host/bin")])).unwrap();

        assert_eq!(
            environment.get("PATH"),
            Some(OsStr::new(&format!(
                "/host/bin:{}",
                fixture
                    .location
                    .root()
                    .join("libexec/wrapped-bin")
                    .display()
            )))
        );
        assert_eq!(
            environment.trace("PATH"),
            Some(
                [
                    EnvironmentOrigin::Host,
                    EnvironmentOrigin::Common(EnvironmentOperation::Append),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn unset_removes_inherited_variable() {
        let fixture = Fixture::new(
            r#"
[environment.BAD]
operation = "unset"
"#,
            "",
        );

        let environment = fixture.environment(host(&[("BAD", "value")])).unwrap();

        assert_eq!(environment.get("BAD"), None);
    }

    #[test]
    fn default_distinguishes_absent_and_present_empty() {
        let fixture = Fixture::new(
            r#"
[environment.DATA]
operation = "default"
paths = ["root/usr/share"]
"#,
            "",
        );

        let absent = fixture.environment(host(&[])).unwrap();
        let empty = fixture.environment(host(&[("DATA", "")])).unwrap();

        assert_eq!(
            absent.get("DATA"),
            Some(fixture.location.root().join("root/usr/share").as_os_str())
        );
        assert_eq!(empty.get("DATA"), Some(OsStr::new("")));
    }

    #[test]
    fn literal_values_are_not_resolved_as_bundle_paths() {
        let fixture = Fixture::new(
            r#"
[environment.QEMU_AUDIO_DRV]
operation = "default"
values = ["none"]
"#,
            "",
        );

        let environment = fixture.environment(host(&[])).unwrap();

        assert_eq!(environment.get("QEMU_AUDIO_DRV"), Some(OsStr::new("none")));
        assert_eq!(
            environment.origin("QEMU_AUDIO_DRV"),
            Some(EnvironmentOrigin::Common(EnvironmentOperation::Default))
        );
    }

    #[test]
    fn records_host_common_and_tool_origins() {
        let fixture = Fixture::new(
            r#"
[environment.COMMON]
operation = "replace"
values = ["common"]
"#,
            r#"
[tools.demo.environment.TOOL]
operation = "replace"
values = ["tool"]
"#,
        );
        let environment = fixture.environment(host(&[("HOST", "host")])).unwrap();

        assert_eq!(environment.origin("HOST"), Some(EnvironmentOrigin::Host));
        assert_eq!(
            environment.origin("COMMON"),
            Some(EnvironmentOrigin::Common(EnvironmentOperation::Replace))
        );
        assert_eq!(
            environment.origin("TOOL"),
            Some(EnvironmentOrigin::Tool(EnvironmentOperation::Replace))
        );
    }

    #[test]
    fn per_tool_rules_apply_after_common_rules() {
        let fixture = Fixture::new(
            r#"
[environment.PATH]
operation = "prepend"
paths = ["libexec/wrapped-bin"]
inherit = true
"#,
            r#"
[tools.demo.environment.PATH]
operation = "prepend"
paths = ["root/usr/bin"]
inherit = true
"#,
        );

        let environment = fixture.environment(host(&[("PATH", "/host/bin")])).unwrap();

        assert_eq!(
            environment.get("PATH"),
            Some(OsStr::new(&format!(
                "{}:{}:/host/bin",
                fixture.location.root().join("root/usr/bin").display(),
                fixture
                    .location
                    .root()
                    .join("libexec/wrapped-bin")
                    .display()
            )))
        );
        assert_eq!(
            environment.trace("PATH"),
            Some(
                [
                    EnvironmentOrigin::Host,
                    EnvironmentOrigin::Common(EnvironmentOperation::Prepend),
                    EnvironmentOrigin::Tool(EnvironmentOperation::Prepend),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn loader_sensitive_host_variables_are_removed() {
        let fixture = Fixture::new("", "");

        let environment = fixture
            .environment(host(&[
                ("LD_LIBRARY_PATH", "/host/lib"),
                ("LD_PRELOAD", "bad.so"),
                ("LD_DEBUG", "libs"),
                ("GLIBC_TUNABLES", "glibc.malloc.check=3"),
                ("HOME", "/home/user"),
            ]))
            .unwrap();

        assert_eq!(environment.get("LD_LIBRARY_PATH"), None);
        assert_eq!(environment.get("LD_PRELOAD"), None);
        assert_eq!(environment.get("LD_DEBUG"), None);
        assert_eq!(environment.get("GLIBC_TUNABLES"), None);
        assert_eq!(environment.get("HOME"), Some(OsStr::new("/home/user")));
    }

    #[test]
    fn explicit_rule_can_restore_loader_variable() {
        let fixture = Fixture::new(
            r#"
[environment.LD_LIBRARY_PATH]
operation = "replace"
paths = ["root/usr/share"]
"#,
            "",
        );

        let environment = fixture
            .environment(host(&[("LD_LIBRARY_PATH", "/host/lib")]))
            .unwrap();

        assert_eq!(
            environment.get("LD_LIBRARY_PATH"),
            Some(fixture.location.root().join("root/usr/share").as_os_str())
        );
    }

    #[test]
    fn preserves_non_utf8_host_values() {
        let fixture = Fixture::new("", "");
        let opaque = OsString::from_vec(vec![0xff, 0xfe]);

        let environment = fixture
            .environment(vec![(OsString::from("OPAQUE"), opaque.clone())])
            .unwrap();

        assert_eq!(environment.get("OPAQUE"), Some(opaque.as_os_str()));
    }

    #[test]
    fn rejects_invalid_rule_shapes() {
        let cases = [
            (
                r#"
[environment.BAD]
operation = "replace"
"#,
                "replace requires",
            ),
            (
                r#"
[environment.BAD]
operation = "unset"
paths = ["root/usr/share"]
"#,
                "unset accepts no",
            ),
            (
                r#"
[environment.BAD]
operation = "prepend"
paths = ["root/usr/share"]
host_default = ["/host"]
"#,
                "host_default requires",
            ),
            (
                r#"
[environment.BAD]
operation = "replace"
paths = ["root/usr/share"]
values = ["literal"]
"#,
                "cannot be combined",
            ),
        ];

        for (rule, expected) in cases {
            let fixture = Fixture::new(rule, "");
            let error = fixture.environment(host(&[])).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{error} does not contain {expected:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_variable_name() {
        let fixture = Fixture::new(
            r#"
[environment."BAD=NAME"]
operation = "unset"
"#,
            "",
        );

        let error = fixture.environment(host(&[])).unwrap_err();

        assert!(matches!(error, EnvironmentError::InvalidVariableName(_)));
    }

    #[test]
    fn rejects_environment_path_that_escapes_bundle() {
        let fixture = Fixture::new(
            r#"
[environment.BAD]
operation = "replace"
paths = ["escape"]
"#,
            "",
        );
        std::os::unix::fs::symlink(
            std::env::current_exe().unwrap(),
            fixture.location.root().join("escape"),
        )
        .unwrap();

        let error = fixture.environment(host(&[])).unwrap_err();

        assert!(matches!(error, EnvironmentError::PathEscapesBundle { .. }));
    }
}
