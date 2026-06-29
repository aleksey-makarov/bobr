use crate::cancellation::CancellationToken;
use crate::logging::BuildLogger;
use crate::workspace::Workspace;
use bobr_runtime::runtime_provider::RuntimeProvider;
use std::path::Path;
use std::sync::Arc;

/// Store-free per-run context handed to a planned subject's `execute`.
///
/// The caller (the runtime) allocates the workspace, binds the per-run logger,
/// prepares the temp directory, and selects the runtime provider before
/// constructing this value. A planned subject only reads it to build its
/// artifact under `workspace.temp_dir()`; it never touches the object store.
pub struct SubjectRunContext {
    workspace: Workspace,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
    runtime: RuntimeProvider,
}

impl std::fmt::Debug for SubjectRunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubjectRunContext")
            .field("cancellation", &self.cancellation)
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl SubjectRunContext {
    /// Creates a run context from already-allocated runtime state.
    pub fn new(
        workspace: Workspace,
        logger: Arc<dyn BuildLogger>,
        cancellation: CancellationToken,
        runtime: RuntimeProvider,
    ) -> Self {
        Self {
            workspace,
            logger,
            cancellation,
            runtime,
        }
    }

    /// Returns the per-run workspace paths.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Convenience accessor for the per-run temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self.workspace.temp_dir()
    }

    /// Returns the per-run logger.
    pub fn logger(&self) -> &Arc<dyn BuildLogger> {
        &self.logger
    }

    /// Returns the cancellation token for this run.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Returns the runtime provider selected for this subject execution.
    pub fn runtime(&self) -> &RuntimeProvider {
        &self.runtime
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopBuildLogger;
    use bobr_runtime::runtime_provider::{RuntimeBackend, RuntimeProvider};
    use std::path::PathBuf;

    #[test]
    fn subject_run_context_preserves_runtime_provider() {
        let workspace = Workspace::new(
            PathBuf::from("/tmp/log"),
            PathBuf::from("/tmp/raw"),
            PathBuf::from("/tmp/tmp"),
        );
        let context = SubjectRunContext::new(
            workspace,
            Arc::new(NoopBuildLogger),
            CancellationToken::new(),
            RuntimeProvider::namespace(),
        );

        assert_eq!(context.runtime().backend(), RuntimeBackend::Namespace);
    }
}
