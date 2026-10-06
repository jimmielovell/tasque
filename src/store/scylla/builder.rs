use super::{
    HEARTBEAT_INTERVAL, Inner, JOB_TTL, Ownership, ScyllaStore, Statements, WORKER_LEASE_TIMEOUT,
    heartbeat_loop,
};
use crate::{BoxError, Error};
use scylla::client::session::Session;
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::{Consistency, SerialConsistency};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone)]
enum Replication {
    Simple(u8),
    NetworkTopology(HashMap<String, u8>),
}

impl Replication {
    fn as_cql(&self) -> String {
        match self {
            Self::Simple(rf) => {
                format!("{{'class': 'SimpleStrategy', 'replication_factor': {rf}}}")
            }
            Self::NetworkTopology(dcs) => {
                let dcs: String = dcs
                    .iter()
                    .map(|(dc, rf)| format!(", '{dc}': {rf}"))
                    .collect();
                format!("{{'class': 'NetworkTopologyStrategy'{dcs}}}")
            }
        }
    }
}

fn validate_identifier(name: &str) -> Result<(), Error> {
    let mut chars = name.chars();
    let valid = name.len() <= 48
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::Store(
            format!(
                "invalid identifier {name:?}: 1-48 ASCII letters, digits or underscores, \
                 not starting with a digit"
            )
            .into(),
        ))
    }
}

/// Builds a [`ScyllaStore`].
///
/// Its tables are `{prefix}_jobs`, `{prefix}_workers` and `{prefix}_failed` in
/// `keyspace`. With [`create_tables`](Self::create_tables), `build` creates them and the
/// keyspace if missing; otherwise they're expected to exist, for example from a
/// migration.
#[derive(Debug)]
pub struct ScyllaStoreBuilder {
    session: Arc<Session>,
    keyspace: String,
    prefix: String,
    replication: Replication,
    create_tables: bool,
}

