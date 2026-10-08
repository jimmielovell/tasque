use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tasque::{BoxError, DurableJob, Error, JobId, MokaStore, Priority, Step, Store, Tasque};
use tokio::time::{sleep, Instant};

#[derive(Clone, Serialize, Deserialize)]
struct Email {
    address: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pdf {
    address: String,
}

/// A `Pdf` that takes 5 seconds before handing off.
#[derive(Clone, Serialize, Deserialize)]
struct SlowPdf {
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

impl State {
    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

/// How the email handler behaves.
#[derive(Clone, Copy)]
enum Emails {
    /// Sends after this many seconds.
    Take(u64),
    /// Always fails.
    Fail,
}

async fn tasque(store: Arc<MokaStore>, emails: Emails) -> (Tasque, Arc<State>) {
    let state = Arc::new(State::default());
    let t = Tasque::new(store)
        .add("email", state.clone(), move |ctx, Email { address }| async move {
            let Emails::Take(secs) = emails else {
                return Err("mail server down".into());
            };
            sleep(Duration::from_secs(secs)).await;
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .add("pdf", state.clone(), |ctx, Pdf { address }| async move {
            ctx.pdfs.fetch_add(1, Ordering::SeqCst);
            Ok(Step::next(Email { address }))
        })
        .add("slow_pdf", (), |_ctx, SlowPdf { address }| async move {
            sleep(Duration::from_secs(5)).await;
            Ok(Step::next(Email { address }))
        })
        .add("extract", (), |_ctx, Extract { address }| async move {
            Ok(Step::next(Email { address }).durable())
        })
        .add("fails", (), |_ctx, Fails| async move {
            Err::<(), BoxError>("always".into())
        })
        .build()
        .unwrap();
    t.run().await.unwrap();
    (t, state)
}

fn email(address: &str) -> Email {
    Email {
        address: address.into(),
    }
}

/// The store's unfinished jobs, by name.
async fn unfinished(store: &MokaStore) -> Vec<String> {
    store.release().await.unwrap();
    let mut names: Vec<String> = store
        .reclaim_stale()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.handler_name)
        .collect();
    names.sort();
    names
}

#[tokio::test(start_paused = true)]
async fn a_durable_job_is_kept_until_it_finishes() {
    let store = Arc::new(MokaStore::default());
    let (t, _) = tasque(store.clone(), Emails::Take(10)).await;
    t.queue(email("a@example.com"), Priority::High, Some(5), true)
        .await
        .unwrap();

    store.release().await.unwrap();
    let records = store.reclaim_stale().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].handler_name, "email");
    assert_eq!(records[0].priority, Priority::High);
    assert_eq!(records[0].max_retries, 5);

