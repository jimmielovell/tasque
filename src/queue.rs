use crate::manager::TasqManager;
use crate::Metrics;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::Ordering;
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
    pub fn promote(&self) -> Self {
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

impl<T: Send + 'static> PriorityQueue<T> {
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

impl<T: Send + 'static> PriorityQueue<T> {
    pub fn get_queue_metrics(&self) -> (usize, usize, usize, [Option<Instant>; 3]) {
        let mut counts = [0, 0, 0];
        let mut oldest = [None, None, None];

        // Count ready tasks
        for tasq_manager in &self.ready_tasqs {
            match tasq_manager.current_priority {
                TasqPriority::High => counts[0] += 1,
                TasqPriority::Medium => counts[1] += 1,
                TasqPriority::Low => counts[2] += 1,
            }

            // Track oldest task per priority
            let idx = tasq_manager.current_priority as usize;
            if oldest[idx].is_none() || oldest[idx].map_or(true, |t| tasq_manager.created_at < t) {
                oldest[idx] = Some(tasq_manager.created_at);
            }
        }

        (counts[0], counts[1], counts[2], oldest)
    }

    pub fn try_promote_tasks(&mut self, metrics: &Metrics) -> usize {
        let hp_pressure = metrics.get_priority_pressure();
        let mp_stress = metrics.get_stress_ratio(TasqPriority::Medium);
        let lp_stress = metrics.get_stress_ratio(TasqPriority::Low);

        // Calculate dynamic promotion batch size based on queue pressure
        let hp_size = metrics
            .high_priority_stats
            .current_queue_size
            .load(Ordering::Relaxed);
        let promotion_batch_size = if hp_pressure > 1.0 {
            // More conservative when HP queue is under pressure
            1
        } else {
            // More aggressive when HP queue is healthy
            // Use at most 10% of waiting tasks, minimum 1
            (hp_size / 10).max(1)
        };

        // Check medium priority first
        if mp_stress > (1.0 + hp_pressure) {
            let promoted = self.promote_batch(TasqPriority::Medium, promotion_batch_size);
            if promoted > 0 {
                metrics.record_promotion(TasqPriority::Medium, promoted);
                return promoted;
            }
        }

        // Then check low priority with a higher threshold
        if lp_stress > (2.0 + hp_pressure) {
            let promoted = self.promote_batch(TasqPriority::Low, promotion_batch_size);
            if promoted > 0 {
                metrics.record_promotion(TasqPriority::Low, promoted);
                return promoted;
            }
        }

        0
    }

    fn promote_batch(&mut self, from: TasqPriority, count: usize) -> usize {
        let mut promoted = 0;
        let mut new_ready_queue = BinaryHeap::new();

        let tasq_managers: Vec<_> = self.ready_tasqs.drain().collect();
        let (to_promote, other_tasq_managers): (Vec<_>, Vec<_>) = tasq_managers
            .into_iter()
            .partition(|tm| promoted < count && tm.current_priority == from);

        // Promote the selected tasks
        for mut tasq_manager in to_promote {
            // Update priority
            match from {
                TasqPriority::Medium => tasq_manager.current_priority = TasqPriority::High,
                TasqPriority::Low => tasq_manager.current_priority = TasqPriority::Medium,
                _ => {}
            }
            new_ready_queue.push(tasq_manager);
            promoted += 1;
        }

        // Add remaining tasks
        new_ready_queue.extend(other_tasq_managers);

        self.ready_tasqs = new_ready_queue;
        promoted
    }
}
