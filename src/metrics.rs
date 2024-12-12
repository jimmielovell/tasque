use std::sync::atomic::AtomicUsize;

/// Task metrics tracking
#[derive(Debug, Default)]
pub struct Metrics {
    pub total_processed: AtomicUsize,
    pub total_failed: AtomicUsize,
    pub tasks_in_progress: AtomicUsize,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }
}
