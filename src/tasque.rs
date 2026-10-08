use crate::step::{Handoff, IntoStep};
use crate::store::{DurableJob, Store};
use crate::{BoxError, Error, Priority};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::any::{type_name, Any, TypeId};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio::time::Instant;

/// How many jobs each handler runs at once.
const HANDLER_MAX_CONCURRENCY: usize = 4;
/// How long one attempt may run before it fails.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RETRIES: u8 = 3;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// The head start each priority level gets in line, so low priority work can't starve.
const PRIORITY_AGING_STEP: Duration = Duration::from_secs(60);
/// How often to reclaim jobs from processes that have stopped.
const RECLAIM_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Lifecycle {
    Running,
    /// Finishing running jobs; only durable jobs may still be queued.
    Draining,
    Stopped,
}

/// A `Lifecycle` that can be shared across threads.
struct AtomicLifecycle(AtomicU8);

impl AtomicLifecycle {
    fn new(lifecycle: Lifecycle) -> Self {
        Self(AtomicU8::new(lifecycle as u8))
    }

    fn load(&self) -> Lifecycle {
        Self::decode(self.0.load(AtomicOrdering::SeqCst))
    }

    fn store(&self, lifecycle: Lifecycle) {
        self.0.store(lifecycle as u8, AtomicOrdering::SeqCst);
    }

    fn swap(&self, lifecycle: Lifecycle) -> Lifecycle {
        Self::decode(self.0.swap(lifecycle as u8, AtomicOrdering::SeqCst))
    }

    fn decode(value: u8) -> Lifecycle {
        match value {
            0 => Lifecycle::Running,
            1 => Lifecycle::Draining,
            _ => Lifecycle::Stopped,
        }
    }
}

/// A UUIDv7, so ids sort by when their job was queued.
pub type JobId = uuid::Uuid;
type ErasedJobPayload = Box<dyn Any + Send>;
type NextStep = Option<Handoff>;
type HandlerFuture = Pin<Box<dyn Future<Output = Result<NextStep, BoxError>> + Send>>;

/// Runs each job on the handler registered for its type. Clones share the same handlers.
#[derive(Clone)]
pub struct Tasque {
    inner: Arc<Inner>,
}

impl Tasque {
    /// Starts building a `Tasque`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(store: impl Store) -> Builder {
        Builder {
            store: Box::new(store),
            handlers: HashMap::new(),
        }
    }

    /// Queues `job` on its type's handler. `retries: None` means 3. With `persist`, the
    /// job is saved to the store before this returns.
    pub async fn queue<T: Send + 'static>(
        &self,
        job: T,
        priority: Priority,
        retries: Option<u8>,
        persist: bool,
    ) -> Result<(), Error> {
        let ty = (TypeId::of::<T>(), type_name::<T>());
        self.inner
            .enqueue(ty, Box::new(job), priority, retries, persist)
            .await
    }

    /// Replays unfinished jobs from the store.
    pub async fn run(&self) -> Result<(), Error> {
        let recovered = self.inner.store.reclaim_stale().await.map_err(Error::Store)?;
        self.inner.restore_jobs(recovered);
        let inner = self.inner.clone();

        tokio::spawn(reclaim_loop(Arc::downgrade(&inner)));

        Ok(())
    }

    /// Stops taking jobs, waits for running attempts, then releases the store's
    /// unfinished jobs to other processes.
    ///
    /// Jobs waiting in line or for a retry don't run here. Persisted ones are picked up
    /// by another process; in-memory ones are discarded.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let inner = &self.inner;
        if inner.lifecycle.swap(Lifecycle::Draining) != Lifecycle::Running {
            return Ok(());
        }

        loop {
            let idle = inner.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();

            if inner
                .handlers
                .values()
                .all(|h| h.queue.lock().unwrap().running == 0)
            {
                break;
            }

            idle.await;
        }

        let discarded: usize = inner
            .handlers
            .values()
            .map(|h| std::mem::take(&mut h.queue.lock().unwrap().pending).len())
            .sum();
        if discarded > 0 {
            tracing::info!("discarded {discarded} non-durable jobs waiting in line");
        }

        inner.lifecycle.store(Lifecycle::Stopped);
        inner.store.release().await.map_err(Error::Store)
    }
}