    sleep(Duration::from_secs(11)).await;
    assert!(unfinished(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_non_durable_job_never_reaches_the_store() {
    let store = Arc::new(MokaStore::default());
    let (t, state) = tasque(store.clone(), Emails::Take(10)).await;
    t.queue(email("a@example.com"), Priority::High, None, false)
        .await
        .unwrap();

    assert!(unfinished(&store).await.is_empty());
    sleep(Duration::from_secs(11)).await;
    assert_eq!(state.sent(), ["a@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn a_job_waiting_to_retry_runs_on_the_next_process() {
    let store = Arc::new(MokaStore::default());
    let (before, _) = tasque(store.clone(), Emails::Fail).await;
    before
        .queue(email("a@example.com"), Priority::Medium, Some(5), true)
        .await
        .unwrap();

    // The first attempt fails; the job waits to retry when the process stops.
    sleep(Duration::from_millis(10)).await;
    before.shutdown().await.unwrap();

    let (_after, state) = tasque(store.clone(), Emails::Take(0)).await;
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.sent(), ["a@example.com"]);
    assert!(unfinished(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_finished_step_leaves_only_its_next_in_the_store() {
    let store = Arc::new(MokaStore::default());
    let (before, _) = tasque(store.clone(), Emails::Fail).await;
    let pdf = Pdf {
        address: "b@example.com".into(),
    };
    before
        .queue(pdf, Priority::Medium, None, true)
        .await
        .unwrap();
    sleep(Duration::from_millis(10)).await;

    assert_eq!(unfinished(&store).await, ["email"]);

    // After a restart only the email step runs again, not the pdf one.
    before.shutdown().await.unwrap();
    let (_after, state) = tasque(store.clone(), Emails::Take(0)).await;
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 0);
    assert_eq!(state.sent(), ["b@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn the_next_step_of_an_in_memory_job_stays_in_memory() {
    let store = Arc::new(MokaStore::default());
    let (t, _) = tasque(store.clone(), Emails::Take(10)).await;
    let pdf = Pdf {
        address: "b@example.com".into(),
    };
    t.queue(pdf, Priority::Medium, None, false).await.unwrap();

    sleep(Duration::from_millis(1)).await;
    assert!(unfinished(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_step_can_persist_the_next_job() {
    let store = Arc::new(MokaStore::default());
    let (t, state) = tasque(store.clone(), Emails::Take(10)).await;
    let extract = Extract {
        address: "b@example.com".into(),
    };
    t.queue(extract, Priority::Medium, None, false)
        .await
        .unwrap();

    sleep(Duration::from_millis(1)).await;
    assert_eq!(unfinished(&store).await, ["email"]);

    sleep(Duration::from_secs(11)).await;
    assert_eq!(state.sent(), ["b@example.com"]);
    assert!(unfinished(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_that_gives_up_leaves_the_store() {
    let store = Arc::new(MokaStore::default());
    let (t, _) = tasque(store.clone(), Emails::Take(0)).await;
    t.queue(Fails, Priority::Medium, Some(0), true)
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert!(unfinished(&store).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_with_no_handler_stays_in_the_store() {
    let store = Arc::new(MokaStore::default());
    let orphan = DurableJob {
        id: JobId::now_v7(),
        handler_name: "removed_handler".into(),
        payload: Vec::new(),
        priority: Priority::Low,
        max_retries: 0,
        enqueued_at: SystemTime::now(),
    };
    store.save(&orphan).await.unwrap();
    store.release().await.unwrap();

    let (_t, _) = tasque(store.clone(), Emails::Take(0)).await;
    sleep(Duration::from_millis(10)).await;
    assert_eq!(unfinished(&store).await, ["removed_handler"]);
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_running_jobs() {
    let store = Arc::new(MokaStore::default());
    let (t, state) = tasque(store, Emails::Take(10)).await;
    t.queue(email("a@example.com"), Priority::Medium, None, false)
        .await
        .unwrap();
    sleep(Duration::from_millis(1)).await;

    let began = Instant::now();
    t.shutdown().await.unwrap();
    assert!(began.elapsed() >= Duration::from_secs(9));
    assert_eq!(state.sent(), ["a@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn queueing_after_shutdown_fails() {
    let store = Arc::new(MokaStore::default());
    let (t, _) = tasque(store, Emails::Take(0)).await;
    t.shutdown().await.unwrap();

    for persist in [false, true] {
        let err = t
            .queue(email("a@example.com"), Priority::Medium, None, persist)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Stopped), "{err}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_persisted_step_handed_off_while_shutting_down_runs_on_the_next_process() {
    let store = Arc::new(MokaStore::default());
    let (before, before_state) = tasque(store.clone(), Emails::Take(0)).await;
    let pdf = SlowPdf {
        address: "c@example.com".into(),
    };
    before
        .queue(pdf, Priority::Medium, None, true)
        .await
        .unwrap();
    sleep(Duration::from_millis(1)).await;

    // The pdf step finishes during shutdown; its email is saved, not run.
    before.shutdown().await.unwrap();
    assert!(before_state.sent().is_empty());

    let (_after, state) = tasque(store.clone(), Emails::Take(0)).await;
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.sent(), ["c@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn queue_fails_when_the_store_does() {
    struct Down;

    #[async_trait]
    impl Store for Down {
        async fn save(&self, _: &DurableJob) -> Result<(), BoxError> {
            Err("connection refused".into())
        }
        async fn finish(&self, _: JobId) -> Result<(), BoxError> {
            Err("connection refused".into())
        }
        async fn fail(&self, _: JobId, _: &str) -> Result<(), BoxError> {
            Err("connection refused".into())
        }
        async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError> {
            Ok(Vec::new())
        }
        async fn release(&self) -> Result<(), BoxError> {
            Ok(())
        }
    }

    let state = Arc::new(State::default());
    let t = Tasque::new(Down)
        .add("email", state.clone(), |ctx, Email { address }| async move {
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .build()
        .unwrap();
    t.run().await.unwrap();

    let err = t
        .queue(email("a@example.com"), Priority::Medium, None, true)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Store(_)), "{err}");

    sleep(Duration::from_millis(10)).await;
    assert!(state.sent().is_empty());

    // Jobs that don't persist don't need the store.
    t.queue(email("b@example.com"), Priority::Medium, None, false)
        .await
        .unwrap();
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.sent(), ["b@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn a_next_step_the_store_refuses_does_not_hold_back_the_one_before() {
    /// Saves everything but emails.
    #[derive(Default)]
    struct NoEmails(MokaStore);

    #[async_trait]
    impl Store for NoEmails {
        async fn save(&self, record: &DurableJob) -> Result<(), BoxError> {
            match record.handler_name.as_str() {
                "email" => Err("disk full".into()),
                _ => self.0.save(record).await,
            }
        }
        async fn finish(&self, id: JobId) -> Result<(), BoxError> {
            self.0.finish(id).await
        }
        async fn fail(&self, id: JobId, error: &str) -> Result<(), BoxError> {
            self.0.fail(id, error).await
        }
        async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError> {
            self.0.reclaim_stale().await
        }
        async fn release(&self) -> Result<(), BoxError> {
            self.0.release().await
        }
    }

    let store = Arc::new(NoEmails::default());
    let state = Arc::new(State::default());
    let t = Tasque::new(store.clone())
        .add("email", state.clone(), |ctx, Email { address }| async move {
            ctx.sent.lock().unwrap().push(address);
            Ok(())
        })
        .add("pdf", state.clone(), |ctx, Pdf { address }| async move {
            ctx.pdfs.fetch_add(1, Ordering::SeqCst);
            Ok(Step::next(Email { address }))
        })
        .build()
        .unwrap();
    t.run().await.unwrap();

    let pdf = Pdf {
        address: "b@example.com".into(),
    };
    t.queue(pdf, Priority::Medium, None, true).await.unwrap();
    sleep(Duration::from_millis(10)).await;

    // The pdf step finished; its email couldn't be saved, so it wasn't run.
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 1);
    assert!(unfinished(&store.0).await.is_empty());
    assert!(state.sent().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_job_claimed_by_another_process_is_not_retried_here() {
    /// A store that can be told it lost every job saved so far.
    #[derive(Default)]
    struct Losable {
        store: MokaStore,
        saved: Mutex<Vec<JobId>>,
        lost: Mutex<Vec<JobId>>,
    }

    #[async_trait]
    impl Store for Losable {
        async fn save(&self, record: &DurableJob) -> Result<(), BoxError> {
            self.saved.lock().unwrap().push(record.id);
            self.store.save(record).await
        }
        async fn finish(&self, id: JobId) -> Result<(), BoxError> {
            self.store.finish(id).await
        }
        async fn fail(&self, id: JobId, error: &str) -> Result<(), BoxError> {
            self.store.fail(id, error).await
        }
        async fn reclaim_stale(&self) -> Result<Vec<DurableJob>, BoxError> {
            self.store.reclaim_stale().await
        }
        async fn release(&self) -> Result<(), BoxError> {
            self.store.release().await
        }
        fn is_owned(&self, id: JobId) -> bool {
            !self.lost.lock().unwrap().contains(&id)
        }
    }

    let store = Arc::new(Losable::default());
    let state = Arc::new(State::default());
    let t = Tasque::new(store.clone())
        .add("fails", state.clone(), |ctx, Fails| async move {
            ctx.pdfs.fetch_add(1, Ordering::SeqCst);
            Err::<(), BoxError>("always".into())
        })
        .build()
        .unwrap();
    t.run().await.unwrap();

    t.queue(Fails, Priority::Medium, Some(5), true)
        .await
        .unwrap();
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 1);

    // Another process claims it while it waits to retry.
    let saved = store.saved.lock().unwrap().clone();
    *store.lost.lock().unwrap() = saved;

    sleep(Duration::from_secs(600)).await;
    assert_eq!(state.pdfs.load(Ordering::SeqCst), 1);
}
