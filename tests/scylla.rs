//! Needs a running ScyllaDB at `SCYLLA_URI` (default `127.0.0.1:9042`).

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tasque::{BoxError, DurableJob, JobId, Priority, ScyllaStore, ScyllaStoreBuilder, Store, Tasque};
use tokio::time::sleep;

const KEYSPACE: &str = "tasque_test";

async fn session() -> Arc<Session> {
    let uri = std::env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    Arc::new(SessionBuilder::new().known_node(uri).build().await.unwrap())
}

/// Drops and recreates the tables under `prefix`, then builds a store on them.
async fn fresh(session: &Arc<Session>, prefix: &str) -> ScyllaStore {
    for table in ["jobs", "workers", "failed"] {
        session
            .query_unpaged(
                format!("drop table if exists {KEYSPACE}.{prefix}_{table}"),
                (),
            )
            .await
            .unwrap();
    }
    builder(session, prefix)
        .create_tables(true)
        .build()
        .await
        .unwrap()
}

/// Another process on the same tables.
async fn another(session: &Arc<Session>, prefix: &str) -> ScyllaStore {
    builder(session, prefix).build().await.unwrap()
}

fn builder(session: &Arc<Session>, prefix: &str) -> ScyllaStoreBuilder {
    ScyllaStoreBuilder::new(session.clone())
        .keyspace_name(KEYSPACE)
        .unwrap()
        .table_prefix(prefix)
        .unwrap()
}

fn record(name: &str) -> DurableJob {
    DurableJob {
        id: JobId::now_v7(),
        handler_name: name.into(),
        payload: name.as_bytes().to_vec(),
        priority: Priority::Medium,
        max_retries: 200,
        enqueued_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
    }
}