/// Registers handlers, then [`build`](Builder::build)s the [`Tasque`].
pub struct Builder {
    store: Box<dyn Store>,
    handlers: HashMap<TypeId, Handler>,
}

impl Builder {
    /// Registers `handler` for jobs of type `I`. It reaches `state` through its
    /// [`Context`].
    ///
    /// `name` identifies `I` in the store, so keep it stable once `I` is persisted.
    ///
    /// # Panics
    ///
    /// If another handler is registered under `name` or for `I`.
    pub fn add<S, I, F, Fut, O>(mut self, name: &'static str, state: S, handler: F) -> Self
    where
        S: Send + Sync + 'static,
        I: Clone + Serialize + DeserializeOwned + Send + 'static,
        F: Fn(Context<S>, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, BoxError>> + Send + 'static,
        O: IntoStep,
    {
        assert!(
            self.handlers.values().all(|h| h.name != name),
            "tasque: a handler named {name:?} is already registered"
        );

        assert!(
            !self.handlers.contains_key(&TypeId::of::<I>()),
            "tasque: a handler for {} is already registered",
            type_name::<I>()
        );

        self.handlers.insert(
            TypeId::of::<I>(),
            Handler {
                name,
                invoker: Box::new(FunctionInvoker::<S, I, F, O> {
                    state: Arc::new(state),
                    f: handler,
                    _types: PhantomData,
                }),
                next_handler: O::next_handler(),
                queue: Mutex::default(),
            },
        );

        self
    }

    /// Checks every [`Step::next`](crate::Step::next) type has a handler,
    /// then builds `Tasque`.
    pub fn build(self) -> Result<Tasque, Error> {
        for handler in self.handlers.values() {
            if let Some((handler_type_id, handler_name)) = handler.next_handler {
                if !self.handlers.contains_key(&handler_type_id) {
                    return Err(Error::MissingHandler {
                        handler: handler.name,
                        next: handler_name,
                    });
                }
            }
        }

        let inner = Arc::new(Inner {
            store: self.store,
            handlers: self.handlers,
            sequence: AtomicU64::new(0),
            lifecycle: AtomicLifecycle::new(Lifecycle::Running),
            idle: Notify::new(),
        });

        Ok(Tasque { inner })
    }
}

/// Passed to each handler run. Derefs to the state given to [`Builder::add`].
pub struct Context<S> {
    state: Arc<S>,
    tasque: Tasque,
    attempt_count: u8,
}

impl<S> Deref for Context<S> {
    type Target = S;

    fn deref(&self) -> &S {
        &self.state
    }
}

impl<S> Context<S> {
    /// 0 on the first run, 1 on the first retry, and so on.
    pub fn attempt_count(&self) -> u8 {
        self.attempt_count
    }

    /// Queues another job, as [`Tasque::queue`] does.
    pub async fn queue<T: Send + 'static>(
        &self,
        job: T,
        priority: Priority,
        max_retries: Option<u8>,
        durable: bool,
    ) -> Result<(), Error> {
        self.tasque.queue(job, priority, max_retries, durable).await
    }
}

struct Inner {
    store: Box<dyn Store>,
    handlers: HashMap<TypeId, Handler>,
    sequence: AtomicU64,
    lifecycle: AtomicLifecycle,
    /// Woken whenever a slot frees up, for `shutdown`.
    idle: Notify,
}

struct Handler {
    name: &'static str,
    invoker: Box<dyn Invoker>,
    next_handler: Option<(TypeId, &'static str)>,
    queue: Mutex<HandlerQueue>,
}

/// A handler's running count and the line of jobs waiting for a slot.
#[derive(Default)]
struct HandlerQueue {
    running: usize,
    /// `Reverse` so the heap pops the earliest job first.
    pending: BinaryHeap<Reverse<Job>>,
}

struct Job {
    id: JobId,
    type_id: TypeId,
    payload: ErasedJobPayload,
    priority: Priority,
    max_retries: u8,
    attempt: u8,
    durable: bool,
    enqueued_at: Instant,
    sequence: u64,
}

impl Job {
    /// Its place in line. Lower priority counts as arriving later.
    fn queue_position(&self) -> Instant {
        let priority_offset = Priority::High as u32 - self.priority as u32;
        self.enqueued_at + PRIORITY_AGING_STEP * priority_offset
    }
}

/// Ordered by place in line, not identity.
impl Ord for Job {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.queue_position(), self.sequence).cmp(&(other.queue_position(), other.sequence))
    }
}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Job {}

