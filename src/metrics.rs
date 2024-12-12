use crate::TasqPriority;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct PriorityStats {
    pub avg_wait_time: AtomicU64, // in nanoseconds
    pub avg_exec_time: AtomicU64, // in nanoseconds
    pub last_queue_size: AtomicUsize,
    pub current_queue_size: AtomicUsize,
    pub last_update_time: AtomicU64, // in nanoseconds since UNIX_EPOCH
}

impl PriorityStats {
    fn new() -> Self {
        Self {
            avg_wait_time: AtomicU64::new(0),
            avg_exec_time: AtomicU64::new(0),
            last_queue_size: AtomicUsize::new(0),
            current_queue_size: AtomicUsize::new(0),
            last_update_time: AtomicU64::new(Instant::now().elapsed().as_nanos() as u64),
        }
    }

    fn update_wait_time(&self, wait_time: Duration) {
        // Simple exponential moving average with alpha = 0.2
        const ALPHA: f64 = 0.2;
        let new_time = wait_time.as_nanos() as f64;
        let old_avg = self.avg_wait_time.load(Ordering::Relaxed) as f64;
        let new_avg = (ALPHA * new_time + (1.0 - ALPHA) * old_avg) as u64;
        self.avg_wait_time.store(new_avg, Ordering::Relaxed);
    }

    fn update_exec_time(&self, exec_time: Duration) {
        const ALPHA: f64 = 0.2;
        let new_time = exec_time.as_nanos() as f64;
        let old_avg = self.avg_exec_time.load(Ordering::Relaxed) as f64;
        let new_avg = (ALPHA * new_time + (1.0 - ALPHA) * old_avg) as u64;
        self.avg_exec_time.store(new_avg, Ordering::Relaxed);
    }

    fn update_queue_size(&self, new_size: usize) {
        let old_size = self.current_queue_size.load(Ordering::Relaxed);
        self.last_queue_size.store(old_size, Ordering::Relaxed);
        self.current_queue_size.store(new_size, Ordering::Relaxed);
        self.last_update_time.store(
            Instant::now().elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
    }

    fn get_queue_growth_rate(&self) -> f64 {
        let current = self.current_queue_size.load(Ordering::Relaxed) as f64;
        let last = self.last_queue_size.load(Ordering::Relaxed) as f64;

        if last == 0.0 {
            return 0.0;
        }

        (current - last) / last
    }

    fn get_avg_wait_time(&self) -> Duration {
        Duration::from_nanos(self.avg_wait_time.load(Ordering::Relaxed))
    }

    fn get_avg_exec_time(&self) -> Duration {
        Duration::from_nanos(self.avg_exec_time.load(Ordering::Relaxed))
    }
}

#[derive(Debug)]
pub struct Metrics {
    // Existing metrics
    pub total_processed: AtomicUsize,
    pub total_failed: AtomicUsize,
    pub tasks_in_progress: AtomicUsize,

    // Promotion tracking
    pub total_promotions: AtomicUsize,
    pub medium_to_high_promotions: AtomicUsize,
    pub low_to_medium_promotions: AtomicUsize,

    // New priority stats
    pub high_priority_stats: PriorityStats,
    pub medium_priority_stats: PriorityStats,
    pub low_priority_stats: PriorityStats,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            total_processed: AtomicUsize::new(0),
            total_failed: AtomicUsize::new(0),
            tasks_in_progress: AtomicUsize::new(0),
            total_promotions: AtomicUsize::new(0),
            medium_to_high_promotions: AtomicUsize::new(0),
            low_to_medium_promotions: AtomicUsize::new(0),
            high_priority_stats: PriorityStats::new(),
            medium_priority_stats: PriorityStats::new(),
            low_priority_stats: PriorityStats::new(),
        }
    }

    pub fn get_priority_pressure(&self) -> f64 {
        let growth_rate = self.high_priority_stats.get_queue_growth_rate();
        let exec_time = self.high_priority_stats.get_avg_exec_time();
        growth_rate * exec_time.as_secs_f64()
    }

    pub fn get_stress_ratio(&self, priority: TasqPriority) -> f64 {
        let hp_wait_time = self.high_priority_stats.get_avg_wait_time().as_secs_f64();
        if hp_wait_time == 0.0 {
            return 0.0;
        }

        let priority_wait_time = match priority {
            TasqPriority::Medium => self.medium_priority_stats.get_avg_wait_time().as_secs_f64(),
            TasqPriority::Low => self.low_priority_stats.get_avg_wait_time().as_secs_f64(),
            _ => hp_wait_time,
        };

        priority_wait_time / hp_wait_time
    }

    pub fn record_task_stats(
        &self,
        priority: TasqPriority,
        wait_time: Duration,
        exec_time: Duration,
    ) {
        match priority {
            TasqPriority::High => {
                self.high_priority_stats.update_wait_time(wait_time);
                self.high_priority_stats.update_exec_time(exec_time);
            }
            TasqPriority::Medium => {
                self.medium_priority_stats.update_wait_time(wait_time);
                self.medium_priority_stats.update_exec_time(exec_time);
            }
            TasqPriority::Low => {
                self.low_priority_stats.update_wait_time(wait_time);
                self.low_priority_stats.update_exec_time(exec_time);
            }
        }
    }

    pub fn update_queue_lengths(&self, high: usize, medium: usize, low: usize) {
        self.high_priority_stats.update_queue_size(high);
        self.medium_priority_stats.update_queue_size(medium);
        self.low_priority_stats.update_queue_size(low);
    }

    pub fn record_promotion(&self, from: TasqPriority, count: usize) {
        self.total_promotions.fetch_add(count, Ordering::Relaxed);
        match from {
            TasqPriority::Medium => {
                self.medium_to_high_promotions
                    .fetch_add(count, Ordering::Relaxed);
            }
            TasqPriority::Low => {
                self.low_to_medium_promotions
                    .fetch_add(count, Ordering::Relaxed);
            }
            _ => {}
        };
    }
}
