#[cfg(feature = "moka-store")]
mod moka;
#[cfg(feature = "scylla-store")]
mod scylla;

use crate::{BoxError, Priority};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

use crate::tasque::JobId;
#[cfg(feature = "moka-store")]
pub use moka::MokaStore;
#[cfg(feature = "scylla-store")]
pub use scylla::{ScyllaStore, ScyllaStoreBuilder};

/// A persisted job.
#[derive(Clone, Debug)]
pub struct DurableJob {
    pub id: JobId,
    /// Its handler's name.
    pub handler_name: String,
    /// The job, as bincode, or JSON with the `json` feature.
    pub payload: Vec<u8>,
    pub priority: Priority,
    pub max_retries: u8,
    pub enqueued_at: SystemTime,
}

/// Keeps persisted jobs until they finish.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Saves a job before it runs.
    async fn save(&self, job: &DurableJob) -> Result<(), BoxError>;

    /// Marks a job finished.
    async fn finish(&self, job_id: JobId) -> Result<(), BoxError>;

    /// Marks a job failed: it ran out of retries.
    async fn fail(&self, job_id: JobId, error: &str) -> Result<(), BoxError>;

    /// Takes over unfinished jobs whose process has stopped. Called periodically.
    async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError>;

    /// Lets other processes claim this one's unfinished jobs straight away.
    async fn release(&self) -> Result<(), BoxError>;

    /// Returns whether this process currently considers `job_id` owned by it.
    /// Checked before each attempt to avoid running a job after another process
    /// has reclaimed its worker.
    fn is_owned(&self, _job_id: JobId) -> bool {
        true
    }
}

#[async_trait]
impl<T: Store + ?Sized> Store for Arc<T> {
    async fn save(&self, record: &DurableJob) -> Result<(), BoxError> {
        (**self).save(record).await
    }

    async fn finish(&self, job_id: JobId) -> Result<(), BoxError> {
        (**self).finish(job_id).await
    }

    async fn fail(&self, job_id: JobId, error: &str) -> Result<(), BoxError> {
        (**self).fail(job_id, error).await
    }

    async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError> {
        (**self).reclaim_stale().await
    }

    async fn release(&self) -> Result<(), BoxError> {
        (**self).release().await
    }

    fn is_owned(&self, job_id: JobId) -> bool {
        (**self).is_owned(job_id)
    }
}
