use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tasque::{Tasq, TasqPriority, Tasque};
use tokio::time::sleep;

// Simple counter to track task execution
static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// A simple task that increments a counter and prints
#[derive(Debug)]
struct CounterTask {
    id: usize,
}

// Simple context that will be passed to tasks
#[derive(Debug)]
struct Context {
    name: String,
}

#[async_trait]
impl Tasq for CounterTask {
    type A = Context;

    async fn run(&self, ctx: &Self::A) -> Result<(), ()> {
        println!("Starting task {} with context {}", self.id, ctx.name);

        // Simulate retry logic
        if self.id == 3 {
            return Err(());
        }

        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // Simulate some work
        sleep(Duration::from_millis(100)).await;

        println!("Completed task {}", self.id);
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing for better debug output
    tracing_subscriber::fmt::init();

    // Create the task queue with debug-friendly settings
    let tasque: Tasque<CounterTask> = Tasque::new(
        Some(Duration::from_secs(5)),  // timeout
        Some(Duration::from_secs(10)), // max_delay
        Some(Duration::from_secs(30)), // aging_duration
        Some(1),                       // worker_count
        Some(100),                     // queue_capacity
    );

    // Create context for tasks
    let context = Context {
        name: "TestContext".to_string(),
    };

    // Print initial metrics
    let (processed, failed, in_progress) = tasque.get_metrics().await;
    println!(
        "Initial metrics - Processed: {}, Failed: {}, In Progress: {}",
        processed, failed, in_progress
    );

    tasque.run(Arc::new(context)).await;

    // Give it time to enter sleep state
    sleep(Duration::from_millis(100)).await;

    for i in 0..5 {
        tasque
            .add(CounterTask { id: i }, TasqPriority::Medium, 3, None)
            .await
            .expect("Failed to add task");
        println!("Added task {}", i);
    }

    sleep(Duration::from_secs(30)).await;
    tasque.shutdown().await;

    // Print final metrics
    let (processed, failed, in_progress) = tasque.get_metrics().await;
    println!(
        "Final metrics - Processed: {}, Failed: {}, In Progress: {}",
        processed, failed, in_progress
    );

    // Print final counter value
    println!(
        "Total tasks executed: {}",
        COUNTER.load(std::sync::atomic::Ordering::SeqCst)
    );
}
