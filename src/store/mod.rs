mod memory;

use crate::{BoxError, Priority};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;

pub use memory::MemoryStore;

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

/// Keeps persisted jobs until they finish. [`Builder::run`](crate::Builder::run)
/// replays anything unfinished, so jobs run at least once.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Saves a job before it runs.
    async fn save(&self, record: &Record) -> Result<(), BoxError>;

    /// Marks a job finished, whether it succeeded or gave up.
    async fn finish(&self, id: u128) -> Result<(), BoxError>;

    /// Every unfinished job.
    async fn pending(&self) -> Result<Vec<Record>, BoxError>;
}

#[async_trait]
impl<T: Store + ?Sized> Store for Arc<T> {
    async fn save(&self, record: &Record) -> Result<(), BoxError> {
        (**self).save(record).await
    }

    async fn finish(&self, id: u128) -> Result<(), BoxError> {
        (**self).finish(id).await
    }

    async fn pending(&self) -> Result<Vec<Record>, BoxError> {
        (**self).pending().await
    }
}