impl ScyllaStoreBuilder {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            keyspace: "tasque".to_string(),
            prefix: "t_tasque".to_string(),
            replication: Replication::Simple(1),
            create_tables: false,
        }
    }

    pub fn keyspace_name(mut self, name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        validate_identifier(&name)?;
        self.keyspace = name;
        Ok(self)
    }

    /// Sets the prefix of the table names. The longest name it makes, `{prefix}_workers`,
    /// must still be a valid identifier.
    pub fn table_prefix(mut self, prefix: impl Into<String>) -> Result<Self, Error> {
        let prefix = prefix.into();
        validate_identifier(&prefix)?;
        validate_identifier(&format!("{prefix}_workers"))?;
        self.prefix = prefix;
        Ok(self)
    }

    pub fn simple_strategy(mut self, replication_factor: u8) -> Self {
        self.replication = Replication::Simple(replication_factor);
        self
    }

    pub fn network_topology_strategy(
        mut self,
        datacenter: impl Into<String>,
        replication_factor: u8,
    ) -> Self {
        match &mut self.replication {
            Replication::NetworkTopology(dcs) => {
                dcs.insert(datacenter.into(), replication_factor);
            }
            replication => {
                *replication = Replication::NetworkTopology(HashMap::from([(
                    datacenter.into(),
                    replication_factor,
                )]));
            }
        }
        self
    }

    pub fn create_tables(mut self, create: bool) -> Self {
        self.create_tables = create;
        self
    }

    pub async fn build(self) -> Result<ScyllaStore, Error> {
        self.try_build().await.map_err(Error::Store)
    }

    async fn try_build(self) -> Result<ScyllaStore, BoxError> {
        let session = self.session;
        let ks = &self.keyspace;
        let jobs = format!("{ks}.{}_jobs", self.prefix);
        let workers = format!("{ks}.{}_workers", self.prefix);
        let failed = format!("{ks}.{}_failed", self.prefix);

        if self.create_tables {
            let schema = [
                format!(
                    "create keyspace if not exists {ks} with replication = {}",
                    self.replication.as_cql()
                ),
                format!(
                    r#"
                    create table if not exists {jobs} (
                        worker_id uuid,
                        hour_bucket bigint,
                        job_id uuid,
                        name text,
                        payload blob,
                        priority tinyint,
                        retries smallint,
                        enqueued_at timestamp,
                        status tinyint,
                        primary key ((worker_id, hour_bucket), job_id)
                    ) with compaction = {{
                        'class': 'TimeWindowCompactionStrategy',
                        'compaction_window_unit': 'HOURS',
                        'compaction_window_size': 6
                    }}
                    "#
                ),
                format!(
                    r#"
                    create table if not exists {workers} (
                        worker_id uuid primary key,
                        owner_id uuid,
                        last_heartbeat_at timestamp,
                        oldest_job_bucket bigint
                    )
                    "#
                ),
                format!(
                    r#"
                    create table if not exists {failed} (
                        day bigint,
                        failed_at timestamp,
                        job_id uuid,
                        name text,
                        payload blob,
                        priority tinyint,
                        retries smallint,
                        enqueued_at timestamp,
                        error text,
                        primary key (day, failed_at, job_id)
                    ) with clustering order by (failed_at desc, job_id asc)
                    "#
                ),
            ];

            for cql in schema {
                session.query_unpaged(cql, ()).await?;
            }

            session.await_schema_agreement().await?;
        }

        let ttl = JOB_TTL.as_secs();
        let prepare = |cql: String, conditional: bool| {
            let session = session.clone();

            async move {
                let mut statement: PreparedStatement = session.prepare(cql).await?;
                statement.set_consistency(Consistency::LocalQuorum);
                if conditional {
                    statement.set_serial_consistency(Some(SerialConsistency::LocalSerial));
                }

                Ok::<_, BoxError>(statement)
            }
        };

        let job_columns = "job_id, name, payload, priority, retries, enqueued_at, status";
        let statements = Statements {
            insert_job: prepare(
                format!(
                    r#"
                    insert into {jobs}
                        (worker_id, hour_bucket, job_id, name, payload, priority,  retries, enqueued_at, status)
                    values (?, ?, ?, ?, ?, ?, ?, ?, 0) using ttl {ttl}
                    "#
                ),
                false,
            )
            .await?,
            update_job_status: prepare(
                format!(
                    r#"
                    update {jobs} using ttl {ttl} set status = ?
                    where worker_id = ?
                        and hour_bucket = ?
                        and job_id = ?
                    "#
                ),
                false,
            )
            .await?,
            load_job: prepare(
                format!(
                    r#"
                    select {job_columns} from {jobs}
                    where worker_id = ?
                        and hour_bucket = ?
                        and job_id = ?
                    "#
                ),
                false,
            )
            .await?,
            scan_job_bucket: prepare(
                format!(
                    "select {job_columns} from {jobs} where worker_id = ? and hour_bucket = ?"
                ),
                false,
            )
            .await?,
            insert_failed_job: prepare(
                format!(
                    r#"
                    insert into {failed}
                        (day, failed_at, job_id, name, payload, priority, retries, enqueued_at, error)
                    values (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#
                ),
                false,
            )
            .await?,
            register_worker: prepare(
                format!(
                    r#"
                    insert into {workers}
                        (worker_id, owner_id, last_heartbeat_at, oldest_job_bucket)
                    values (?, ?, ?, ?) if not exists
                    "#
                ),
                true,
            )
            .await?,
            scan_workers: prepare(
                format!(
                    "select worker_id, owner_id, last_heartbeat_at, oldest_job_bucket from {workers}"
                ),
                false,
            )
            .await?,
            renew_worker_lease: prepare(
                format!(
                    r#"
                    update {workers} set last_heartbeat_at = ?, oldest_job_bucket = ?
                    where worker_id = ? if owner_id = ?
                    "#
                ),
                true,
            )
            .await?,
            reclaim_worker: prepare(
                format!(
                    r#"
                    update {workers} set owner_id = ?, last_heartbeat_at = ?
                    where worker_id = ? if owner_id = ? and last_heartbeat_at = ?
                    "#
                ),
                true,
            )
            .await?,
            delete_worker: prepare(
                format!("delete from {workers} where worker_id = ? if owner_id = ?"),
                true,
            )
            .await?,
        };

        let owner_id = uuid::Builder::from_random_bytes(rand::random()).into_uuid();
        let mut inner = Inner {
            session,
            owner_id,
            statements,
            ownership: Mutex::new(Ownership {
                own_worker_id: Uuid::nil(),
                own_lease_renewed_at: Instant::now(),
                worker_ids: HashSet::new(),
                jobs: HashMap::new(),
                released: false,
            }),
        };
        let own_worker_id = inner.register_worker().await?;
        let ownership = inner.ownership.get_mut().unwrap();
        ownership.own_worker_id = own_worker_id;
        ownership.worker_ids.insert(own_worker_id);

        debug_assert!(HEARTBEAT_INTERVAL * 3 <= WORKER_LEASE_TIMEOUT);
        let inner = Arc::new(inner);
        let heartbeat_task = tokio::spawn(heartbeat_loop(Arc::downgrade(&inner))).abort_handle();

        Ok(ScyllaStore {
            inner,
            heartbeat_task,
        })
    }
}
