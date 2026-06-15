use crate::cancellation::CancellationToken;
use crate::logging::BuildLogger;
use crate::workspace::Workspace;
use std::path::Path;
use std::sync::Arc;

/// Store-free per-run context handed to a planned subject's `execute`.
///
/// The caller (the runtime) allocates the workspace, binds the per-run logger,
/// and prepares the temp directory before constructing this value. A planned
/// subject only reads it to build its artifact under `workspace.temp_dir()`;
/// it never touches the object store.
pub struct SubjectRunContext {
    workspace: Workspace,
    logger: Arc<dyn BuildLogger>,
    cancellation: CancellationToken,
}

impl SubjectRunContext {
    /// Creates a run context from already-allocated runtime state.
    pub fn new(
        workspace: Workspace,
        logger: Arc<dyn BuildLogger>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            workspace,
            logger,
            cancellation,
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
}
