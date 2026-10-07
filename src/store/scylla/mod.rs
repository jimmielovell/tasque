//! Jobs in ScyllaDB, shared by any number of processes.
//!
//! Jobs are filed under a *worker*. Each process *owns* one worker of its own, plus any
//! it has reclaimed, and renews a lease on each by heartbeat. When a worker's lease
//! goes stale, another process reclaims it and runs its unfinished jobs.

mod builder;

pub use builder::ScyllaStoreBuilder;

use crate::{BoxError, DurableJob, JobId, Priority, Store};
use async_trait::async_trait;
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::serialize::row::SerializeRow;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::{CqlTimestamp, CqlValue, Row};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::AbortHandle;
use uuid::Uuid;

/// How long a job's row lives.
const JOB_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// How often an owner renews its workers' leases.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// How long a lease lasts without a heartbeat before another process may reclaim it.
const WORKER_LEASE_TIMEOUT: Duration = Duration::from_secs(15);
const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// The worker a job is filed under.
type WorkerId = Uuid;
/// The process that owns a worker.
type OwnerId = Uuid;
/// Hours since the epoch: the partition a job is filed in within its worker.
type HourBucket = i64;

#[derive(Clone, Copy)]
struct JobLocation {
    worker_id: WorkerId,
    hour_bucket: HourBucket,
}

#[derive(Clone, Copy, PartialEq)]
#[repr(i8)]
enum JobStatus {
    Pending = 0,
    Done = 1,
    Failed = 2,
}

impl TryFrom<i8> for JobStatus {
    type Error = i8;

    fn try_from(value: i8) -> Result<Self, i8> {
        match value {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Done),
            2 => Ok(Self::Failed),
            other => Err(other),
        }
    }
}

/// Keeps jobs in ScyllaDB, shared by any number of processes.
///
/// Each process files its jobs under a worker it owns and renews the worker's lease
/// every 5 seconds. A worker whose lease goes 15 seconds without renewal, or is
/// released, is reclaimed by another process, which runs its unfinished jobs.
pub struct ScyllaStore {
    inner: Arc<Inner>,
    heartbeat_task: AbortHandle,
}

impl Drop for ScyllaStore {
    fn drop(&mut self) {
        self.heartbeat_task.abort();
    }
}

struct Inner {
    session: Arc<Session>,
    /// This process.
    owner_id: OwnerId,
    statements: Statements,
    ownership: Mutex<Ownership>,
}

struct Statements {
    insert_job: PreparedStatement,
    update_job_status: PreparedStatement,
    load_job: PreparedStatement,
    scan_job_bucket: PreparedStatement,
    insert_failed_job: PreparedStatement,
    register_worker: PreparedStatement,
    scan_workers: PreparedStatement,
    renew_worker_lease: PreparedStatement,
    reclaim_worker: PreparedStatement,
    delete_worker: PreparedStatement,
}

/// The workers this process owns, and the unfinished jobs filed under them.
struct Ownership {
    /// The worker new jobs are filed under. Replaced if another process reclaims it.
    own_worker_id: WorkerId,
    /// When the own worker's lease was last renewed.
    own_lease_renewed_at: Instant,
    /// Every worker this process owns: its own and any it reclaimed.
    worker_ids: HashSet<WorkerId>,
    jobs: HashMap<JobId, JobLocation>,
    released: bool,
}

impl Ownership {
    /// The oldest hour bucket `worker_id` has unfinished jobs in: where a reclaim scan
    /// has to start.
    fn oldest_job_bucket(&self, worker_id: WorkerId, current: HourBucket) -> HourBucket {
        self.jobs
            .values()
            .filter(|job| job.worker_id == worker_id)
            .map(|job| job.hour_bucket)
            .min()
            .unwrap_or(current)
    }

    fn has_unfinished_jobs(&self, worker_id: WorkerId) -> bool {
        self.jobs.values().any(|job| job.worker_id == worker_id)
    }
}

impl Inner {
    /// Executes a conditional (LWT) statement and reports whether it applied.
    async fn execute_conditional(
        &self,
        statement: &PreparedStatement,
        values: impl SerializeRow,
    ) -> Result<bool, BoxError> {
        let row = self
            .session
            .execute_unpaged(statement, values)
            .await?
            .into_rows_result()?
            .first_row::<Row>()?;

        Ok(matches!(
            row.columns.first(),
            Some(Some(CqlValue::Boolean(true)))
        ))
    }

    /// Where job `job_id` is filed, if this process still owns it.
    fn job_location(&self, job_id: JobId) -> Option<JobLocation> {
        self.ownership.lock().unwrap().jobs.get(&job_id).copied()
    }

