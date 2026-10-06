use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tasque::{BoxError, Error, MemoryStore, Priority, Record, Step, Store, Tasque};
use tokio::time::sleep;

#[derive(Clone, Serialize, Deserialize)]
struct Email {
    address: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pdf {
    address: String,
}

/// Like `Pdf`, but asks for its email to be persisted.
#[derive(Clone, Serialize, Deserialize)]
struct Extract {
    address: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Fails;

#[derive(Default)]
struct State {
    pdfs: AtomicU32,
    sent: Mutex<Vec<String>>,
}

/// A `Tasque` on `store` whose emails take `send_secs` to send.
async fn tasque(store: Arc<MemoryStore>, send_secs: u64) -> (Tasque<Arc<State>>, Arc<State>) {
    let state = Arc::new(State::default());
    let t = Tasque::new(store, state.clone())
        .add("email", move |ctx, Email { address }| async move {
            sleep(Duration::from_secs(send_secs)).await;
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .add("pdf", |ctx, Pdf { address }| async move {
            ctx.pdfs.fetch_add(1, Ordering::SeqCst);
            Ok(Step::next(Email { address }))
        })
        .add("extract", |_ctx, Extract { address }| async move {
            Ok(Step::next(Email { address }).persist())
        })
        .add("fails", |_ctx, Fails| async move {
            Err::<(), BoxError>("always".into())
        })
        .run()
        .await
        .unwrap();
    (t, state)
}

fn email(address: &str) -> Email {
    Email {
        address: address.into(),
    }
}

async fn pending(store: &MemoryStore) -> Vec<Record> {
    store.pending().await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_persisted_job_is_kept_until_it_finishes() {
    let store = Arc::new(MemoryStore::default());
    let (t, _) = tasque(store.clone(), 10).await;
    t.queue(email("a@example.com"), Priority::High, Some(5), true)
        .await
        .unwrap();

    let records = pending(&store).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "email");
    #[cfg(feature = "json")]
    let expected = br#"{"address":"a@example.com"}"#.to_vec();
    #[cfg(not(feature = "json"))]
    let expected =
        bincode::serde::encode_to_vec(email("a@example.com"), bincode::config::standard()).unwrap();
    assert_eq!(records[0].payload, expected);
    assert_eq!(records[0].priority, Priority::High);
    assert_eq!(records[0].retries, 5);

    sleep(Duration::from_secs(11)).await;
    assert!(pending(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_not_persisted_never_reaches_the_store() {
    let store = Arc::new(MemoryStore::default());
    let (t, state) = tasque(store.clone(), 10).await;
    t.queue(email("a@example.com"), Priority::High, None, false)
        .await
        .unwrap();

    assert!(pending(&store).await.is_empty());
    sleep(Duration::from_secs(11)).await;
    assert_eq!(*state.sent.lock().unwrap(), ["a@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn unfinished_jobs_run_again_after_a_restart() {
    let store = Arc::new(MemoryStore::default());
    let (before, _) = tasque(store.clone(), 10).await;
    before
        .queue(email("a@example.com"), Priority::Medium, None, true)
        .await
        .unwrap();
    sleep(Duration::from_millis(1)).await;

    // The process stops mid-send; a new one starts on the same store.
    let (_after, state) = tasque(store.clone(), 0).await;
    sleep(Duration::from_millis(1)).await;

    assert_eq!(*state.sent.lock().unwrap(), ["a@example.com"]);
    assert!(pending(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_finished_step_leaves_only_its_next_in_the_store() {
    let store = Arc::new(MemoryStore::default());
    let (before, _) = tasque(store.clone(), 10).await;
    before
        .queue(
            Pdf {
                address: "b@example.com".into(),
            },
            Priority::Medium,
            None,
            true,
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(1)).await;

    let names: Vec<String> = pending(&store).await.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["email"]);

    // After a restart only the email step runs again, not the pdf one.
    let (_after, state) = tasque(store.clone(), 0).await;
    sleep(Duration::from_millis(1)).await;
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 0);
    assert_eq!(*state.sent.lock().unwrap(), ["b@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn the_next_step_of_an_in_memory_job_stays_in_memory() {
    let store = Arc::new(MemoryStore::default());
    let (t, _) = tasque(store.clone(), 10).await;
    t.queue(
        Pdf {
            address: "b@example.com".into(),
        },
        Priority::Medium,
        None,
        false,
    )
    .await
    .unwrap();

    sleep(Duration::from_millis(1)).await;
    assert!(pending(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_step_can_persist_the_next_job() {
    let store = Arc::new(MemoryStore::default());
    let (t, state) = tasque(store.clone(), 10).await;
    t.queue(
        Extract {
            address: "b@example.com".into(),
        },
        Priority::Medium,
        None,
        false,
    )
    .await
    .unwrap();

    sleep(Duration::from_millis(1)).await;
    let names: Vec<String> = pending(&store).await.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["email"]);

    sleep(Duration::from_secs(11)).await;
    assert_eq!(*state.sent.lock().unwrap(), ["b@example.com"]);
    assert!(pending(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_next_step_the_store_refuses_does_not_hold_back_the_one_before() {
    /// Saves everything but emails.
    #[derive(Default)]
    struct NoEmails(MemoryStore);

    #[async_trait]
    impl Store for NoEmails {
        async fn save(&self, record: &Record) -> Result<(), BoxError> {
            match record.name.as_str() {
                "email" => Err("disk full".into()),
                _ => self.0.save(record).await,
            }
        }
        async fn finish(&self, id: u128) -> Result<(), BoxError> {
            self.0.finish(id).await
        }
        async fn pending(&self) -> Result<Vec<Record>, BoxError> {
            self.0.pending().await
        }
    }

    let store = Arc::new(NoEmails::default());
    let state = Arc::new(State::default());
    let t = Tasque::new(store.clone(), state.clone())
        .add("email", |ctx, Email { address }| async move {
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .add("pdf", |ctx, Pdf { address }| async move {
            ctx.pdfs.fetch_add(1, Ordering::SeqCst);
            Ok(Step::next(Email { address }))
        })
        .run()
        .await
        .unwrap();

    t.queue(
        Pdf {
            address: "b@example.com".into(),
        },
        Priority::Medium,
        None,
        true,
    )
    .await
    .unwrap();
    sleep(Duration::from_millis(10)).await;

    // The pdf step finished; its email couldn't be saved, so it wasn't run.
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 1);
    assert!(store.pending().await.unwrap().is_empty());
    assert!(state.sent.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_that_gives_up_leaves_the_store() {
    let store = Arc::new(MemoryStore::default());
    let (t, _) = tasque(store.clone(), 0).await;
    t.queue(Fails, Priority::Medium, Some(0), true)
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert!(pending(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_with_no_handler_stays_in_the_store() {
    let store = Arc::new(MemoryStore::default());
    let orphan = Record {
        id: 7,
        name: "removed_handler".into(),
        payload: b"{}".to_vec(),
        priority: Priority::Low,
        retries: 0,
        enqueued_at: SystemTime::now(),
    };
    store.save(&orphan).await.unwrap();

    let (_t, _) = tasque(store.clone(), 0).await;
    sleep(Duration::from_millis(10)).await;

    let ids: Vec<u128> = pending(&store).await.into_iter().map(|r| r.id).collect();
    assert_eq!(ids, [7]);
}

#[tokio::test(start_paused = true)]
async fn queue_fails_when_the_store_does() {
    struct Down;

    #[async_trait]
    impl Store for Down {
        async fn save(&self, _: &Record) -> Result<(), BoxError> {
            Err("connection refused".into())
        }
        async fn finish(&self, _: u128) -> Result<(), BoxError> {
            Err("connection refused".into())
        }
        async fn pending(&self) -> Result<Vec<Record>, BoxError> {
            Ok(Vec::new())
        }
    }

    let state = Arc::new(State::default());
    let t = Tasque::new(Down, state.clone())
        .add("email", |ctx, Email { address }| async move {
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .run()
        .await
        .unwrap();

    let err = t
        .queue(email("a@example.com"), Priority::Medium, None, true)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Store(_)), "{err}");

    sleep(Duration::from_millis(10)).await;
    assert!(state.sent.lock().unwrap().is_empty());

    // Jobs that don't persist don't need the store.
    t.queue(email("b@example.com"), Priority::Medium, None, false)
        .await
        .unwrap();
    sleep(Duration::from_millis(10)).await;
    assert_eq!(*state.sent.lock().unwrap(), ["b@example.com"]);
}
