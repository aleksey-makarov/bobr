use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A shared, cloneable cancellation flag. Clones share one flag, so cancelling
/// any handle signals all of them; long-running work polls
/// [`is_cancelled`](CancellationToken::is_cancelled) to stop early.
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Creates a fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Marks the token (and all its clones) cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Returns whether the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
