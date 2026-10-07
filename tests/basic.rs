use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasque::{Error, MokaStore, Priority, Step, Tasque};
use tokio::time::sleep;

#[derive(Clone, Serialize, Deserialize)]
struct Email {
    address: String,
    body: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pdf {
    email_address: String,
    bytes: Vec<u8>,
}

/// Fails until its `succeed_on` attempt.
#[derive(Clone, Serialize, Deserialize)]
struct Flaky {
    succeed_on: u8,
}

/// Hands on a `Flaky` that never succeeds, with `retries` if given.
#[derive(Clone, Serialize, Deserialize)]
struct Relay {
    retries: Option<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Slow;

#[derive(Clone, Serialize, Deserialize)]
struct Panics;

#[derive(Default)]
struct State {
    runs: AtomicU32,
    attempts: Mutex<Vec<u8>>,
    sent: Mutex<Vec<(String, String)>>,
}

impl State {
    fn runs(&self) -> u32 {
        self.runs.load(Ordering::SeqCst)
    }

    fn sent_to(&self) -> Vec<String> {
        let mut sent: Vec<String> = self
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a.clone())
            .collect();
        sent.sort();
        sent
    }
}

fn email(address: &str) -> Email {
    Email {
        address: address.into(),
        body: "hello".into(),
    }
}

#[allow(unreachable_code)]
async fn tasque() -> (Tasque, Arc<State>) {
    let state = Arc::new(State::default());
    let t = Tasque::new(MokaStore::default())
        .add("email", state.clone(), |ctx, Email { address, body }| async move {
            ctx.sent.lock().unwrap().push((address, body));
            Ok(())
        })
        .add(
            "pdf",
            (),
            |_ctx,
             Pdf {
                 email_address,
                 bytes,
             }| async move {
                let body = String::from_utf8(bytes)?;
                Ok(Step::next(Email {
                    address: email_address,
                    body,
                }))
            },
        )
        .add("flaky", state.clone(), |ctx, Flaky { succeed_on }| async move {
            ctx.runs.fetch_add(1, Ordering::SeqCst);
            ctx.attempts.lock().unwrap().push(ctx.attempt_count());
            if ctx.attempt_count() < succeed_on {
                return Err("not yet".into());
            }
            Ok(())
        })
        .add("relay", (), |_ctx, Relay { retries }| async move {
            let next = Step::next(Flaky {
                succeed_on: u8::MAX,
            });
            Ok(match retries {
                Some(retries) => next.max_retries(retries),
                None => next,
            })
        })
        .add("slow", state.clone(), |ctx, Slow| async move {
            sleep(Duration::from_secs(60)).await;
            ctx.runs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .add("panics", state.clone(), |ctx, Panics| async move {
            ctx.runs.fetch_add(1, Ordering::SeqCst);
            panic!("handler panicked");
            Ok(())
        })
        .run()
        .await
        .unwrap();
    (t, state)
}

#[tokio::test(start_paused = true)]
async fn runs_a_job_on_its_handler() {
    let (t, state) = tasque().await;
    t.queue(email("a@example.com"), Priority::Medium, None, false)
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert_eq!(
        *state.sent.lock().unwrap(),
        [("a@example.com".to_string(), "hello".to_string())]
    );
}

#[tokio::test(start_paused = true)]
async fn retries_a_failing_job_until_it_succeeds() {
    let (t, state) = tasque().await;
    t.queue(Flaky { succeed_on: 2 }, Priority::Medium, Some(3), false)
        .await
        .unwrap();

    sleep(Duration::from_secs(60)).await;
    assert_eq!(*state.attempts.lock().unwrap(), [0, 1, 2]);
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_its_retries() {
    let (t, state) = tasque().await;
    t.queue(
        Flaky {
            succeed_on: u8::MAX,
        },
        Priority::Medium,
        Some(2),
        false,
    )
    .await
    .unwrap();

    sleep(Duration::from_secs(600)).await;
    assert_eq!(state.runs(), 3);
}

#[tokio::test(start_paused = true)]
async fn no_retries_given_means_three() {
    let (t, state) = tasque().await;
    t.queue(
        Flaky {
            succeed_on: u8::MAX,
        },
        Priority::Medium,
        None,
        false,
    )
    .await
    .unwrap();

    sleep(Duration::from_secs(600)).await;
    assert_eq!(state.runs(), 4);
}

#[tokio::test(start_paused = true)]
async fn waits_longer_before_each_retry() {
    let (t, state) = tasque().await;
    t.queue(
        Flaky {
            succeed_on: u8::MAX,
        },
        Priority::Medium,
        Some(3),
        false,
    )
    .await
    .unwrap();

    // Retries come ~1s, ~2s and ~4s apart, each ±10%.
    sleep(Duration::from_millis(500)).await;
    assert_eq!(state.runs(), 1);
    sleep(Duration::from_millis(1_000)).await;
    assert_eq!(state.runs(), 2);
    sleep(Duration::from_millis(2_500)).await;
    assert_eq!(state.runs(), 3);
    sleep(Duration::from_millis(5_000)).await;
    assert_eq!(state.runs(), 4);
}

#[tokio::test(start_paused = true)]
async fn a_job_running_past_the_timeout_is_stopped() {
    let (t, state) = tasque().await;
    t.queue(Slow, Priority::Medium, Some(0), false)
        .await
        .unwrap();

    sleep(Duration::from_secs(120)).await;
    assert_eq!(state.runs(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_handler_is_retried_and_frees_its_slot() {
    let (t, state) = tasque().await;
    for _ in 0..4 {
        t.queue(Panics, Priority::Medium, Some(1), false)
            .await
            .unwrap();
    }

    sleep(Duration::from_secs(10)).await;
    assert_eq!(state.runs(), 8);

    // Had a panic leaked its slot, these would never run.
    for _ in 0..4 {
        t.queue(Panics, Priority::Medium, Some(0), false)
            .await
            .unwrap();
    }
    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.runs(), 12);
}

#[tokio::test(start_paused = true)]
async fn next_runs_the_handler_for_its_type() {
    let (t, state) = tasque().await;
    let pdf = Pdf {
        email_address: "b@example.com".into(),
        bytes: b"from the pdf".to_vec(),
    };
    t.queue(pdf, Priority::Medium, None, false).await.unwrap();

    sleep(Duration::from_millis(10)).await;
    assert_eq!(
        *state.sent.lock().unwrap(),
        [("b@example.com".to_string(), "from the pdf".to_string())]
    );
}

#[tokio::test(start_paused = true)]
async fn the_next_step_inherits_retries() {
    let (t, state) = tasque().await;
    t.queue(Relay { retries: None }, Priority::Medium, Some(2), false)
        .await
        .unwrap();

    sleep(Duration::from_secs(600)).await;
    assert_eq!(state.runs(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_step_can_override_retries() {
    let (t, state) = tasque().await;
    t.queue(Relay { retries: Some(0) }, Priority::Medium, Some(2), false)
        .await
        .unwrap();

    sleep(Duration::from_secs(600)).await;
    assert_eq!(state.runs(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failing_step_does_not_reach_the_next() {
    let (t, state) = tasque().await;
    let pdf = Pdf {
        email_address: "b@example.com".into(),
        bytes: vec![0xff, 0xfe],
    };
    t.queue(pdf, Priority::Medium, Some(0), false)
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert!(state.sent.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_handler_can_queue_more_jobs() {
    #[derive(Clone, Serialize, Deserialize)]
    struct Fanout(Vec<String>);

    let state = Arc::new(State::default());
    let t = Tasque::new(MokaStore::default())
        .add("email", state.clone(), |ctx, Email { address, body }| async move {
            ctx.sent.lock().unwrap().push((address, body));
            Ok(())
        })
        .add("fanout", state.clone(), |ctx, Fanout(addresses)| async move {
            for address in addresses {
                ctx.queue(email(&address), Priority::Low, None, false)
                    .await?;
            }
            Ok(())
        })
        .run()
        .await
        .unwrap();

    let fanout = Fanout(vec!["a@example.com".into(), "b@example.com".into()]);
    t.queue(fanout, Priority::Medium, None, false)
        .await
        .unwrap();

    sleep(Duration::from_millis(10)).await;
    assert_eq!(state.sent_to(), ["a@example.com", "b@example.com"]);
}

#[tokio::test(start_paused = true)]
async fn each_handler_gets_its_own_state() {
    let state = Arc::new(State::default());
    let pdfs = Arc::new(AtomicU32::new(0));
    let t = Tasque::new(MokaStore::default())
        .add("email", state.clone(), |ctx, Email { address, body }| async move {
            ctx.sent.lock().unwrap().push((address, body));
            Ok(())
        })
        .add("pdf", pdfs.clone(), |ctx, Pdf { email_address, bytes }| async move {
            ctx.fetch_add(1, Ordering::SeqCst);
            Ok(Step::next(Email {
                address: email_address,
                body: String::from_utf8(bytes)?,
            }))
        })
        .run()
        .await
        .unwrap();

    let pdf = Pdf {
        email_address: "a@example.com".into(),
        bytes: b"text".to_vec(),
    };
    t.queue(pdf, Priority::Medium, None, false).await.unwrap();

    sleep(Duration::from_millis(10)).await;
    assert_eq!(pdfs.load(Ordering::SeqCst), 1);
    assert_eq!(state.sent_to(), ["a@example.com"]);
}

#[tokio::test]
async fn queueing_a_type_without_a_handler_fails() {
    let (t, _) = tasque().await;
    let err = t
        .queue(42u32, Priority::Medium, None, false)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unregistered("u32")), "{err}");
}

#[tokio::test]
async fn start_fails_when_a_next_step_has_no_handler() {
    let result = Tasque::new(MokaStore::default())
        .add(
            "pdf",
            (),
            |_ctx,
             Pdf {
                 email_address,
                 bytes,
             }| async move {
                Ok(Step::next(Email {
                    address: email_address,
                    body: String::from_utf8(bytes)?,
                }))
            },
        )
        .run()
        .await;

    let Err(err) = result else {
        panic!("start should fail");
    };
    assert!(
        matches!(err, Error::MissingHandler { handler: "pdf", .. }),
        "{err}"
    );
}

#[test]
#[should_panic(expected = "a handler named \"email\" is already registered")]
fn registering_a_name_twice_panics() {
    let _ = Tasque::new(MokaStore::default())
        .add("email", (), |_ctx, _: Email| async move { Ok(()) })
        .add("email", (), |_ctx, _: Pdf| async move { Ok(()) });
}

#[test]
#[should_panic(expected = "is already registered")]
fn registering_a_type_twice_panics() {
    let _ = Tasque::new(MokaStore::default())
        .add("email", (), |_ctx, _: Email| async move { Ok(()) })
        .add("email_again", (), |_ctx, _: Email| async move { Ok(()) });
}
