use crate::manager::TasqManager;
use std::collections::{BTreeMap, BinaryHeap};
use std::time::Instant;

/// Represents the priority of a `tasq` in the queue.
///
/// Tasqs can be assigned `High`, `Medium`, or `Low` priority.
#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug, PartialOrd, Ord)]
pub enum TasqPriority {
    High = 2,
    Medium = 1,
    Low = 0,
}

impl TasqPriority {
    /// Upgrade the tasq's priority.
    ///
    /// # Returns
    /// The next higher priority. `Tasq`s with `High` priority remain unchanged.
    pub fn upgrade(&self) -> Self {
        match self {
            TasqPriority::Low => TasqPriority::Medium,
            TasqPriority::Medium => TasqPriority::High,
            TasqPriority::High => TasqPriority::High,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueFullError;

/// Priority queue implementation with separate ready and delayed tasqs
#[derive(Debug)]
pub struct PriorityQueue<T: Send + 'static> {
    ready_tasqs: BinaryHeap<TasqManager<T>>,
    delayed_tasqs: BTreeMap<Instant, Vec<TasqManager<T>>>,
    capacity: usize,
}

impl<T> PriorityQueue<T>
where
    T: Send + 'static,
{
    pub fn new(capacity: usize) -> Self {
        Self {
            ready_tasqs: BinaryHeap::new(),
            delayed_tasqs: BTreeMap::new(),
            capacity,
        }
    }

    pub fn push(&mut self, tasq_manager: TasqManager<T>) -> Result<(), QueueFullError> {
        if self.len() >= self.capacity {
            return Err(QueueFullError);
        }

        let now = Instant::now();
        if tasq_manager.next_run <= now {
            self.ready_tasqs.push(tasq_manager);
        } else {
            self.delayed_tasqs
                .entry(tasq_manager.next_run)
                .or_default()
                .push(tasq_manager);
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Option<TasqManager<T>> {
        self.move_ready_tasqs();
        self.ready_tasqs.pop()
    }

    pub fn len(&self) -> usize {
        self.ready_tasqs.len() + self.delayed_tasqs.values().map(|v| v.len()).sum::<usize>()
    }

    fn move_ready_tasqs(&mut self) {
        let now = Instant::now();
        let ready_times: Vec<_> = self.delayed_tasqs.range(..=now).map(|(k, _)| *k).collect();

        for time in ready_times {
            if let Some(tasqs) = self.delayed_tasqs.remove(&time) {
                for tasq in tasqs {
                    self.ready_tasqs.push(tasq);
                }
            }
        }
    }

    pub fn next_ready_time(&self) -> Option<Instant> {
        if !self.ready_tasqs.is_empty() {
            return Some(Instant::now());
        }
        self.delayed_tasqs.keys().next().copied()
    }
}
