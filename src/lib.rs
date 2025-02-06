mod metrics;
mod queue;

mod manager;
mod tasq;

use crate::queue::PriorityQueue;

use crate::manager::TasqManager;
pub use metrics::Metrics;
pub use queue::{QueueFullError, TasqPriority};
use rand::Rng;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
pub use tasq::Tasq;
use tokio::sync::{broadcast, watch, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::sleep_until;

const MAX_FAILED_TASK_HISTORY: usize = 1_000;
const DEFAULT_WORKER_COUNT: usize = 4;
const DEFAULT_QUEUE_CAPACITY: usize = 10_000;
const DEFAULT_TIMEOUT: u64 = 30;
const DEFAULT_MAX_RETRY_DELAY: u64 = 300;
const DEFAULT_AGING_DURATION: u64 = 300;

#[derive(Clone)]
pub struct Tasque<T>
where
    T: Send + Sync + 'static,
{
    queue: Arc<Mutex<PriorityQueue<T>>>,
    pub metrics: Arc<Metrics>,
    pub failed_tasqs: Arc<Mutex<VecDeque<TasqManager<T>>>>,
    timeout: Duration,
    max_retry_delay: Duration,
    aging_duration: Duration,
    worker_count: usize,
    semaphore: Arc<Semaphore>,
    shutdown: broadcast::Sender<()>,
    task_signal: (watch::Sender<bool>, watch::Receiver<bool>),
}

impl<T> Default for Tasque<T>
where
    T: Tasq + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new(None, None, None, None, None)
    }
}

impl<T> Tasque<T>
where
    T: Tasq + Send + Sync + 'static,
{
    pub fn new(
        timeout: Option<Duration>,
        max_retry_delay: Option<Duration>,
        aging_duration: Option<Duration>,
        worker_count: Option<usize>,
        queue_capacity: Option<usize>,
    ) -> Self {
        let timeout = timeout.unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT));
        let max_retry_delay =
            max_retry_delay.unwrap_or(Duration::from_secs(DEFAULT_MAX_RETRY_DELAY));
        let aging_duration = aging_duration.unwrap_or(Duration::from_secs(DEFAULT_AGING_DURATION));
        let worker_count = worker_count.unwrap_or(DEFAULT_WORKER_COUNT);
        let queue_capacity = queue_capacity.unwrap_or(DEFAULT_QUEUE_CAPACITY);
        let (shutdown_tx, _) = broadcast::channel(1);
        let (task_tx, task_rx) = watch::channel(false);

        Self {
            queue: Arc::new(Mutex::new(PriorityQueue::new(queue_capacity))),
            metrics: Arc::new(Metrics::new()),
            failed_tasqs: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_FAILED_TASK_HISTORY))),
            timeout,
            max_retry_delay,
            aging_duration,
            worker_count,
            semaphore: Arc::new(Semaphore::new(worker_count)),
            shutdown: shutdown_tx,
            task_signal: (task_tx, task_rx),
        }
    }

    pub async fn add(
        &self,
        tasq: T,
        priority: TasqPriority,
        max_retries: u32,
        next_run_at: Option<Instant>,
    ) -> Result<(), QueueFullError> {
        let tasq_manager = TasqManager {
            tasq,
            max_retries,
            retry_count: 0,
            created_at: Instant::now(),
            next_run: next_run_at.unwrap_or(Instant::now()),
            priority,
            current_priority: priority,
        };

        let mut queue = self.queue.lock().await;
        queue.push(tasq_manager)?;
        drop(queue);

        let _ = self.task_signal.0.send_if_modified(|_| true);
        Ok(())
    }

    pub fn run(&self, arg: T::A) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::with_capacity(self.worker_count);
        let arg = Arc::new(arg);

        for _ in 0..self.worker_count {
            let handle = spawn_worker(
                Arc::clone(&self.queue),
                Arc::clone(&self.metrics),
                Arc::clone(&self.semaphore),
                Arc::clone(&self.failed_tasqs),
                self.shutdown.clone().subscribe(),
                self.task_signal.1.clone(),
                self.timeout,
                self.max_retry_delay,
                arg.clone(),
            );
            handles.push(handle);
        }

        // Spawn aging tasq
        let handle = spawn_aging_tasq(
            Arc::clone(&self.queue),
            Arc::clone(&self.metrics),
            self.shutdown.clone().subscribe(),
            self.aging_duration,
        );
        handles.push(handle);

        handles
    }

    pub async fn shutdown(&self) {
        tracing::info!("Initiating tasque shutdown...");
        // Send shutdown signal to all workers
        let _ = self.shutdown.send(());
    }

    pub async fn get_metrics(&self) -> (usize, usize, usize) {
        (
            self.metrics.total_processed.load(Ordering::SeqCst),
            self.metrics.total_failed.load(Ordering::SeqCst),
            self.metrics.tasks_in_progress.load(Ordering::SeqCst),
        )
    }
}

