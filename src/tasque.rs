use crate::step::{Handoff, IntoStep};
use crate::store::{Record, Store};
use crate::{BoxError, Error, Priority};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::{Any, TypeId, type_name};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// How many jobs each handler runs at once.
const SLOTS: usize = 4;
/// How long one attempt may run before it fails.
const TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RETRIES: u8 = 3;
const BASE_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// The head start each priority level gets in line, so low priority work can't starve.
const PRIORITY_STEP: Duration = Duration::from_secs(60);

/// A job with its type erased.
type Value = Box<dyn Any + Send>;
type Next = Option<Handoff>;
type CallFuture = Pin<Box<dyn Future<Output = Result<Next, BoxError>> + Send>>;

/// Runs each job on the handler registered for its type. Clones share the same handlers.
pub struct Tasque<S> {
    inner: Arc<Inner<S>>,
}

impl<S> Clone for Tasque<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Send + Sync + 'static> Tasque<S> {
    /// Starts building a `Tasque`. Handlers reach `state` through their [`Ctx`].
    #[allow(clippy::new_ret_no_self)]
    pub fn new(store: impl Store, state: S) -> Builder<S> {
        Builder {
            state,
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
            .queue(ty, Box::new(job), priority, retries, persist)
            .await
    }
}

/// Registers handlers, then [`run`](Builder::run)s the [`Tasque`].
pub struct Builder<S> {
    state: S,
    store: Box<dyn Store>,
    handlers: HashMap<TypeId, Handler<S>>,
}

impl<S: Send + Sync + 'static> Builder<S> {
    /// Registers `handler` for jobs of type `I`.
    ///
    /// `name` identifies `I` in the store, so keep it stable once `I` is persisted.
    ///
    /// # Panics
    ///
    /// If another handler is registered under `name` or for `I`.
    pub fn add<I, F, Fut, O>(mut self, name: &'static str, handler: F) -> Self
    where
        I: Clone + Serialize + DeserializeOwned + Send + 'static,
        F: Fn(Ctx<S>, I) -> Fut + Send + Sync + 'static,
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
                call: Box::new(FnCall::<I, F, O> {
                    f: handler,
                    _types: PhantomData,
                }),
                next: O::next_type(),
                lane: Mutex::default(),
            },
        );

        self
    }

    /// Checks every [`Step::next`](crate::Step::next) type has a handler, then replays
    /// unfinished jobs from the store.
    pub async fn run(self) -> Result<Tasque<S>, Error> {
        for handler in self.handlers.values() {
            if let Some((next, next_name)) = handler.next {
                if !self.handlers.contains_key(&next) {
                    return Err(Error::MissingHandler {
                        handler: handler.name,
                        next: next_name,
                    });
                }
            }
        }

        let inner = Arc::new(Inner {
            state: self.state,
            store: self.store,
            handlers: self.handlers,
            seq: AtomicU64::new(0),
        });

        let records = inner.store.pending().await.map_err(Error::Store)?;
        let mut jobs: Vec<Job> = records
            .into_iter()
            .filter_map(|record| inner.replay(record))
            .collect();
        jobs.sort();
        for job in jobs {
            inner.submit(job);
        }

        Ok(Tasque { inner })
    }
}

/// Passed to each handler run. Derefs to the state given to [`Tasque::new`].
pub struct Ctx<S> {
    inner: Arc<Inner<S>>,
    attempt: u8,
}

impl<S> Deref for Ctx<S> {
    type Target = S;

    fn deref(&self) -> &S {
        &self.inner.state
    }
}

impl<S: Send + Sync + 'static> Ctx<S> {
    /// 0 on the first run, 1 on the first retry, and so on.
    pub fn attempt(&self) -> u8 {
        self.attempt
    }

    /// Queues another job, as [`Tasque::queue`] does.
    pub async fn queue<T: Send + 'static>(
        &self,
        job: T,
        priority: Priority,
        retries: Option<u8>,
        persist: bool,
    ) -> Result<(), Error> {
        let ty = (TypeId::of::<T>(), type_name::<T>());
        self.inner
            .queue(ty, Box::new(job), priority, retries, persist)
            .await
    }
}