impl Inner {
    async fn enqueue(
        self: &Arc<Self>,
        (type_id, type_name): (TypeId, &'static str),
        payload: ErasedJobPayload,
        priority: Priority,
        max_retries: Option<u8>,
        durable: bool,
    ) -> Result<(), Error> {
        let handler = self
            .handlers
            .get(&type_id)
            .ok_or(Error::Unregistered(type_name))?;

        // While draining, a persisted job is saved for another process to run.
        let lifecycle = self.lifecycle.load();
        if lifecycle == Lifecycle::Stopped || (lifecycle == Lifecycle::Draining && !durable) {
            return Err(Error::Stopped);
        }

        let job = Job {
            id: JobId::now_v7(),
            type_id,
            payload,
            priority,
            max_retries: max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
            attempt: 0,
            durable,
            enqueued_at: Instant::now(),
            sequence: self.sequence.fetch_add(1, AtomicOrdering::Relaxed),
        };

        if durable {
            let record = to_record(handler, &job).map_err(Error::Encode)?;
            self.store.save(&record).await.map_err(Error::Store)?;
        }

        self.submit(job);

        Ok(())
    }

    /// Starts `job` if its handler has a free slot, or puts it in line.
    fn submit(self: &Arc<Self>, job: Job) {
        if self.lifecycle.load() != Lifecycle::Running {
            return;
        }

        let mut handler_queue = self.handlers[&job.type_id].queue.lock().unwrap();
        if handler_queue.running < HANDLER_MAX_CONCURRENCY {
            handler_queue.running += 1;
            drop(handler_queue);
            
            tokio::spawn(run_handler_slot(self.clone(), job));
        } else {
            handler_queue.pending.push(Reverse(job));
        }
    }

    async fn run_attempt(self: &Arc<Self>, mut job: Job) {
        let handler = &self.handlers[&job.type_id];

        if job.durable && !self.store.is_owned(job.id) {
            tracing::info!(
                handler = handler.name,
                "job {:032x} was claimed by another process; dropping it",
                job.id
            );
            return;
        }

        let tasque = Tasque {
            inner: self.clone(),
        };

        // A separate task, so a panic fails the attempt without losing the slot.
        let mut task = tokio::spawn(handler.invoker.invoke(tasque, job.attempt, &*job.payload));
        let result = match tokio::time::timeout(ATTEMPT_TIMEOUT, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(err.into()),
            Err(_) => {
                task.abort();
                Err(format!("timed out after {ATTEMPT_TIMEOUT:?}").into())
            }
        };

        match result {
            Ok(next_step) => {
                let (job_id, durable) = (job.id, job.durable);
                self.advance(job, next_step).await;

                if durable {
                    self.finish(job_id).await;
                }
            }
            Err(err) if job.attempt < job.max_retries => {
                job.attempt += 1;
                let delay = retry_delay(job.attempt);

                tracing::warn!(
                    handler = handler.name,
                    attempt = job.attempt,
                    ?delay,
                    "{err}; retrying"
                );

                let inner = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    inner.submit(job);
                });
            }
            Err(err) => {
                tracing::error!(
                    handler = handler.name,
                    "{err}; giving up after {} retries",
                    job.max_retries
                );

                if job.durable {
                    self.fail(job.id, &err.to_string()).await;
                }
            }
        }
    }

    async fn advance(self: &Arc<Self>, job: Job, next: NextStep) {
        if let Some(handoff) = next {
            let ty = (handoff.type_id, handoff.type_name);
            let queued = self
                .enqueue(
                    ty,
                    handoff.payload,
                    handoff.priority.unwrap_or(job.priority),
                    Some(handoff.max_retries.unwrap_or(job.max_retries)),
                    job.durable || handoff.durable,
                )
                .await;
            if let Err(err) = queued {
                tracing::error!(
                    handler = self.handlers[&ty.0].name,
                    "failed to queue next step: {err}"
                );
            }
        }
    }

    async fn finish(&self, job_id: JobId) {
        if let Err(err) = self.store.finish(job_id).await {
            tracing::error!("store failed to finish job {job_id:032x}: {err}");
        }
    }

    async fn fail(&self, job_id: JobId, error: &str) {
        if let Err(err) = self.store.fail(job_id, error).await {
            tracing::error!("store failed to mark job {job_id:032x} failed: {err}");
        }
    }

    /// Runs claimed records, earliest in line first.
    fn restore_jobs(self: &Arc<Self>, records: Vec<DurableJob>) {
        let mut jobs: Vec<Job> = records.into_iter().filter_map(|r| self.restore_job(r)).collect();
        jobs.sort();
        for job in jobs {
            self.submit(job);
        }
    }

    /// Turns a stored record back into a job, if a handler takes it.
    fn restore_job(&self, record: DurableJob) -> Option<Job> {
        let Some((&type_id, handler)) = self.handlers.iter().find(|(_, h)| h.name == record.handler_name)
        else {
            tracing::warn!(
                "no handler named {:?}; leaving job {:032x} in the store",
                record.handler_name,
                record.id
            );
            return None;
        };

        let value = match handler.invoker.decode(&record.payload) {
            Ok(value) => value,
            Err(err) => {
                tracing::error!(
                    handler = handler.name,
                    "failed to read job {:032x}, leaving it in the store: {err}",
                    record.id
                );
                return None;
            }
        };

        let age = SystemTime::now()
            .duration_since(record.enqueued_at)
            .unwrap_or_default();
        Some(Job {
            id: record.id,
            type_id,
            payload: value,
            priority: record.priority,
            max_retries: record.max_retries,
            attempt: 0,
            durable: true,
            enqueued_at: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
            sequence: self.sequence.fetch_add(1, AtomicOrdering::Relaxed),
        })
    }
}

