use crate::{BoxError, Record, Store};
use async_trait::async_trait;
use moka::future::Cache;

/// Keeps jobs in memory. For tests and apps that never persist.
pub struct MemoryStore {
    // Unbounded: an eviction would silently drop a pending job.
    records: Cache<u128, Record>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            records: Cache::builder().build(),
        }
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn save(&self, record: &Record) -> Result<(), BoxError> {
        self.records.insert(record.id, record.clone()).await;
        Ok(())
    }

    async fn finish(&self, id: u128) -> Result<(), BoxError> {
        self.records.invalidate(&id).await;
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<Record>, BoxError> {
        Ok(self.records.iter().map(|(_, record)| record).collect())
    }
}
