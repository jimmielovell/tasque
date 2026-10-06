use crate::{BoxError, Record, Store};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Keeps jobs in memory. For tests and apps that never persist.
#[derive(Default)]
pub struct MemoryStore {
    records: Mutex<HashMap<u128, Record>>,
}

#[async_trait]
impl Store for MemoryStore {
    async fn save(&self, record: &Record) -> Result<(), BoxError> {
        self.records
            .lock()
            .unwrap()
            .insert(record.id, record.clone());
        Ok(())
    }

    async fn finish(&self, id: u128) -> Result<(), BoxError> {
        self.records.lock().unwrap().remove(&id);
        Ok(())
    }

    async fn pending(&self) -> Result<Vec<Record>, BoxError> {
        Ok(self.records.lock().unwrap().values().cloned().collect())
    }
}
