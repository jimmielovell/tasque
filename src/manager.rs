use crate::queue::TasqPriority;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct TasqManager<T: Send + 'static> {
    pub tasq: T,
    pub max_retries: u32,
    pub retry_count: u32,
    pub created_at: Instant,
    pub next_run: Instant,
    pub priority: TasqPriority,
    pub current_priority: TasqPriority,
}

impl<T> Ord for TasqManager<T>
where
    T: Send + 'static,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Order by priority first, then by next run time
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.next_run.cmp(&self.next_run))
    }
}

impl<T> PartialOrd for TasqManager<T>
where
    T: Send + 'static,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> PartialEq for TasqManager<T>
where
    T: Send + 'static,
{
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.next_run == other.next_run
    }
}

impl<T> Eq for TasqManager<T> where T: Send + 'static {}
