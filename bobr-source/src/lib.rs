mod http;
pub mod oci_registry;
pub mod origin;
mod origins;

pub use origin::*;

use bobr_core::{BuildKey, BuildLogSubject, ObjectHash, SubjectRunContext, Workspace};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// Error reported while parsing a source recipe node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRecipeError {
    message: String,
}

impl SourceRecipeError {
    /// Creates a source recipe parse error from a user-facing message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the user-facing error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SourceRecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SourceRecipeError {}

/// Error reported while executing a planned source subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceExecutionError {
    /// The run was cancelled before source materialization started.
    Cancelled(String),
    /// Source materialization failed.
    Build(String),
}

impl SourceExecutionError {
    fn cancelled() -> Self {
        Self::Cancelled("build cancelled by signal".to_string())
    }

    fn build(message: impl Into<String>) -> Self {
        Self::Build(message.into())
    }
}

impl fmt::Display for SourceExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(message) | Self::Build(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SourceExecutionError {}

/// Parsed source node prepared for graph planning and source execution.
#[derive(Debug, Clone)]
pub struct SourcePlannedSubject {
    name: String,
    build_key: BuildKey,
    declared_object_hash: ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
}

impl SourcePlannedSubject {
    /// Creates a parsed source subject from its recipe name, declared object
    /// hash, and optional materialization origin. The source build key is
    /// derived from the declared object hash.
    pub fn new(
        name: String,
        declared_object_hash: ObjectHash,
        origin: Option<Box<dyn ParsedOrigin>>,
    ) -> Self {
        Self {
            name,
            build_key: BuildKey::from_object_hash(declared_object_hash),
            declared_object_hash,
            origin,
        }
    }

    /// Returns the source recipe name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the source tag.
    pub fn tag(&self) -> &str {
        "Source"
    }

    /// Returns the source build key.
    pub fn build_key(&self) -> BuildKey {
        self.build_key
    }

    /// Returns the declared object hash.
    pub fn declared_object_hash(&self) -> ObjectHash {
        self.declared_object_hash
    }

    /// Clones the parsed origin when one was declared.
    pub fn clone_origin(&self) -> Option<Box<dyn ParsedOrigin>> {
        self.origin.clone()
    }

    /// Returns the parsed origin, when one was declared.
    pub fn origin(&self) -> Option<&dyn ParsedOrigin> {
        self.origin.as_deref()
    }

    /// Builds the per-run log subject from the runtime-allocated workspace.
    pub fn log_subject(&self, workspace: &Workspace) -> BuildLogSubject {
        BuildLogSubject::new(
            self.tag(),
            self.name(),
            self.build_key().to_string(),
            workspace.log_dir().to_path_buf(),
            workspace.raw_log_dir().to_path_buf(),
        )
    }

    /// Materializes the source into the run's temp directory and returns the
    /// staged path.
    ///
    /// This does not touch the object store: the caller looks up reuse before
    /// calling this, and imports the returned staged path afterwards.
    pub fn execute(&self, ctx: &SubjectRunContext) -> Result<PathBuf, SourceExecutionError> {
        let Some(origin) = self.origin() else {
            return Err(SourceExecutionError::build(format!(
                "source '{}' has no origin and object '{}' is not present in store",
                self.name(),
                self.declared_object_hash()
            )));
        };
        if ctx.cancellation().is_cancelled() {
            return Err(SourceExecutionError::cancelled());
        }
        let temp_root = ctx.temp_dir();
        let origin_cx = OriginContext {
            temp_root,
            logger: ctx.logger().as_ref(),
            cancellation: ctx.cancellation(),
        };
        let staged_path = match origin.materialize(&origin_cx) {
            Ok(path) => path,
            // A mid-fetch abort surfaces as an opaque error; if the run was
            // cancelled, report it as cancellation rather than a build failure.
            Err(error) if ctx.cancellation().is_cancelled() => {
                let _ = error;
                return Err(SourceExecutionError::cancelled());
            }
            Err(error) => return Err(SourceExecutionError::build(error)),
        };
        validate_origin_staged_path(&staged_path, temp_root)
            .map_err(SourceExecutionError::build)?;
        Ok(staged_path)
    }
}

fn validate_origin_staged_path(staged_path: &Path, temp_root: &Path) -> Result<(), String> {
    let canonical_temp_root = temp_root.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize source temp root '{}': {error}",
            temp_root.display()
        )
    })?;
    let canonical_staged_path = staged_path.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize source staged path '{}': {error}",
            staged_path.display()
        )
    })?;
    if !canonical_staged_path.starts_with(&canonical_temp_root) {
        return Err(format!(
            "source origin returned staged path '{}' outside temp root '{}'",
            staged_path.display(),
            temp_root.display()
        ));
    }
    Ok(())
}

