use bobr_core::{BuildLogEvent, BuildLogLevel, BuildLogger, BuildStatus, CancellationToken};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct OriginSpec {
    pub tag: &'static str,
}

/// What an origin handler is given to materialize a source: the staging
/// directory plus a logger and a cancellation token, so long fetches can report
/// progress and stop promptly when the run is cancelled.
#[derive(Clone, Copy)]
pub struct OriginContext<'a> {
    pub temp_root: &'a Path,
    pub logger: &'a dyn BuildLogger,
    pub cancellation: &'a CancellationToken,
}

impl OriginContext<'_> {
    /// Emits a durable milestone (`Info`): persisted to the event log and shown
    /// on screen. Use for "fetching X" / "fetched N bytes" / errors.
    pub fn milestone(&self, message: impl Into<String>) {
        self.event(BuildLogLevel::Info, message);
    }

    /// Emits a transient progress tick (`Progress`): shown on screen only, never
    /// persisted. Producers should throttle these (e.g. once per second).
    pub fn progress(&self, message: impl Into<String>) {
        self.event(BuildLogLevel::Progress, message);
    }

    fn event(&self, level: BuildLogLevel, message: impl Into<String>) {
        self.logger.log_event(BuildLogEvent {
            level,
            status: BuildStatus::Running,
            op: Some("fetch".to_string()),
            message: message.into(),
            object_hash: None,
            raw_log_path: None,
            details: Map::new(),
        });
    }

    /// Whether the run has been cancelled. Long fetches poll this and stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

pub trait ParsedOrigin: fmt::Debug + Send + Sync {
    fn spec(&self) -> &'static OriginSpec;

    /// Materializes the origin and returns the staged path.
    ///
    /// The returned path MUST be located inside `cx.temp_root`. The runtime
    /// only cleans up `temp_root` (including on error), so any staged data left
    /// outside it would leak. Implementations should stage everything under
    /// `cx.temp_root` and return a path within it.
    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String>;

    fn clone_box(&self) -> Box<dyn ParsedOrigin>;
}

impl Clone for Box<dyn ParsedOrigin> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

pub trait OriginHandler: Send + Sync {
    fn spec(&self) -> &'static OriginSpec;

    fn parse(
        &self,
        object: Map<String, Value>,
        field_path: &str,
    ) -> Result<Box<dyn ParsedOrigin>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct DummyOrigin;

    impl ParsedOrigin for DummyOrigin {
        fn spec(&self) -> &'static OriginSpec {
            static SPEC: OriginSpec = OriginSpec { tag: "dummy" };
            &SPEC
        }

        fn materialize(&self, _cx: &OriginContext<'_>) -> Result<PathBuf, String> {
            Ok(PathBuf::from("/tmp/dummy"))
        }

        fn clone_box(&self) -> Box<dyn ParsedOrigin> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn boxed_parsed_origin_is_cloneable() {
        let origin: Box<dyn ParsedOrigin> = Box::new(DummyOrigin);
        let cloned = origin.clone();
        assert_eq!(origin.spec().tag, cloned.spec().tag);
    }
}
