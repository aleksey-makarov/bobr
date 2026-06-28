use bobr_core::{BuildLogEvent, BuildLogLevel, BuildLogger, BuildStatus, CancellationToken};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

/// Static descriptor for one origin kind, naming the recipe `tag` it handles
/// (e.g. `"Http"`, `"Oci"`, `"Path"`). Both [`OriginHandler`] and
/// [`ParsedOrigin`] report the spec they belong to via `spec()`.
#[derive(Debug, Clone, Copy)]
pub struct OriginSpec {
    /// The origin's recipe tag, matched against the `tag` field of a source's
    /// `origin` object.
    pub tag: &'static str,
}

/// What an origin handler is given to materialize a source: the staging
/// directory plus a logger and a cancellation token, so long fetches can report
/// progress and stop promptly when the run is cancelled.
#[derive(Clone, Copy)]
pub struct OriginContext<'a> {
    /// Staging directory the origin must materialize into; the runtime only
    /// cleans up this path (see [`ParsedOrigin::materialize`]).
    pub temp_root: &'a Path,
    /// Subject logger for milestones ([`OriginContext::milestone`]) and progress
    /// ticks ([`OriginContext::progress`]).
    pub logger: &'a dyn BuildLogger,
    /// Run cancellation token; long fetches poll [`OriginContext::is_cancelled`].
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

/// A parsed, ready-to-run source origin.
///
/// Produced by an [`OriginHandler`] from a recipe's `origin` object; it knows
/// how to fetch and stage its content via
/// [`materialize`](ParsedOrigin::materialize). Boxed trait objects are cloneable
/// (see the `Clone` impl below) so a planned source can be duplicated cheaply.
pub trait ParsedOrigin: fmt::Debug + Send + Sync {
    /// The [`OriginSpec`] (kind/tag) this origin belongs to.
    fn spec(&self) -> &'static OriginSpec;

    /// Materializes the origin and returns the staged path.
    ///
    /// The returned path MUST be located inside `cx.temp_root`. The runtime
    /// only cleans up `temp_root` (including on error), so any staged data left
    /// outside it would leak. Implementations should stage everything under
    /// `cx.temp_root` and return a path within it.
    fn materialize(&self, cx: &OriginContext<'_>) -> Result<PathBuf, String>;

    /// Clones into a fresh boxed trait object. Backs the `Clone` impl for
    /// `Box<dyn ParsedOrigin>`.
    fn clone_box(&self) -> Box<dyn ParsedOrigin>;
}

impl Clone for Box<dyn ParsedOrigin> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Parses a recipe `origin` object of one kind into a [`ParsedOrigin`].
///
/// There is one handler per origin kind (HTTP, OCI registry, path, …); the
/// source layer dispatches to the handler whose [`OriginSpec::tag`] matches the
/// `tag` field of the recipe's `origin` object.
pub trait OriginHandler: Send + Sync {
    /// The [`OriginSpec`] (kind/tag) this handler parses.
    fn spec(&self) -> &'static OriginSpec;

    /// Parses `object` (a source's `origin` map) into a [`ParsedOrigin`].
    ///
    /// `field_path` is the JSON-path prefix of `object`, used to build readable
    /// error messages. Returns a human-readable error string on invalid input.
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
