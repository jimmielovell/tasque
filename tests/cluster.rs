//! Several processes sharing one ScyllaDB while they crash, pause, stop and retry.
//!
//! Each process gets its own tokio runtime and Scylla session, so they share nothing
//! but the database. Needs ScyllaDB at `SCYLLA_URI` (default `127.0.0.1:9042`), and
//! takes about a minute and a half: noticing a crash means waiting out the 15s lease.

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Barrier, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tasque::{Priority, ScyllaStoreBuilder, Step, Tasque};
use tokio::runtime::Runtime;

const KEYSPACE: &str = "tasque_test";
const PREFIX: &str = "t_cluster";
const RETRIES: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Kind {
    /// Done in 300ms.
    Quick,
    /// Done in 3s, so crashes and stops catch it running.
    Slow,
    /// Fails its first two attempts.
    Flaky,
    /// Always fails, so it should end up in the failed table.
    Broken,
    /// Hands off to a `FollowUp`.
    Chain,
}

fn kind_of(id: u32) -> Kind {
    match id % 5 {
        0 => Kind::Quick,
        1 => Kind::Slow,
        2 => Kind::Flaky,
        3 => Kind::Broken,
        _ => Kind::Chain,
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Job {
    id: u32,
    kind: Kind,
}

#[derive(Clone, Serialize, Deserialize)]
struct FollowUp {
    id: u32,
}

/// What every process did, shared by the test only.
#[derive(Default)]
struct Ledger {
    /// Every attempt: job id, process, attempt number.
    runs: Mutex<Vec<(u32, &'static str, u8)>>,
    /// How many times each job completed.
    completed: Mutex<HashMap<u32, u32>>,
}

impl Ledger {
    fn complete(&self, id: u32) {
        *self.completed.lock().unwrap().entry(id).or_default() += 1;
    }

    fn ran_on(&self, ids: impl Fn(u32) -> bool, process: &str) -> bool {
        let runs = self.runs.lock().unwrap();
        runs.iter().any(|(id, p, _)| ids(*id) && *p == process)
    }
}

struct Ctx {
    process: &'static str,
    ledger: Arc<Ledger>,
}

async fn session() -> Arc<Session> {
    let uri = std::env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    Arc::new(SessionBuilder::new().known_node(uri).build().await.unwrap())
}

/// One simulated process.
struct Process {
    name: &'static str,
    runtime: Runtime,
    tasque: Option<Tasque<Ctx>>,
}

impl Process {
    fn start(name: &'static str, ledger: &Arc<Ledger>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ledger = ledger.clone();
        let tasque = runtime.block_on(async move {
            let store = ScyllaStoreBuilder::new(session().await)
                .keyspace_name(KEYSPACE)
                .unwrap()
                .table_prefix(PREFIX)
                .unwrap()
                .create_tables(true)
                .build()
                .await
                .unwrap();
            Tasque::new(
                store,
                Ctx {
                    process: name,
                    ledger,
                },
            )
            .add("job", |ctx, Job { id, kind }| async move {
                let attempt = ctx.attempt();
                ctx.ledger
                    .runs
                    .lock()
                    .unwrap()
                    .push((id, ctx.process, attempt));
                let ms = if kind == Kind::Slow { 3000 } else { 300 };
                tokio::time::sleep(Duration::from_millis(ms)).await;
                match kind {
                    Kind::Broken => Err("broken".into()),
                    Kind::Flaky if attempt < 2 => Err("flaky".into()),
                    Kind::Chain => Ok(Step::next(FollowUp { id })),
                    _ => {
                        ctx.ledger.complete(id);
                        Ok(Step::done())
                    }
                }
            })
            .add("follow_up", |ctx, FollowUp { id }| async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                ctx.ledger.complete(id);
                Ok(())
            })
            .run()
            .await
            .unwrap()
        });
        println!("{name}: started");
        Self {
            name,
            runtime,
            tasque: Some(tasque),
        }
    }

    fn queue(&self, id: u32) {
        let tasque = self.tasque.as_ref().unwrap();
        let job = Job {
            id,
            kind: kind_of(id),
        };
        self.runtime
            .block_on(tasque.queue(job, Priority::Medium, Some(RETRIES), true))
            .unwrap();
    }

    /// Like `kill -9`: every task stops where it is.
    fn crash(mut self) {
        println!("{}: crashed", self.name);
        self.runtime.shutdown_background();
        // Nothing else of the process may run, not even its destructors.
        std::mem::forget(self.tasque.take());
    }

    /// Like a long GC pause or a frozen VM: every runtime thread blocks, so the
    /// process misses its heartbeats, then carries on where it was.
    fn pause(&self, duration: Duration) {
        println!("{}: paused for {duration:?}", self.name);
        let barrier = Arc::new(Barrier::new(2));
        for _ in 0..2 {
            let barrier = barrier.clone();
            self.runtime.spawn(async move {
                barrier.wait();
                sleep(duration);
            });
        }
    }

    fn stop(mut self) {
        let tasque = self.tasque.take().unwrap();
        self.runtime.block_on(tasque.shutdown()).unwrap();
        println!("{}: stopped", self.name);
    }
}

/// Queries the shared tables, from outside every process.
struct Admin {
    runtime: Runtime,
    session: Arc<Session>,
}

impl Admin {
    fn new() -> Self {
        let runtime = Runtime::new().unwrap();
        let session = runtime.block_on(session());
        Self { runtime, session }
    }

    fn rows<T>(&self, cql: &str) -> Vec<T>
    where
        T: for<'f, 'm> scylla::deserialize::row::DeserializeRow<'f, 'm>,
    {
        self.runtime.block_on(async {
            self.session
                .query_unpaged(cql, ())
                .await
                .unwrap()
                .into_rows_result()
                .unwrap()
                .rows::<T>()
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
    }

    fn reset(&self) {
        self.runtime.block_on(async {
            for table in ["jobs", "workers", "failed"] {
                let cql = format!("drop table if exists {KEYSPACE}.{PREFIX}_{table}");
                self.session.query_unpaged(cql, ()).await.unwrap();
            }
        });
    }

    fn pending_jobs(&self) -> usize {
        let statuses: Vec<(Option<i8>,)> =
            self.rows(&format!("select status from {KEYSPACE}.{PREFIX}_jobs"));
        statuses.iter().filter(|(s,)| *s == Some(0)).count()
    }

    fn workers(&self) -> usize {
        self.rows::<(uuid::Uuid,)>(&format!(
            "select worker_id from {KEYSPACE}.{PREFIX}_workers"
        ))
        .len()
    }

    /// The ids of the jobs in the failed table.
    fn failed(&self) -> HashSet<u32> {
        let payloads: Vec<(Vec<u8>,)> =
            self.rows(&format!("select payload from {KEYSPACE}.{PREFIX}_failed"));
        payloads.iter().map(|(p,)| decode(p).id).collect()
    }
}

#[cfg(feature = "json")]
fn decode(payload: &[u8]) -> Job {
    serde_json::from_slice(payload).unwrap()
}

#[cfg(not(feature = "json"))]
fn decode(payload: &[u8]) -> Job {
    bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .unwrap()
        .0
}

#[test]
fn processes_that_crash_pause_and_stop_lose_no_jobs() {
    let admin = Admin::new();
    admin.reset();
    let ledger = Arc::new(Ledger::default());
    let began = Instant::now();
    let at = |what: &str| println!("{:>5.1}s {what}", began.elapsed().as_secs_f64());

    let p1 = Process::start("p1", &ledger);
    let p2 = Process::start("p2", &ledger);
    let p3 = Process::start("p3", &ledger);
    for id in 0..60 {
        [&p1, &p2, &p3][id as usize % 3].queue(id);
    }
    at("queued 60 jobs across p1, p2 and p3");

    sleep(Duration::from_secs(1));
    at("crashing p1 with jobs running");
    p1.crash();

    let p4 = Process::start("p4", &ledger);
    at("p4 joined");

    sleep(Duration::from_secs(1));
    at("stopping p2 gracefully");
    p2.stop();

    // Long enough for p4 to reclaim p3 before it wakes.
    p3.pause(Duration::from_secs(30));
    at("paused p3");
    sleep(Duration::from_secs(32));
    at("p3 is awake again; queueing 10 more jobs on it");
    for id in 60..70 {
        p3.queue(id);
    }

    sleep(Duration::from_secs(3));
    at("crashing p4, with what it reclaimed");
    p4.crash();

    // Wait for p3 to reclaim p4 and everything to settle, including p3 removing the
    // workers of the processes that crashed: only its own should be left.
    let all: Vec<u32> = (0..70).collect();
    let broken: HashSet<u32> = all
        .iter()
        .copied()
        .filter(|id| kind_of(*id) == Kind::Broken)
        .collect();
    let should_complete: Vec<u32> = all
        .iter()
        .copied()
        .filter(|id| !broken.contains(id))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let completed = ledger.completed.lock().unwrap().len();
        let failed = admin.failed();
        let pending = admin.pending_jobs();
        let workers = admin.workers();
        let settled = workers == 1
            && should_complete
                .iter()
                .all(|id| ledger.completed.lock().unwrap().contains_key(id))
            && broken.is_subset(&failed)
            && pending == 0;
        if settled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not settled: {completed}/{} completed, {}/{} failed, {pending} pending, \
             {workers} workers",
            should_complete.len(),
            failed.intersection(&broken).count(),
            broken.len(),
        );
        sleep(Duration::from_secs(1));
    }
    at("every job completed or failed");
    p3.stop();

    // Nothing lost: every job that can complete did; every broken one failed.
    let completed = ledger.completed.lock().unwrap().clone();
    for id in &should_complete {
        assert!(completed.contains_key(id), "job {id} never completed");
    }
    for id in &broken {
        assert!(!completed.contains_key(id), "broken job {id} completed");
    }
    assert_eq!(admin.failed(), broken);
    assert_eq!(admin.pending_jobs(), 0);
    assert_eq!(admin.workers(), 0, "worker rows left behind");

    // And it got there the hard way.
    assert!(
        ledger.ran_on(|id| id < 60 && id % 3 == 0, "p4")
            || ledger.ran_on(|id| id < 60 && id % 3 == 0, "p3"),
        "p1's jobs were never reclaimed"
    );
    assert!(
        ledger.ran_on(|id| id < 60 && id % 3 == 2, "p4"),
        "p3's jobs weren't reclaimed while it was paused"
    );

    let runs = ledger.runs.lock().unwrap();
    let mut per_process: HashMap<&str, usize> = HashMap::new();
    for (_, process, _) in runs.iter() {
        *per_process.entry(process).or_default() += 1;
    }
    let mut per_process: Vec<_> = per_process.into_iter().collect();
    per_process.sort();
    let duplicates: Vec<u32> = completed
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(id, _)| *id)
        .collect();
    println!("attempts per process: {per_process:?}");
    println!(
        "{} attempts for 70 jobs; {} completed more than once (allowed by at-least-once): {duplicates:?}",
        runs.len(),
        duplicates.len()
    );
}