/// Parses a source recipe object whose tag was already removed by the caller.
pub fn parse_source_subject(
    mut object: Map<String, Value>,
) -> Result<SourcePlannedSubject, SourceRecipeError> {
    let name = take_string(&mut object, "name")?;
    let declared_object_hash = take_string(&mut object, "object_hash")?
        .trim()
        .parse::<ObjectHash>()
        .map_err(|error| {
            SourceRecipeError::new(format!("object_hash: invalid object hash: {error}"))
        })?;
    let origin = match object.remove("origin") {
        Some(value) => Some(origins::parse_origin_value(value, "origin")?),
        None => None,
    };
    if !object.is_empty() {
        return Err(SourceRecipeError::new(format!(
            "unexpected fields: {}",
            object.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    Ok(SourcePlannedSubject::new(
        name,
        declared_object_hash,
        origin,
    ))
}

fn take_string(object: &mut Map<String, Value>, field: &str) -> Result<String, SourceRecipeError> {
    let value = object
        .remove(field)
        .ok_or_else(|| SourceRecipeError::new(format!("missing required field '{field}'")))?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| SourceRecipeError::new(format!("{field}: expected string")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::OriginSpec;
    use bobr_core::{CancellationToken, NoopBuildLogger, RuntimeProvider};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    fn source_object(origin: Option<Value>) -> Map<String, Value> {
        let mut object = json!({
            "name": "local-source",
            "object_hash": "1111111111111111111111111111111111111111111111111111111111111111"
        })
        .as_object()
        .cloned()
        .unwrap();
        if let Some(origin) = origin {
            object.insert("origin".to_string(), origin);
        }
        object
    }

    #[test]
    fn source_without_origin_is_accepted() {
        let subject = parse_source_subject(source_object(None)).unwrap();

        assert_eq!(subject.name(), "local-source");
        assert_eq!(subject.tag(), "Source");
        assert_eq!(
            subject.declared_object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(
            subject.build_key().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert!(subject.clone_origin().is_none());
    }

    #[test]
    fn source_path_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "Path",
            "path": "/tmp/source.tar",
            "unpack": true
        }))))
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "Path");
    }

    #[test]
    fn source_path_origin_requires_absolute_paths() {
        let error = parse_source_subject(source_object(Some(json!({
            "tag": "Path",
            "path": "source.tar",
            "unpack": true
        }))))
        .unwrap_err();

        assert!(error.to_string().contains("expected absolute path"));
    }

    #[test]
    fn source_http_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "Http",
            "url": "https://example.invalid/source.tar.gz",
            "unpack": true
        }))))
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "Http");
    }

    #[test]
    fn source_oci_registry_origin_is_accepted() {
        let subject = parse_source_subject(source_object(Some(json!({
            "tag": "OciRegistry",
            "image": "docker.io/library/alpine:3.20",
            "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "platform": {
                "os": "linux",
                "architecture": "amd64"
            }
        }))))
        .unwrap();

        assert_eq!(subject.clone_origin().unwrap().spec().tag, "OciRegistry");
    }

    #[test]
    fn source_object_hash_allows_trailing_whitespace() {
        let mut object = source_object(None);
        object.insert(
            "object_hash".to_string(),
            Value::String(
                "1111111111111111111111111111111111111111111111111111111111111111\n".to_string(),
            ),
        );

        let subject = parse_source_subject(object).unwrap();

        assert_eq!(
            subject.declared_object_hash().to_string(),
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[derive(Debug, Clone)]
    struct StagingOrigin {
        target: PathBuf,
    }

    #[derive(Debug, Clone)]
    struct RecordingOrigin {
        called: Arc<AtomicBool>,
    }

    impl ParsedOrigin for StagingOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec { tag: "Stub" };
            &SPEC
        }
        fn materialize(&self, _cx: &OriginContext<'_>) -> Result<PathBuf, String> {
            if let Some(parent) = self.target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&self.target, b"payload").map_err(|error| error.to_string())?;
            Ok(self.target.clone())
        }
        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    impl ParsedOrigin for RecordingOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec { tag: "Recording" };
            &SPEC
        }
        fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String> {
            self.called.store(true, Ordering::SeqCst);
            Ok(cx.temp_root.join("staged"))
        }
        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    fn sample_hash() -> ObjectHash {
        "1111111111111111111111111111111111111111111111111111111111111111"
            .parse()
            .unwrap()
    }

    fn run_ctx(temp_root: &Path) -> SubjectRunContext {
        run_ctx_with_cancellation(temp_root, CancellationToken::new())
    }

    fn run_ctx_with_cancellation(
        temp_root: &Path,
        cancellation: CancellationToken,
    ) -> SubjectRunContext {
        let workspace = Workspace::new(
            temp_root.join("log"),
            temp_root.join("log/raw"),
            temp_root.to_path_buf(),
        );
        SubjectRunContext::new(
            workspace,
            Arc::new(NoopBuildLogger),
            cancellation,
            RuntimeProvider::host(),
        )
    }

    #[test]
    fn execute_without_origin_is_an_error() {
        let subject = SourcePlannedSubject::new("src".to_string(), sample_hash(), None);
        let temp = tempdir().unwrap();
        let error = subject.execute(&run_ctx(temp.path())).unwrap_err();
        assert!(error.to_string().contains("has no origin"), "{error}");
    }

    #[test]
    fn execute_stages_under_temp_root() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("staged");
        let subject = SourcePlannedSubject::new(
            "src".to_string(),
            sample_hash(),
            Some(Box::new(StagingOrigin {
                target: target.clone(),
            })),
        );
        let staged = subject.execute(&run_ctx(temp.path())).unwrap();
        assert_eq!(staged, target);
        assert!(staged.is_file());
    }

    #[test]
    fn execute_does_not_materialize_when_cancelled() {
        let temp = tempdir().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let called = Arc::new(AtomicBool::new(false));
        let subject = SourcePlannedSubject::new(
            "src".to_string(),
            sample_hash(),
            Some(Box::new(RecordingOrigin {
                called: called.clone(),
            })),
        );

        let error = subject
            .execute(&run_ctx_with_cancellation(temp.path(), cancellation))
            .unwrap_err();

        assert!(matches!(error, SourceExecutionError::Cancelled(_)));
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn execute_rejects_staged_path_outside_temp_root() {
        let temp = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let subject = SourcePlannedSubject::new(
            "src".to_string(),
            sample_hash(),
            Some(Box::new(StagingOrigin {
                target: outside.path().join("escaped"),
            })),
        );
        let error = subject.execute(&run_ctx(temp.path())).unwrap_err();
        assert!(error.to_string().contains("outside temp root"), "{error}");
    }
}
