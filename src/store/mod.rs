#[cfg(feature = "moka-store")]
mod moka;
#[cfg(feature = "scylla-store")]
mod scylla;

use crate::{BoxError, Priority};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

#[cfg(feature = "moka-store")]
pub use moka::MokaStore;
#[cfg(feature = "scylla-store")]
pub use scylla::{ScyllaStore, ScyllaStoreBuilder};

/// A persisted job.
#[derive(Clone, Debug)]
pub struct Record {
    pub id: u128,
    /// Its handler's name.
    pub name: String,
    /// The job, as bincode, or JSON with the `json` feature.
    pub payload: Vec<u8>,
    pub priority: Priority,
    pub retries: u8,
    pub enqueued_at: SystemTime,
}

/// Keeps persisted jobs until they finish.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Saves a job before it runs.
    async fn save(&self, record: &Record) -> Result<(), BoxError>;

    /// Marks a job finished.
    async fn finish(&self, id: u128) -> Result<(), BoxError>;

    /// Marks a job failed: it ran out of retries.
    async fn fail(&self, id: u128, error: &str) -> Result<(), BoxError>;

    /// Takes over unfinished jobs whose process has stopped. Called periodically.
    async fn claim(&self) -> Result<Vec<Record>, BoxError>;

    /// Lets other processes claim this one's unfinished jobs straight away.
    async fn release(&self) -> Result<(), BoxError>;

    /// Whether this process still owns job `id`. Checked before each attempt, so a
    /// job another process has claimed isn't run here too.
    fn owns(&self, _id: u128) -> bool {
        true
    }
}

#[async_trait]
impl<T: Store + ?Sized> Store for Arc<T> {
    async fn save(&self, record: &Record) -> Result<(), BoxError> {
        (**self).save(record).await
    }

    async fn finish(&self, id: u128) -> Result<(), BoxError> {
        (**self).finish(id).await
    }

    async fn fail(&self, id: u128, error: &str) -> Result<(), BoxError> {
        (**self).fail(id, error).await
    }

    async fn claim(&self) -> Result<Vec<Record>, BoxError> {
        (**self).claim().await
    }

    async fn release(&self) -> Result<(), BoxError> {
        (**self).release().await
    }

    fn owns(&self, id: u128) -> bool {
        (**self).owns(id)
    }
}