    async fn update_job_status(
        &self,
        location: JobLocation,
        job_id: JobId,
        status: JobStatus,
    ) -> Result<(), BoxError> {
        let values = (
            status as i8,
            location.worker_id,
            location.hour_bucket,
            job_id,
        );
        self.session
            .execute_unpaged(&self.statements.update_job_status, values)
            .await?;

        Ok(())
    }

    /// Stops tracking a job, once its status is written, and deletes its worker if
    /// that was the last job of a reclaimed one.
    async fn untrack_job(&self, job_id: JobId, location: JobLocation) -> Result<(), BoxError> {
        let idle_reclaimed_worker = {
            let mut ownership = self.ownership.lock().unwrap();
            ownership.jobs.remove(&job_id);
            let worker_id = location.worker_id;
            worker_id != ownership.own_worker_id
                && !ownership.has_unfinished_jobs(worker_id)
                && ownership.worker_ids.remove(&worker_id)
        };

        if idle_reclaimed_worker {
            self.delete_worker(location.worker_id).await?;
        }

        Ok(())
    }

    async fn delete_worker(&self, worker_id: WorkerId) -> Result<bool, BoxError> {
        self.execute_conditional(&self.statements.delete_worker, (worker_id, self.owner_id))
            .await
    }

    /// Registers a new worker owned by this process, to file new jobs under.
    async fn register_worker(&self) -> Result<WorkerId, BoxError> {
        let worker_id = uuid::Builder::from_random_bytes(rand::random()).into_uuid();
        let now = now_ms();
        let values = (worker_id, self.owner_id, CqlTimestamp(now), now / HOUR_MS);

        if !self
            .execute_conditional(&self.statements.register_worker, values)
            .await?
        {
            return Err(format!("worker {worker_id} already exists").into());
        }

        Ok(worker_id)
    }

    /// Renews the lease on `worker_id`. Reports whether this process still owns it.
    async fn renew_lease(&self, worker_id: WorkerId, heartbeat_at: i64) -> Result<bool, BoxError> {
        let oldest_job_bucket = {
            let ownership = self.ownership.lock().unwrap();
            ownership.oldest_job_bucket(worker_id, heartbeat_at / HOUR_MS)
        };
        let values = (
            CqlTimestamp(heartbeat_at),
            oldest_job_bucket,
            worker_id,
            self.owner_id,
        );

        self.execute_conditional(&self.statements.renew_worker_lease, values)
            .await
    }

    /// Renews every lease this process holds, letting go of workers reclaimed by
    /// another process. If that includes its own worker, registers a new one.
    async fn renew_leases(&self) -> Result<(), BoxError> {
        let worker_ids: Vec<WorkerId> = {
            let ownership = self.ownership.lock().unwrap();
            if ownership.released {
                return Ok(());
            }

            ownership.worker_ids.iter().copied().collect()
        };

        let now = now_ms();
        for worker_id in worker_ids {
            if self.renew_lease(worker_id, now).await? {
                let mut ownership = self.ownership.lock().unwrap();
                if worker_id == ownership.own_worker_id {
                    ownership.own_lease_renewed_at = Instant::now();
                }
            } else {
                self.lose_worker(worker_id).await?;
            }
        }

        Ok(())
    }

    /// Lets go of a worker another process has reclaimed, and of its jobs: those not
    /// yet started here are dropped, and those still running finish but aren't
    /// recorded, since the other process now owns them.
    async fn lose_worker(&self, worker_id: WorkerId) -> Result<(), BoxError> {
        let was_own = {
            let mut ownership = self.ownership.lock().unwrap();
            ownership.worker_ids.remove(&worker_id);
            let before = ownership.jobs.len();
            ownership.jobs.retain(|_, job| job.worker_id != worker_id);
            let lost = before - ownership.jobs.len();
            tracing::warn!("worker {worker_id} was reclaimed by another process, with {lost} jobs");
            worker_id == ownership.own_worker_id
        };
        if was_own {
            let new_worker_id = self.register_worker().await?;
            let mut ownership = self.ownership.lock().unwrap();
            ownership.own_worker_id = new_worker_id;
            ownership.own_lease_renewed_at = Instant::now();
            ownership.worker_ids.insert(new_worker_id);
        }

        Ok(())
    }

    /// The worker to file a new job under. If its lease hasn't been renewed lately,
    /// say after a long pause, renews it first, so a job is never filed under a
    /// worker another process has already reclaimed.
    async fn own_worker_for_new_job(&self) -> Result<WorkerId, BoxError> {
        let (worker_id, renewed_at) = {
            let ownership = self.ownership.lock().unwrap();
            (ownership.own_worker_id, ownership.own_lease_renewed_at)
        };

        if renewed_at.elapsed() < WORKER_LEASE_TIMEOUT - HEARTBEAT_INTERVAL {
            return Ok(worker_id);
        }

        if self.renew_lease(worker_id, now_ms()).await? {
            self.ownership.lock().unwrap().own_lease_renewed_at = Instant::now();
        } else {
            self.lose_worker(worker_id).await?;
        }

        Ok(self.ownership.lock().unwrap().own_worker_id)
    }

