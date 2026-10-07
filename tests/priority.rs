use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tasque::{MokaStore, Priority, Step, Tasque};
use tokio::time::{Instant, sleep};

#[derive(Clone, Serialize, Deserialize)]
struct Work {
    label: String,
    secs: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Push {
    secs: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Email;

/// Takes a second per step and hands off to itself until `left` reaches 0.
#[derive(Clone, Serialize, Deserialize)]
struct Chain {
    label: String,
    left: u32,
}

/// Hands on a `Work` labelled `label` at High priority.
#[derive(Clone, Serialize, Deserialize)]
struct Relay {
    label: String,
}

struct State {
    began: Instant,
    running: AtomicUsize,
    most_running: AtomicUsize,
    started: Mutex<Vec<String>>,
    email_after: Mutex<Option<Duration>>,
}

async fn tasque() -> (Tasque, Arc<State>) {
    let state = Arc::new(State {
        began: Instant::now(),
        running: AtomicUsize::new(0),
        most_running: AtomicUsize::new(0),
        started: Mutex::default(),
        email_after: Mutex::default(),
    });

    let t = Tasque::new(MokaStore::default())
        .add("work", state.clone(), |ctx, Work { label, secs }| async move {
            let running = ctx.running.fetch_add(1, Ordering::SeqCst) + 1;
            ctx.most_running.fetch_max(running, Ordering::SeqCst);
            ctx.started.lock().unwrap().push(label);
            sleep(Duration::from_secs(secs)).await;
            ctx.running.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
        .add("push", (), |_ctx, Push { secs }| async move {
            sleep(Duration::from_secs(secs)).await;
            Ok(())
        })
        .add("chain", state.clone(), |ctx, Chain { label, left }| async move {
            if left == 0 {
                ctx.started.lock().unwrap().push(label.clone());
            }
            sleep(Duration::from_secs(1)).await;
            Ok(match left {
                0 => Step::done(),
                _ => Step::next(Chain {
                    label,
                    left: left - 1,
                }),
            })
        })
        .add("relay", (), |_ctx, Relay { label }| async move {
            Ok(Step::next(work(&label, 0)).priority(Priority::High))
        })
        .add("email", state.clone(), |ctx, Email| async move {
            *ctx.email_after.lock().unwrap() = Some(ctx.began.elapsed());
            Ok(())
        })
        .run()
        .await
        .unwrap();
    (t, state)
}

fn work(label: &str, secs: u64) -> Work {
    Work {
        label: label.into(),
        secs,
    }
}

#[tokio::test(start_paused = true)]
async fn each_handler_runs_four_at_a_time() {
    let (t, state) = tasque().await;
    for i in 0..10 {
        t.queue(work(&i.to_string(), 5), Priority::Medium, None, false)
            .await
            .unwrap();
    }

    let started = || state.started.lock().unwrap().len();
    sleep(Duration::from_secs(1)).await;
    assert_eq!(started(), 4);
    sleep(Duration::from_secs(5)).await;
    assert_eq!(started(), 8);
    sleep(Duration::from_secs(5)).await;
    assert_eq!(started(), 10);
    assert_eq!(state.most_running.load(Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn a_backlog_on_one_handler_does_not_hold_up_another() {
    let (t, state) = tasque().await;
    for _ in 0..20 {
        t.queue(Push { secs: 20 }, Priority::High, None, false)
            .await
            .unwrap();
    }
    t.queue(Email, Priority::Low, None, false).await.unwrap();

    sleep(Duration::from_secs(1)).await;
    let email_after = state.email_after.lock().unwrap().expect("email ran");
    assert!(email_after < Duration::from_secs(1), "{email_after:?}");
}

#[tokio::test(start_paused = true)]
async fn higher_priority_goes_first_when_slots_are_full() {
    let (t, state) = tasque().await;
    // Fill the slots, each freeing a second after the last.
    for i in 0..4 {
        t.queue(work("blocker", 10 + i), Priority::Medium, None, false)
            .await
            .unwrap();
    }
    t.queue(work("low", 0), Priority::Low, None, false)
        .await
        .unwrap();
    t.queue(work("medium", 0), Priority::Medium, None, false)
        .await
        .unwrap();
    t.queue(work("high", 0), Priority::High, None, false)
        .await
        .unwrap();

    sleep(Duration::from_secs(20)).await;
    let started: Vec<String> = state
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|label| *label != "blocker")
        .cloned()
        .collect();
    assert_eq!(started, ["high", "medium", "low"]);
}

#[tokio::test(start_paused = true)]
async fn a_step_can_raise_the_next_jobs_priority() {
    let (t, state) = tasque().await;
    for i in 0..4 {
        t.queue(work("blocker", 10 + i), Priority::Medium, None, false)
            .await
            .unwrap();
    }
    t.queue(work("low", 0), Priority::Low, None, false)
        .await
        .unwrap();
    // Queued after "low" at the same priority, but its next step runs at High.
    t.queue(
        Relay {
            label: "relayed".into(),
        },
        Priority::Low,
        None,
        false,
    )
    .await
    .unwrap();

    sleep(Duration::from_secs(20)).await;
    let started: Vec<String> = state
        .started
        .lock()
        .unwrap()
        .iter()
        .filter(|label| *label != "blocker")
        .cloned()
        .collect();
    assert_eq!(started, ["relayed", "low"]);
}

#[tokio::test(start_paused = true)]
async fn a_long_chain_does_not_keep_its_slot() {
    let (t, state) = tasque().await;
    for i in 0..4 {
        let chain = Chain {
            label: format!("chain{i}"),
            left: 100,
        };
        t.queue(chain, Priority::Medium, None, false).await.unwrap();
    }
    sleep(Duration::from_millis(500)).await;
    let fresh = Chain {
        label: "fresh".into(),
        left: 0,
    };
    t.queue(fresh, Priority::Medium, None, false).await.unwrap();

    // The chains' next steps join the line behind it, so it gets the first free slot.
    sleep(Duration::from_millis(600)).await;
    assert_eq!(*state.started.lock().unwrap(), ["fresh"]);
}