fn spawn_worker<T>(
    queue: Arc<Mutex<PriorityQueue<T>>>,
    metrics: Arc<Metrics>,
    semaphore: Arc<Semaphore>,
    failed_tasqs: Arc<Mutex<VecDeque<TasqManager<T>>>>,
    mut shutdown_rx: broadcast::Receiver<()>,
    mut task_rx: watch::Receiver<bool>,
    timeout: Duration,
    max_retry_delay: Duration,
    arg: Arc<T::A>,
) -> JoinHandle<()>
where
    T: Tasq + Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            // First check for immediate tasks
            let mut lock = queue.lock().await;
            if let Some(tasq_manager) = lock.pop() {
                drop(lock);

                metrics.tasks_in_progress.fetch_add(1, Ordering::SeqCst);

                let _permit = semaphore.acquire().await.unwrap();

                // Calculate tasq wait time
                let wait_time = tasq_manager.created_at.elapsed();

                let start_time = Instant::now();
                let result = time::timeout(timeout, tasq_manager.tasq.run(&arg)).await;
                let execution_time = start_time.elapsed();

                // Record both wait and execution times
                metrics.record_task_stats(tasq_manager.current_priority, wait_time, execution_time);

                match result {
                    Ok(Ok(_)) => {
                        metrics.total_processed.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {
                        metrics.total_failed.fetch_add(1, Ordering::SeqCst);
                        handle_failed_tasq(tasq_manager, &queue, &failed_tasqs, max_retry_delay)
                            .await;
                    }
                }

                metrics.tasks_in_progress.fetch_sub(1, Ordering::SeqCst);
                continue;
            }

            // Get next wakeup time
            let next_ready = lock.next_ready_time();
            drop(lock);

            let sleep = match next_ready {
                Some(time) => sleep_until(time.into()),
                None => time::sleep(Duration::from_secs(3600)), // 1 hour default if no delayed tasks
            };
            tokio::pin!(sleep);

            tokio::select! {
                _ = shutdown_rx.recv() => return,
                _ = &mut sleep => {
                    // Time elapsed, go check queue again
                    tracing::debug!("Sleep timer elapsed, checking queue");
                },
                Ok(_) = task_rx.changed() => {
                    // New task signal received, update sleep if needed
                    let queue = queue.lock().await;
                    if let Some(next_time) = queue.next_ready_time() {
                        sleep.as_mut().reset(next_time.into());
                    }
                    drop(queue);
                    tracing::debug!("New task signal received, checking queue");
                }
            }
        }
    })
}

async fn handle_failed_tasq<T: Tasq + Send + Sync + 'static>(
    mut tasq_manager: TasqManager<T>,
    queue: &Arc<Mutex<PriorityQueue<T>>>,
    failed_tasqs: &Arc<Mutex<VecDeque<TasqManager<T>>>>,
    max_retry_delay: Duration,
) {
    if tasq_manager.retry_count < tasq_manager.max_retries {
        tasq_manager.retry_count += 1;
        tasq_manager.next_run =
            Instant::now() + calculate_backoff(tasq_manager.retry_count, max_retry_delay);

        let mut queue = queue.lock().await;
        if queue.push(tasq_manager).is_err() {
            tracing::warn!("Queue full, dropping retry attempt");
        }
        drop(queue);
    } else {
        let mut failed_tasqs = failed_tasqs.lock().await;
        if failed_tasqs.len() >= MAX_FAILED_TASK_HISTORY {
            failed_tasqs.pop_front();
        }
        failed_tasqs.push_back(tasq_manager);
    }
}

fn calculate_backoff(retry_count: u32, max_retry_delay: Duration) -> Duration {
    let base = Duration::from_secs(1);
    let exp_backoff = base.mul_f64(2f64.powi(retry_count as i32));
    // Prevent synchronized retries by adding some randomness.
    let with_jitter = exp_backoff.mul_f64(1.0 + rand::rng().random_range(-0.1..0.1));
    std::cmp::min(with_jitter, max_retry_delay)
}

fn spawn_aging_tasq<T>(
    queue: Arc<Mutex<PriorityQueue<T>>>,
    metrics: Arc<Metrics>,
    mut shutdown_rx: broadcast::Receiver<()>,
    aging_duration: Duration,
) -> JoinHandle<()>
where
    T: Tasq + Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("Aging task received shutdown signal");
                    break;
                }
                _ = tokio::time::sleep(aging_duration) => {
                    let mut queue = queue.lock().await;

                    // Update queue length metrics before trying promotions
                    let (high, medium, low, _) = queue.get_queue_metrics();
                    metrics.update_queue_lengths(high, medium, low);

                    // Try to promote tasks based on current metrics
                    let promoted = queue.try_promote_tasks(&metrics);
                    if promoted > 0 {
                        tracing::debug!("Promoted {} tasks", promoted);
                    }
                }
            }
        }
    })
}