    /// Reclaims every worker whose lease has gone stale, returning their unfinished jobs.
    async fn reclaim_stale_workers(&self) -> Result<Vec<DurableJob>, BoxError> {
        if self.ownership.lock().unwrap().released {
            return Ok(Vec::new());
        }

        let stale_cutoff = now_ms() - WORKER_LEASE_TIMEOUT.as_millis() as i64;
        let mut rows = self
            .session
            .execute_iter(self.statements.scan_workers.clone(), ())
            .await?
            .rows_stream::<WorkerRow>()?;

        let mut stale = Vec::new();
        while let Some(row) = rows.next().await {
            let (worker_id, owner_id, last_heartbeat_at, oldest_job_bucket) = row?;
            let (Some(owner_id), Some(last_heartbeat_at)) = (owner_id, last_heartbeat_at) else {
                continue;
            };

            if owner_id != self.owner_id && last_heartbeat_at.0 < stale_cutoff {
                let oldest_job_bucket = oldest_job_bucket.unwrap_or(last_heartbeat_at.0 / HOUR_MS);
                stale.push((worker_id, owner_id, last_heartbeat_at, oldest_job_bucket));
            }
        }

        let mut jobs = Vec::new();
        for (worker_id, previous_owner_id, last_heartbeat_at, oldest_job_bucket) in stale {
            let reclaimed = self
                .reclaim_worker(
                    worker_id,
                    previous_owner_id,
                    last_heartbeat_at,
                    oldest_job_bucket,
                )
                .await?;
            jobs.extend(reclaimed);
        }

        Ok(jobs)
    }

    /// Takes ownership of `worker_id`, provided `previous_owner_id` still owns it and
    /// hasn't renewed since `last_heartbeat_at`, then loads its unfinished jobs.
    async fn reclaim_worker(
        &self,
        worker_id: WorkerId,
        previous_owner_id: OwnerId,
        last_heartbeat_at: CqlTimestamp,
        oldest_job_bucket: HourBucket,
    ) -> Result<Vec<DurableJob>, BoxError> {
        let now = now_ms();
        let values = (
            self.owner_id,
            CqlTimestamp(now),
            worker_id,
            previous_owner_id,
            last_heartbeat_at,
        );
        if !self
            .execute_conditional(&self.statements.reclaim_worker, values)
            .await?
        {
            return Ok(Vec::new());
        }

        self.ownership.lock().unwrap().worker_ids.insert(worker_id);

        // Up to now, not to the last heartbeat: it may have filed jobs since.
        let mut jobs = Vec::new();
        for hour_bucket in oldest_job_bucket..=now / HOUR_MS {
            let location = JobLocation {
                worker_id,
                hour_bucket,
            };
            for job in self.scan_job_bucket(location).await? {
                self.ownership.lock().unwrap().jobs.insert(job.id, location);
                jobs.push(job);
            }
        }

        if jobs.is_empty() {
            self.ownership.lock().unwrap().worker_ids.remove(&worker_id);
            self.delete_worker(worker_id).await?;
        }

        Ok(jobs)
    }

    /// The pending jobs filed at `location`.
    async fn scan_job_bucket(&self, location: JobLocation) -> Result<Vec<DurableJob>, BoxError> {
        let mut rows = self
            .session
            .execute_iter(
                self.statements.scan_job_bucket.clone(),
                (location.worker_id, location.hour_bucket),
            )
            .await?
            .rows_stream::<JobRow>()?;

        let mut jobs = Vec::new();
        while let Some(row) = rows.next().await {
            let row = row?;
            match row.6.map(JobStatus::try_from) {
                Some(Ok(JobStatus::Pending)) => jobs.extend(job_from_row(row)),
                Some(Ok(_)) | None => {}
                Some(Err(status)) => {
                    tracing::warn!("job {} has unknown status {status}; skipping it", row.0);
                }
            }
        }

        Ok(jobs)
    }
}

/// worker_id, owner_id, last_heartbeat_at, oldest_job_bucket
type WorkerRow = (
    WorkerId,
    Option<OwnerId>,
    Option<CqlTimestamp>,
    Option<HourBucket>,
);

/// job_id, handler_name, payload, priority, max_retries, enqueued_at, status
type JobRow = (
    JobId,
    Option<String>,
    Option<Vec<u8>>,
    Option<i8>,
    Option<i16>,
    Option<CqlTimestamp>,
    Option<i8>,
);

