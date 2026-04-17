use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Shared token passed to long-running operations so callers can track
/// progress and request cancellation.
///
/// All methods use `Ordering::Relaxed` — cross-thread visibility is
/// guaranteed eventually; we don't need sequential consistency for a
/// progress bar.
pub struct ProgressToken {
    pub done:      AtomicUsize,
    pub total:     AtomicUsize,
    pub cancelled: AtomicBool,
}

impl ProgressToken {
    /// Create a new token with a known total item count.
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            done:      AtomicUsize::new(0),
            total:     AtomicUsize::new(total),
            cancelled: AtomicBool::new(false),
        })
    }

    /// Update the total (e.g. once scanning is complete).
    pub fn set_total(&self, n: usize) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// Increment the done counter by 1.
    pub fn advance(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Request cancellation.  The operation will stop at the next
    /// batch boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Returns `true` if `cancel()` was called.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Snapshot `(done, total)` atomically (both reads are relaxed, but
    /// that's fine for a progress bar).
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    /// Convenience: fraction in [0.0, 1.0] for progress bars.
    pub fn fraction(&self) -> f32 {
        let (done, total) = self.snapshot();
        if total == 0 { 0.0 } else { (done as f32 / total as f32).min(1.0) }
    }
}