/// Holds a slot: runs `job`, then the rest of its handler's line.
async fn run_handler_slot(inner: Arc<Inner>, mut job: Job) {
    let type_id = job.type_id;
    loop {
        inner.run_attempt(job).await;

        let mut handler_queue = inner.handlers[&type_id].queue.lock().unwrap();
        let next = match inner.lifecycle.load() {
            Lifecycle::Running => handler_queue.pending.pop(),
            _ => None,
        };
        match next {
            Some(Reverse(next)) => job = next,
            None => {
                handler_queue.running -= 1;
                drop(handler_queue);
                inner.idle.notify_waiters();
                return;
            }
        }
    }
}

/// Reclaims jobs from stopped processes every `CLAIM_INTERVAL`, until shutdown.
async fn reclaim_loop(inner: Weak<Inner>) {
    loop {
        tokio::time::sleep(RECLAIM_INTERVAL).await;

        let Some(inner) = inner.upgrade() else {
            return;
        };

        if inner.lifecycle.load() != Lifecycle::Running {
            return;
        }

        match inner.store.reclaim_stale().await {
            Ok(recovered) => inner.restore_jobs(recovered),
            Err(err) => tracing::error!("store failed to claim jobs: {err}"),
        }
    }
}

fn to_record(handler: &Handler, job: &Job) -> Result<DurableJob, BoxError> {
    Ok(DurableJob {
        id: job.id,
        handler_name: handler.name.to_string(),
        payload: handler.invoker.encode(&*job.payload)?,
        priority: job.priority,
        max_retries: job.max_retries,
        enqueued_at: SystemTime::now()
            .checked_sub(job.enqueued_at.elapsed())
            .unwrap_or(UNIX_EPOCH),
    })
}

/// 1s, 2s, 4s… up to 5 minutes, ±10% so retries don't line up.
fn retry_delay(attempt: u8) -> Duration {
    let doublings = u32::from(attempt.saturating_sub(1)).min(31);
    let delay = INITIAL_RETRY_DELAY
        .saturating_mul(1 << doublings)
        .min(MAX_RETRY_DELAY);
    delay
        .mul_f64(0.9 + rand::random::<f64>() * 0.2)
        .min(MAX_RETRY_DELAY)
}

