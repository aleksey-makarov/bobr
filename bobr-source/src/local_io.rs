//! Shared scheduling for blocking local filesystem and namespace work.

use bobr_core::CancellationToken;
use bobr_store::StoreError;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Shared bound for blocking local-I/O work in one realization run.
///
/// Waiting for a slot is cancellation-aware. Once a synchronous operation has
/// started it is allowed to reach its atomic publication boundary.
#[derive(Debug, Clone)]
pub struct LocalIoScheduler {
    semaphore: Arc<Semaphore>,
    cancellation: CancellationToken,
}

impl LocalIoScheduler {
    /// Creates a scheduler with at least one local-I/O slot.
    pub fn new(max_jobs: usize, cancellation: CancellationToken) -> Result<Self, StoreError> {
        if max_jobs == 0 {
            return Err(StoreError::InvalidInput(
                "local-I/O max_jobs must be greater than zero".to_string(),
            ));
        }
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(max_jobs)),
            cancellation,
        })
    }

    /// Waits for one local-I/O slot without starting work after cancellation.
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, StoreError> {
        let permit = self.semaphore.clone().acquire_owned();
        tokio::pin!(permit);
        loop {
            if self.cancellation.is_cancelled() {
                return Err(cancelled_error());
            }
            tokio::select! {
                permit = &mut permit => {
                    let permit = permit.expect("local-I/O semaphore is never closed");
                    if self.cancellation.is_cancelled() {
                        return Err(cancelled_error());
                    }
                    return Ok(permit);
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    /// Runs one synchronous store operation on Tokio's blocking pool.
    pub async fn run<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    {
        let permit = self.acquire().await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation()
        })
        .await
        .map_err(|error| StoreError::Io(format!("local-I/O task panicked: {error}")))?
    }
}

fn cancelled_error() -> StoreError {
    StoreError::Io("local-I/O operation cancelled".to_string())
}
