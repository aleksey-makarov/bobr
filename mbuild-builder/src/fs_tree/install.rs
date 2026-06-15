use globset::{Glob, GlobMatcher};
use mbuild_core::BuilderError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallMeta {
    pub(super) rules: Vec<InstallRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallRule {
    pub(super) path: String,
    pub(super) attrs: InstallAttrs,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallAttrs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) gid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) directory_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) regular_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) executable_file_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) symlink_mode: Option<u32>,
}

#[derive(Debug)]
pub(super) struct CompiledInstallRule {
    pattern: String,
    matcher: GlobMatcher,
    attrs: InstallAttrs,
}

pub(super) fn compile_install_rules(
    rules: &[InstallRule],
) -> Result<Vec<CompiledInstallRule>, BuilderError> {
    rules
        .iter()
        .map(|rule| {
            let glob = Glob::new(&rule.path).map_err(|error| {
                BuilderError::InvalidRecipe(format!(
                    "invalid builder config: invalid install rule pattern '{}': {error}",
                    rule.path
                ))
            })?;
            Ok(CompiledInstallRule {
                pattern: rule.path.clone(),
                matcher: glob.compile_matcher(),
                attrs: rule.attrs.clone(),
            })
        })
        .collect()
}

pub(super) fn resolve_install_attrs(
    rel_path: &str,
    rules: &[CompiledInstallRule],
) -> Result<InstallAttrs, BuilderError> {
    let mut resolved = InstallAttrs::default();
    let mut matched_any = false;
    for rule in rules {
        if install_rule_matches(rule, rel_path) {
            matched_any = true;
            if let Some(uid) = rule.attrs.uid {
                resolved.uid = Some(uid);
            }
            if let Some(gid) = rule.attrs.gid {
                resolved.gid = Some(gid);
            }
            if let Some(mode) = rule.attrs.directory_mode {
                resolved.directory_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.regular_file_mode {
                resolved.regular_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.executable_file_mode {
                resolved.executable_file_mode = Some(mode);
            }
            if let Some(mode) = rule.attrs.symlink_mode {
                resolved.symlink_mode = Some(mode);
            }
        }
    }

    if matched_any {
        Ok(resolved)
    } else {
        let known = rules
            .iter()
            .map(|rule| rule.pattern.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Err(BuilderError::InvalidRecipe(format!(
            "invalid builder config: path '{rel_path}' is not covered by any install rule (known patterns: {known})"
        )))
    }
}

fn install_rule_matches(rule: &CompiledInstallRule, rel_path: &str) -> bool {
    if rule.matcher.is_match(rel_path) {
        return true;
    }

    if let Some(prefix) = rule.pattern.strip_suffix("/**") {
        return rel_path == prefix;
    }

    false
}