struct Inner<S> {
    state: S,
    store: Box<dyn Store>,
    handlers: HashMap<TypeId, Handler<S>>,
    seq: AtomicU64,
}

struct Handler<S> {
    name: &'static str,
    call: Box<dyn Call<S>>,
    next: Option<(TypeId, &'static str)>,
    lane: Mutex<Lane>,
}

/// A handler's running count and the line of jobs waiting for a slot.
#[derive(Default)]
struct Lane {
    running: usize,
    /// `Reverse` so the heap pops the earliest job first.
    waiting: BinaryHeap<Reverse<Job>>,
}

struct Job {
    id: u128,
    type_id: TypeId,
    value: Value,
    priority: Priority,
    retries: u8,
    attempt: u8,
    persist: bool,
    enqueued_at: Instant,
    seq: u64,
}

impl Job {
    /// Its place in line. Lower priority counts as arriving later.
    fn rank(&self) -> Instant {
        let behind = Priority::High as u32 - self.priority as u32;
        self.enqueued_at + PRIORITY_STEP * behind
    }
}

/// Ordered by place in line, not identity.
impl Ord for Job {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.rank(), self.seq).cmp(&(other.rank(), other.seq))
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

impl<S: Send + Sync + 'static> Inner<S> {
    /// Queues an erased job. A job the store can't save isn't run.
    async fn queue(
        self: &Arc<Self>,
        (type_id, type_name): (TypeId, &'static str),
        value: Value,
        priority: Priority,
        retries: Option<u8>,
        persist: bool,
    ) -> Result<(), Error> {
        let handler = self
            .handlers
            .get(&type_id)
            .ok_or(Error::Unregistered(type_name))?;

        let job = Job {
            id: rand::random(),
            type_id,
            value,
            priority,
            retries: retries.unwrap_or(DEFAULT_RETRIES),
            attempt: 0,
            persist,
            enqueued_at: Instant::now(),
            seq: self.seq.fetch_add(1, AtomicOrdering::Relaxed),
        };

        if persist {
            let record = record(handler, &job).map_err(Error::Encode)?;
            self.store.save(&record).await.map_err(Error::Store)?;
        }

        self.submit(job);

        Ok(())
    }

    /// Starts `job` if its handler has a free slot, or puts it in line.
    fn submit(self: &Arc<Self>, job: Job) {
        let mut lane = self.handlers[&job.type_id].lane.lock().unwrap();
        if lane.running < SLOTS {
            lane.running += 1;
            drop(lane);
            tokio::spawn(work(self.clone(), job));
        } else {
            lane.waiting.push(Reverse(job));
        }
    }

    async fn attempt(self: &Arc<Self>, mut job: Job) {
        let handler = &self.handlers[&job.type_id];
        let ctx = Ctx {
            inner: self.clone(),
            attempt: job.attempt,
        };

        // A separate task, so a panic fails the attempt without losing the slot.
        let mut task = tokio::spawn(handler.call.call(ctx, &*job.value));
        let result = match tokio::time::timeout(TIMEOUT, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(err.into()),
            Err(_) => {
                task.abort();
                Err(format!("timed out after {TIMEOUT:?}").into())
            }
        };

        let job_id = job.id;
        let persist_job = job.persist;

        match result {
            Ok(next) => self.advance(job, next).await,
            Err(err) if job.attempt < job.retries => {
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
                    job.retries
                );
            }
        }

        if persist_job {
            self.finish(job_id).await;
        }
    }