/// The job in `row`, unless part of it has expired.
fn job_from_row(
    (job_id, name, payload, priority, retries, enqueued_at, _): JobRow,
) -> Option<DurableJob> {
    let priority = match priority? {
        2 => Priority::High,
        1 => Priority::Medium,
        _ => Priority::Low,
    };

    Some(DurableJob {
        id: job_id,
        handler_name: name?,
        payload: payload?,
        priority,
        max_retries: retries?.clamp(0, u8::MAX.into()) as u8,
        enqueued_at: UNIX_EPOCH + Duration::from_millis(enqueued_at?.0.max(0) as u64),
    })
}

fn now_ms() -> i64 {
    millis(SystemTime::now())
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

async fn heartbeat_loop(inner: Weak<Inner>) {
    loop {
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        let Some(inner) = inner.upgrade() else {
            return;
        };

        if let Err(err) = inner.renew_leases().await {
            tracing::error!("failed to renew worker leases: {err}");
        }
    }
}

#[async_trait]
impl Store for ScyllaStore {
    async fn save(&self, record: &DurableJob) -> Result<(), BoxError> {
        let inner = &self.inner;
        let location = JobLocation {
            worker_id: inner.own_worker_for_new_job().await?,
            hour_bucket: now_ms() / HOUR_MS,
        };
        let values = (
            location.worker_id,
            location.hour_bucket,
            record.id,
            record.handler_name.as_str(),
            record.payload.as_slice(),
            record.priority as i8,
            i16::from(record.max_retries),
            CqlTimestamp(millis(record.enqueued_at)),
        );
        inner
            .session
            .execute_unpaged(&inner.statements.insert_job, values)
            .await?;

        inner
            .ownership
            .lock()
            .unwrap()
            .jobs
            .insert(record.id, location);

        Ok(())
    }

    async fn finish(&self, job_id: JobId) -> Result<(), BoxError> {
        let inner = &self.inner;
        // Reclaimed by another process, which now records how it ends.
        let Some(location) = inner.job_location(job_id) else {
            return Ok(());
        };
        inner
            .update_job_status(location, job_id, JobStatus::Done)
            .await?;
        inner.untrack_job(job_id, location).await
    }

    async fn fail(&self, job_id: JobId, error: &str) -> Result<(), BoxError> {
        let inner = &self.inner;
        // Reclaimed by another process, which now records how it ends.
        let Some(location) = inner.job_location(job_id) else {
            return Ok(());
        };

        let job = inner
            .session
            .execute_unpaged(
                &inner.statements.load_job,
                (location.worker_id, location.hour_bucket, job_id),
            )
            .await?
            .into_rows_result()?
            .maybe_first_row::<JobRow>()?;
        if let Some((_, name, payload, priority, retries, enqueued_at, _)) = job {
            let now = now_ms();
            let values = (
                now / DAY_MS,
                CqlTimestamp(now),
                job_id,
                name,
                payload,
                priority,
                retries,
                enqueued_at,
                error,
            );
            inner
                .session
                .execute_unpaged(&inner.statements.insert_failed_job, values)
                .await?;
        }

        inner
            .update_job_status(location, job_id, JobStatus::Failed)
            .await?;
        inner.untrack_job(job_id, location).await
    }

    async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError> {
        self.inner.reclaim_stale_workers().await
    }

    fn is_owned(&self, job_id: JobId) -> bool {
        self.inner
            .ownership
            .lock()
            .unwrap()
            .jobs
            .contains_key(&job_id)
    }

    async fn release(&self) -> Result<(), BoxError> {
        let inner = &self.inner;
        let current = now_ms() / HOUR_MS;
        let (workers_with_jobs, idle_workers): (Vec<_>, Vec<_>) = {
            let mut ownership = inner.ownership.lock().unwrap();
            ownership.released = true;
            let worker_ids: Vec<WorkerId> = ownership.worker_ids.drain().collect();
            worker_ids
                .into_iter()
                .map(|worker_id| (worker_id, ownership.oldest_job_bucket(worker_id, current)))
                .partition(|(worker_id, _)| ownership.has_unfinished_jobs(*worker_id))
        };

        for (worker_id, _) in idle_workers {
            inner.delete_worker(worker_id).await?;
        }

        // A heartbeat at the epoch makes the lease stale at once.
        for (worker_id, oldest_job_bucket) in workers_with_jobs {
            let values = (
                CqlTimestamp(0),
                oldest_job_bucket,
                worker_id,
                inner.owner_id,
            );
            inner
                .execute_conditional(&inner.statements.renew_worker_lease, values)
                .await?;
        }

        Ok(())
    }
}