/// A handler with its types erased, so one `Tasque` can hold them all.
trait Invoker: Send + Sync {
    fn invoke(&self, tasque: Tasque, attempt_count: u8, value: &(dyn Any + Send)) -> HandlerFuture;
    fn encode(&self, value: &(dyn Any + Send)) -> Result<Vec<u8>, BoxError>;
    fn decode(&self, bytes: &[u8]) -> Result<ErasedJobPayload, BoxError>;
}

struct FunctionInvoker<S, I, F, O> {
    state: Arc<S>,
    f: F,
    _types: PhantomData<fn(I) -> O>,
}

impl<S, I, F, Fut, O> Invoker for FunctionInvoker<S, I, F, O>
where
    S: Send + Sync + 'static,
    I: Clone + Serialize + DeserializeOwned + Send + 'static,
    F: Fn(Context<S>, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, BoxError>> + Send + 'static,
    O: IntoStep,
{
    fn invoke(&self, tasque: Tasque, attempt_count: u8, payload: &(dyn Any + Send)) -> HandlerFuture {
        let ctx = Context {
            state: self.state.clone(),
            tasque,
            attempt_count,
        };
        let input = downcast::<I>(payload).clone();
        let future = (self.f)(ctx, input);
        Box::pin(async move { future.await.map(IntoStep::into_handoff) })
    }

    fn encode(&self, value: &(dyn Any + Send)) -> Result<Vec<u8>, BoxError> {
        crate::codec::encode(downcast::<I>(value))
    }

    fn decode(&self, bytes: &[u8]) -> Result<ErasedJobPayload, BoxError> {
        Ok(Box::new(crate::codec::decode::<I>(bytes)?))
    }
}

fn downcast<I: 'static>(payload: &(dyn Any + Send)) -> &I {
    payload
        .downcast_ref::<I>()
        .expect("jobs are only routed to the handler of their type")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(priority: Priority, enqueued_at: Instant, seq: u64) -> Job {
        Job {
            id: JobId::nil(),
            type_id: TypeId::of::<()>(),
            payload: Box::new(()),
            priority,
            max_retries: 0,
            attempt: 0,
            durable: false,
            enqueued_at,
            sequence: seq,
        }
    }

    fn line(jobs: Vec<Job>) -> Vec<u64> {
        let mut heap: BinaryHeap<Reverse<Job>> = jobs.into_iter().map(Reverse).collect();
        std::iter::from_fn(|| heap.pop().map(|Reverse(job)| job.sequence)).collect()
    }

    #[test]
    fn higher_priority_goes_first() {
        let now = Instant::now();
        let order = line(vec![
            job(Priority::Low, now, 0),
            job(Priority::Medium, now, 1),
            job(Priority::High, now, 2),
        ]);

        assert_eq!(order, [2, 1, 0]);
    }

    #[test]
    fn same_priority_is_first_come_first_served() {
        let now = Instant::now();
        let order = line(vec![
            job(Priority::Medium, now + Duration::from_secs(1), 0),
            job(Priority::Medium, now, 2),
            job(Priority::Medium, now, 1),
        ]);

        assert_eq!(order, [1, 2, 0]);
    }

    #[test]
    fn a_pending_job_is_only_passed_within_the_head_start() {
        let now = Instant::now();
        let low = job(Priority::Low, now, 0);
        // Two levels up gets 2 minutes of head start.
        let high_soon = job(Priority::High, now + Duration::from_secs(119), 1);
        let high_late = job(Priority::High, now + Duration::from_secs(121), 2);

        assert_eq!(line(vec![low, high_soon, high_late]), [1, 0, 2]);
    }

    #[test]
    fn retry_delay_doubles_up_to_the_cap() {
        for (attempt, expected) in [(1, 1), (2, 2), (3, 4), (4, 8)] {
            let delay = retry_delay(attempt).as_secs_f64();
            let expected = expected as f64;

            assert!(
                (expected * 0.9..=expected * 1.1).contains(&delay),
                "attempt {attempt}: {delay}s"
            );
        }

        assert!(retry_delay(u8::MAX) <= MAX_RETRY_DELAY);
    }
}