    async fn advance(self: &Arc<Self>, job: Job, next: Next) {
        if let Some(handoff) = next {
            let ty = (handoff.type_id, handoff.type_name);
            let queued = self
                .queue(
                    ty,
                    handoff.value,
                    handoff.priority.unwrap_or(job.priority),
                    Some(handoff.retries.unwrap_or(job.retries)),
                    job.persist || handoff.persist,
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

    async fn finish(&self, id: u128) {
        if let Err(err) = self.store.finish(id).await {
            tracing::error!("store failed to finish job {id:032x}: {err}");
        }
    }

    /// Turns a stored record back into a job, if a handler takes it.
    fn replay(&self, record: Record) -> Option<Job> {
        let Some((&type_id, handler)) = self.handlers.iter().find(|(_, h)| h.name == record.name)
        else {
            tracing::warn!(
                "no handler named {:?}; leaving job {:032x} in the store",
                record.name,
                record.id
            );
            return None;
        };

        let value = match handler.call.decode(&record.payload) {
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
            value,
            priority: record.priority,
            retries: record.retries,
            attempt: 0,
            persist: true,
            enqueued_at: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
            seq: self.seq.fetch_add(1, AtomicOrdering::Relaxed),
        })
    }
}

/// Holds a slot: runs `job`, then the rest of its handler's line.
async fn work<S: Send + Sync + 'static>(inner: Arc<Inner<S>>, mut job: Job) {
    let type_id = job.type_id;
    loop {
        inner.attempt(job).await;

        let mut lane = inner.handlers[&type_id].lane.lock().unwrap();
        match lane.waiting.pop() {
            Some(Reverse(next)) => job = next,
            None => {
                lane.running -= 1;
                return;
            }
        }
    }
}

fn record<S>(handler: &Handler<S>, job: &Job) -> Result<Record, BoxError> {
    Ok(Record {
        id: job.id,
        name: handler.name.to_string(),
        payload: handler.call.encode(&*job.value)?,
        priority: job.priority,
        retries: job.retries,
        enqueued_at: SystemTime::now()
            .checked_sub(job.enqueued_at.elapsed())
            .unwrap_or(UNIX_EPOCH),
    })
}

/// 1s, 2s, 4s… up to 5 minutes, ±10% so retries don't line up.
fn retry_delay(attempt: u8) -> Duration {
    let doublings = u32::from(attempt.saturating_sub(1)).min(31);
    let delay = BASE_RETRY_DELAY
        .saturating_mul(1 << doublings)
        .min(MAX_RETRY_DELAY);
    delay
        .mul_f64(0.9 + rand::random::<f64>() * 0.2)
        .min(MAX_RETRY_DELAY)
}

/// A handler with its types erased, so one `Tasque` can hold them all.
trait Call<S>: Send + Sync {
    fn call(&self, ctx: Ctx<S>, value: &(dyn Any + Send)) -> CallFuture;
    fn encode(&self, value: &(dyn Any + Send)) -> Result<Vec<u8>, BoxError>;
    fn decode(&self, bytes: &[u8]) -> Result<Value, BoxError>;
}

struct FnCall<I, F, O> {
    f: F,
    _types: PhantomData<fn(I) -> O>,
}

impl<S, I, F, Fut, O> Call<S> for FnCall<I, F, O>
where
    S: Send + Sync + 'static,
    I: Clone + Serialize + DeserializeOwned + Send + 'static,
    F: Fn(Ctx<S>, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, BoxError>> + Send + 'static,
    O: IntoStep,
{
    fn call(&self, ctx: Ctx<S>, value: &(dyn Any + Send)) -> CallFuture {
        let input = downcast::<I>(value).clone();
        let future = (self.f)(ctx, input);
        Box::pin(async move { future.await.map(IntoStep::into_next) })
    }

    fn encode(&self, value: &(dyn Any + Send)) -> Result<Vec<u8>, BoxError> {
        crate::codec::encode(downcast::<I>(value))
    }

    fn decode(&self, bytes: &[u8]) -> Result<Value, BoxError> {
        Ok(Box::new(crate::codec::decode::<I>(bytes)?))
    }
}

fn downcast<I: 'static>(value: &(dyn Any + Send)) -> &I {
    value
        .downcast_ref::<I>()
        .expect("jobs are only routed to the handler of their type")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(priority: Priority, enqueued_at: Instant, seq: u64) -> Job {
        Job {
            id: 0,
            type_id: TypeId::of::<()>(),
            value: Box::new(()),
            priority,
            retries: 0,
            attempt: 0,
            persist: false,
            enqueued_at,
            seq,
        }
    }

    fn line(jobs: Vec<Job>) -> Vec<u64> {
        let mut heap: BinaryHeap<Reverse<Job>> = jobs.into_iter().map(Reverse).collect();
        std::iter::from_fn(|| heap.pop().map(|Reverse(job)| job.seq)).collect()
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
    fn a_waiting_job_is_only_passed_within_the_head_start() {
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
