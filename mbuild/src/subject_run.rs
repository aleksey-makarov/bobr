use mbuild_core::{BuildLogSubject, ObjectHash, ParsedOrigin, Workspace};
use std::path::Path;

/// Concrete per-run object for a normal builder node.
#[derive(Debug, Clone)]
pub(crate) struct BuilderRun {
    tag: String,
    recipe_name: Option<String>,
    build_key: String,
    workspace: Workspace,
}

impl BuilderRun {
    /// Creates a per-run builder object from its identity and workspace.
    pub(crate) fn new(
        tag: impl Into<String>,
        recipe_name: Option<String>,
        build_key: impl Into<String>,
        workspace: Workspace,
    ) -> Self {
        Self {
            tag: tag.into(),
            recipe_name,
            build_key: build_key.into(),
            workspace,
        }
    }

    /// Returns the builder tag for this run.
    pub(crate) fn tag(&self) -> &str {
        &self.tag
    }

    /// Returns the recipe name associated with this run, when there is one.
    pub(crate) fn recipe_name(&self) -> Option<&str> {
        self.recipe_name.as_deref()
    }

    /// Returns the full build key string associated with this run.
    pub(crate) fn build_key(&self) -> &str {
        &self.build_key
    }

    /// Returns the per-run log directory.
    pub(crate) fn log_dir(&self) -> &Path {
        self.workspace.log_dir()
    }

    /// Returns the per-run raw log directory.
    pub(crate) fn raw_log_dir(&self) -> &Path {
        self.workspace.raw_log_dir()
    }

    /// Returns the per-run temporary directory.
    pub(crate) fn temp_dir(&self) -> &Path {
        self.workspace.temp_dir()
    }

    pub(crate) fn log_subject(&self) -> BuildLogSubject {
        BuildLogSubject::new(
            self.tag(),
            self.recipe_name().unwrap_or(""),
            self.build_key(),
            self.log_dir().to_path_buf(),
            self.raw_log_dir().to_path_buf(),
        )
    }
}

/// Concrete per-run object for a source node.
#[derive(Debug, Clone)]
pub(crate) struct SourceBuilder {
    run: BuilderRun,
    declared_object_hash: ObjectHash,
    origin: Option<Box<dyn ParsedOrigin>>,
}

impl SourceBuilder {
    /// Creates a per-run source builder object.
    pub(crate) fn new(
        recipe_name: String,
        build_key: impl Into<String>,
        declared_object_hash: ObjectHash,
        origin: Option<Box<dyn ParsedOrigin>>,
        workspace: Workspace,
    ) -> Self {
        Self {
            run: BuilderRun::new("Source", Some(recipe_name), build_key, workspace),
            declared_object_hash,
            origin,
        }
    }

    /// Returns the recipe name associated with this source.
    pub(crate) fn recipe_name(&self) -> &str {
        self.run
            .recipe_name()
            .expect("source builders are always created with a recipe name")
    }

    /// Returns the source builder tag.
    pub(crate) fn tag(&self) -> &str {
        self.run.tag()
    }

    /// Returns the full build key string associated with this source.
    pub(crate) fn build_key(&self) -> &str {
        self.run.build_key()
    }

    /// Returns the declared object hash for this source.
    pub(crate) fn declared_object_hash(&self) -> ObjectHash {
        self.declared_object_hash
    }

    /// Returns the parsed origin, when one was declared.
    pub(crate) fn origin(&self) -> Option<&dyn ParsedOrigin> {
        self.origin.as_deref()
    }

    /// Returns the per-run log directory.
    pub(crate) fn log_dir(&self) -> &Path {
        self.run.log_dir()
    }

    /// Returns the per-run raw log directory.
    pub(crate) fn raw_log_dir(&self) -> &Path {
        self.run.raw_log_dir()
    }

    /// Returns the per-run temporary directory.
    pub(crate) fn temp_dir(&self) -> &Path {
        self.run.temp_dir()
    }

    pub(crate) fn log_subject(&self) -> BuildLogSubject {
        BuildLogSubject::new(
            self.tag(),
            self.recipe_name(),
            self.build_key(),
            self.log_dir().to_path_buf(),
            self.raw_log_dir().to_path_buf(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builder_run_object_exposes_runtime_allocated_state() {
        let workspace = Workspace::new(
            PathBuf::from("/tmp/dummy/log"),
            PathBuf::from("/tmp/dummy/log/raw"),
            PathBuf::from("/tmp/dummy/tmp"),
        );

        let run = BuilderRun::new(
            "Dummy",
            Some("demo".to_string()),
            "build-key",
            workspace.clone(),
        );

        assert_eq!(run.tag(), "Dummy");
        assert_eq!(run.recipe_name(), Some("demo"));
        assert_eq!(run.build_key(), "build-key");
        assert_eq!(run.log_dir(), workspace.log_dir());
        assert_eq!(run.raw_log_dir(), workspace.raw_log_dir());
        assert_eq!(run.temp_dir(), workspace.temp_dir());
    }

    #[test]
    fn source_builder_object_exposes_runtime_allocated_state() {
        let object_hash = "0000000000000000000000000000000000000000000000000000000000000000"
            .parse::<ObjectHash>()
            .unwrap();
        let workspace = Workspace::new(
            PathBuf::from("/tmp/source/log"),
            PathBuf::from("/tmp/source/log/raw"),
            PathBuf::from("/tmp/source/tmp"),
        );

        let source = SourceBuilder::new(
            "source-demo".to_string(),
            "source-key",
            object_hash,
            None,
            workspace.clone(),
        );

        assert_eq!(source.tag(), "Source");
        assert_eq!(source.recipe_name(), "source-demo");
        assert_eq!(source.build_key(), "source-key");
        assert_eq!(source.declared_object_hash(), object_hash);
        assert!(source.origin().is_none());
        assert_eq!(source.log_dir(), workspace.log_dir());
        assert_eq!(source.raw_log_dir(), workspace.raw_log_dir());
        assert_eq!(source.temp_dir(), workspace.temp_dir());
    }
}