async fn workers(session: &Session, prefix: &str) -> i64 {
    session
        .query_unpaged(
            format!("select count(*) from {KEYSPACE}.{prefix}_workers"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .first_row::<(i64,)>()
        .unwrap()
        .0
}

#[tokio::test(flavor = "multi_thread")]
async fn released_jobs_are_claimed_by_another_process() {
    let session = session().await;
    let a = fresh(&session, "t_released").await;
    let b = another(&session, "t_released").await;

    let (done, pending) = (record("done"), record("pending"));
    a.save(&done).await.unwrap();
    a.save(&pending).await.unwrap();
    a.finish(done.id).await.unwrap();

    // `a` is alive, so nothing is claimable yet.
    assert!(b.reclaim_stale().await.unwrap().is_empty());

    a.release().await.unwrap();
    let claimed = b.reclaim_stale().await.unwrap();
    assert_eq!(claimed.len(), 1);
    let job = &claimed[0];
    assert_eq!((job.id, job.handler_name.as_str()), (pending.id, "pending"));
    assert_eq!(job.payload, pending.payload);
    assert_eq!((job.priority, job.max_retries), (Priority::Medium, 200));
    assert_eq!(job.enqueued_at, pending.enqueued_at);

    assert!(b.reclaim_stale().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_process_is_claimed_once_stale_and_only_once() {
    let session = session().await;
    let a = fresh(&session, "t_silent").await;
    a.save(&record("orphan")).await.unwrap();
    drop(a); // stops checking in, like a crash

    let b = another(&session, "t_silent").await;
    let c = another(&session, "t_silent").await;
    assert!(b.reclaim_stale().await.unwrap().is_empty());

    sleep(Duration::from_secs(16)).await;
    let (b_claimed, c_claimed) = tokio::join!(b.reclaim_stale(), c.reclaim_stale());
    let names: Vec<String> = b_claimed
        .unwrap()
        .into_iter()
        .chain(c_claimed.unwrap())
        .map(|r| r.handler_name)
        .collect();
    assert_eq!(names, ["orphan"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_job_is_recorded_and_not_claimed() {
    let session = session().await;
    let a = fresh(&session, "t_failed").await;
    let job = record("doomed");
    a.save(&job).await.unwrap();
    a.fail(job.id, "mail server down").await.unwrap();

    let failed: Vec<(String, String)> = session
        .query_unpaged(
            format!("select name, error from {KEYSPACE}.t_failed_failed"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String, String)>()
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        failed,
        [("doomed".to_string(), "mail server down".to_string())]
    );

    a.release().await.unwrap();
    let b = another(&session, "t_failed").await;
    assert!(b.reclaim_stale().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn drained_workers_are_removed() {
    let session = session().await;
    let a = fresh(&session, "t_drained").await;
    let job = record("handed_over");
    a.save(&job).await.unwrap();
    a.release().await.unwrap();

    let b = another(&session, "t_drained").await;
    assert_eq!(b.reclaim_stale().await.unwrap().len(), 1);
    b.finish(job.id).await.unwrap();
    // Only `b`'s own worker is left.
    assert_eq!(workers(&session, "t_drained").await, 1);

    b.release().await.unwrap();
    assert_eq!(workers(&session, "t_drained").await, 0);
}

#[derive(Clone, Serialize, Deserialize)]
struct Email {
    address: String,
}

async fn mailer(
    store: ScyllaStore,
    fail: bool,
) -> (Tasque, Arc<Mutex<Vec<String>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let t = Tasque::new(store)
        .add("email", sent.clone(), move |ctx, Email { address }| async move {
            if fail {
                return Err::<(), BoxError>("mail server down".into());
            }
            ctx.lock().unwrap().push(address);
            Ok(())
        })
        .build()
        .unwrap();
    t.run().await.unwrap();
    (t, sent)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_left_by_one_tasque_runs_on_another() {
    let session = session().await;
    let (before, _) = mailer(fresh(&session, "t_tasque").await, true).await;
    let job = Email {
        address: "a@example.com".into(),
    };
    before
        .queue(job, Priority::High, Some(5), true)
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;
    before.shutdown().await.unwrap();

    let (after, sent) = mailer(another(&session, "t_tasque").await, false).await;
    sleep(Duration::from_millis(500)).await;
    assert_eq!(*sent.lock().unwrap(), ["a@example.com"]);

    after.shutdown().await.unwrap();
    assert_eq!(workers(&session, "t_tasque").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_process_whose_worker_was_reclaimed_lets_go_of_its_jobs_and_moves_to_a_new_worker() {
    let session = session().await;
    let a = fresh(&session, "t_lost").await;
    let first = record("first");
    a.save(&first).await.unwrap();
    assert!(a.is_owned(first.id));

    // Expire `a`'s lease, as if `a` had paused for too long.
    let (worker_id, owner_id) = session
        .query_unpaged(
            format!("select worker_id, owner_id from {KEYSPACE}.t_lost_workers"),
            (),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .first_row::<(uuid::Uuid, uuid::Uuid)>()
        .unwrap();
    session
        .query_unpaged(
            format!(
                "update {KEYSPACE}.t_lost_workers set last_heartbeat_at = 0
                    where worker_id = ? if owner_id = ?"
            ),
            (worker_id, owner_id),
        )
        .await
        .unwrap();

    let b = another(&session, "t_lost").await;
    let reclaimed: Vec<String> = b
        .reclaim_stale()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.handler_name)
        .collect();
    assert_eq!(reclaimed, ["first"]);
    b.finish(first.id).await.unwrap();

    // `a` notices at its next heartbeat, lets go of the job and moves to a new worker.
    sleep(Duration::from_secs(6)).await;
    assert!(!a.is_owned(first.id));
    a.finish(first.id).await.unwrap();
    a.save(&record("second")).await.unwrap();
    a.release().await.unwrap();
    let reclaimed: Vec<String> = b
        .reclaim_stale()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.handler_name)
        .collect();
    assert_eq!(reclaimed, ["second"]);
}
