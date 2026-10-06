use crate::{BoxError, Record, Store};
use async_trait::async_trait;
use moka::future::Cache;
use moka::ops::compute::Op;

/// Keeps jobs in memory. For tests and apps that never persist.
///
/// Everything in it belongs to the one process holding it, so [`claim`](Store::claim)
/// only returns jobs after a [`release`](Store::release).
pub struct MokaStore {
    // Unbounded: an eviction would silently drop a pending job.
    records: Cache<u128, Entry>,
}

#[derive(Clone)]
struct Entry {
    record: Record,
    owned: bool,
}

impl Default for MokaStore {
    fn default() -> Self {
        Self {
            records: Cache::builder().build(),
        }
    }
}

impl MokaStore {
    /// Sets `owned` on the job `id`, if it's still there.
    async fn set_owned(&self, id: u128, owned: bool) {
        self.records
            .entry(id)
            .and_compute_with(|entry| async move {
                match entry {
                    Some(entry) => Op::Put(Entry {
                        owned,
                        ..entry.into_value()
                    }),
                    None => Op::Nop,
                }
            })
            .await;
    }
}

#[async_trait]
impl Store for MokaStore {
    async fn save(&self, record: &Record) -> Result<(), BoxError> {
        let entry = Entry {
            record: record.clone(),
            owned: true,
        };
        self.records.insert(record.id, entry).await;
        Ok(())
    }

    async fn finish(&self, id: u128) -> Result<(), BoxError> {
        self.records.invalidate(&id).await;
        Ok(())
    }

    async fn fail(&self, id: u128, _error: &str) -> Result<(), BoxError> {
        self.records.invalidate(&id).await;
        Ok(())
    }

    async fn claim(&self) -> Result<Vec<Record>, BoxError> {
        let released: Vec<Record> = self
            .records
            .iter()
            .filter(|(_, entry)| !entry.owned)
            .map(|(_, entry)| entry.record)
            .collect();
        for record in &released {
            self.set_owned(record.id, true).await;
        }
        Ok(released)
    }

    async fn release(&self) -> Result<(), BoxError> {
        let owned: Vec<u128> = self
            .records
            .iter()
            .filter(|(_, entry)| entry.owned)
            .map(|(id, _)| *id)
            .collect();
        for id in owned {
            self.set_owned(id, false).await;
        }
        Ok(())
    }
}
