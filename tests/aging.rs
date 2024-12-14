#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tasque::{QueueFullError, Tasq, TasqPriority, Tasque};
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    // Test task implementation
    #[derive(Clone, Debug)]
    struct TestTask {
        id: usize,
        execution_time: Duration,
        created_at: Instant,
        completed_at: Option<Instant>,
        original_priority: TasqPriority,
        metrics_map: Arc<Mutex<HashMap<usize, TaskMetrics>>>,
    }

    #[async_trait::async_trait]
    impl Tasq for TestTask {
        type A = ();

        async fn run(&self, _: &Self::A) -> Result<(), ()> {
            let start_execution = Instant::now();
            sleep(self.execution_time).await;

            // Update metrics after execution
            let mut metrics = self.metrics_map.lock().await;
            if let Some(task_metrics) = metrics.get_mut(&self.id) {
                task_metrics.wait_time = start_execution.duration_since(self.created_at);
                task_metrics.execution_time = self.execution_time;
                task_metrics.total_time = Instant::now().duration_since(self.created_at);
            }
            Ok(())
        }
    }

    async fn setup_tasque() -> Tasque<TestTask> {
        Tasque::new(
            Some(Duration::from_secs(10)),   // timeout
            Some(Duration::from_secs(30)),   // max_retry_delay
            Some(Duration::from_millis(20)), // aging_duration
            Some(2),                         // worker_count
            Some(1000),                      // queue_capacity
        )
    }

    // Helper to add tasks with different priorities
    async fn add_task(
        tasque: &Tasque<TestTask>,
        id: usize,
        priority: TasqPriority,
        execution_time: Duration,
    ) -> Result<(), QueueFullError> {
        let task = TestTask {
            id,
            execution_time,
            created_at: Instant::now(),
            completed_at: None,
            original_priority: priority,
            metrics_map: Arc::new(Mutex::new(HashMap::new())),
        };
        tasque.add(task, priority, 0, None).await
    }

    // Task execution metrics for analysis
    #[derive(Debug)]
    struct TaskMetrics {
        task_id: usize,
        original_priority: TasqPriority,
        final_priority: TasqPriority,
        wait_time: Duration,
        execution_time: Duration,
        total_time: Duration,
        was_promoted: bool,
    }

    async fn add_task_with_tracking(
        tasque: &Tasque<TestTask>,
        id: usize,
        priority: TasqPriority,
        execution_time: Duration,
        metrics_map: &Arc<Mutex<HashMap<usize, TaskMetrics>>>,
    ) -> Result<(), QueueFullError> {
        let task = TestTask {
            id,
            execution_time,
            created_at: Instant::now(),
            original_priority: priority,
            completed_at: None,
            metrics_map: metrics_map.clone(),
        };

        // Initialize metrics
        metrics_map.lock().await.insert(
            id,
            TaskMetrics {
                task_id: id,
                original_priority: priority,
                final_priority: priority, // Will be updated if promoted
                wait_time: Duration::default(),
                execution_time: Duration::default(),
                total_time: Duration::default(),
                was_promoted: false,
            },
        );

        tasque.add(task, priority, 0, None).await
    }

    #[tokio::test]
    async fn test_starvation_with_continuous_hp_tasks() {
        let tasque = setup_tasque().await;

        // Add initial mix of tasks
        // 5 LP tasks with 100ms execution time
        for i in 0..5 {
            add_task(&tasque, i, TasqPriority::Low, Duration::from_millis(100))
                .await
                .unwrap();
        }

        // 3 MP tasks with 150ms execution time
        for i in 5..8 {
            add_task(&tasque, i, TasqPriority::Medium, Duration::from_millis(150))
                .await
                .unwrap();
        }

        // Start the queue processing
        let handle = {
            let tasque = tasque.clone();
            tokio::spawn(async move {
                tasque.run(());
            })
        };

        // Continuously add HP tasks
        for i in 8..18 {
            add_task(
                &tasque,
                i,
                TasqPriority::High,
                Duration::from_millis(50), // Quick tasks
            )
            .await
            .unwrap();
            sleep(Duration::from_millis(200)).await; // Add new HP task every 200ms
        }

        // Wait for some processing
        sleep(Duration::from_secs(5)).await;

        // Get metrics
        let (processed, failed, in_progress) = tasque.get_metrics().await;
        let high_count = tasque
            .metrics
            .high_priority_stats
            .current_queue_size
            .load(Ordering::Relaxed);
        let medium_count = tasque
            .metrics
            .medium_priority_stats
            .current_queue_size
            .load(Ordering::Relaxed);
        let low_count = tasque
            .metrics
            .low_priority_stats
            .current_queue_size
            .load(Ordering::Relaxed);

        println!(
            "Processed: {}, Failed: {}, In Progress: {}",
            processed, failed, in_progress
        );
        println!(
            "Queue lengths - High: {}, Medium: {}, Low: {}",
            high_count, medium_count, low_count
        );
        println!(
            "Promotions - Total: {}, M->H: {}, L->M: {}",
            tasque.metrics.total_promotions.load(Ordering::Relaxed),
            tasque
                .metrics
                .medium_to_high_promotions
                .load(Ordering::Relaxed),
            tasque
                .metrics
                .low_to_medium_promotions
                .load(Ordering::Relaxed)
        );

        println!(
            "Average HP execution time: {:?}",
            tasque
                .metrics
                .high_priority_stats
                .avg_exec_time
                .load(Ordering::Relaxed)
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_starvation_with_varying_hp_execution_times() {
        let tasque = setup_tasque().await;

        // Add initial tasks
        // 5 LP tasks with longer execution time
        for i in 0..5 {
            add_task(&tasque, i, TasqPriority::Low, Duration::from_millis(200))
                .await
                .unwrap();
        }

        // Start processing
        let handle = {
            let tasque = tasque.clone();
            tokio::spawn(async move {
                tasque.run(());
            })
        };

        // Add HP tasks with varying execution times
        for i in 5..15 {
            // Alternate between quick and slow HP tasks
            let exec_time = if i % 2 == 0 {
                Duration::from_millis(50) // Quick
            } else {
                Duration::from_millis(300) // Slow
            };

            add_task(&tasque, i, TasqPriority::High, exec_time)
                .await
                .unwrap();
            sleep(Duration::from_millis(150)).await;
        }

        // Wait for processing
        sleep(Duration::from_secs(5)).await;

        // Get and print metrics
        let (processed, failed, in_progress) = tasque.get_metrics().await;
        println!(
            "Processed: {}, Failed: {}, In Progress: {}",
            processed, failed, in_progress
        );

        println!(
            "Average HP execution time: {:?}",
            tasque
                .metrics
                .high_priority_stats
                .avg_exec_time
                .load(Ordering::Relaxed),
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_promotion_effectiveness() {
        let tasque = setup_tasque().await;

        // Add a mix of tasks
        // 3 LP long-running tasks
        for i in 0..3 {
            add_task(&tasque, i, TasqPriority::Low, Duration::from_millis(400))
                .await
                .unwrap();
        }

        // 2 MP medium-length tasks
        for i in 3..5 {
            add_task(&tasque, i, TasqPriority::Medium, Duration::from_millis(200))
                .await
                .unwrap();
        }

        // Start processing
        let handle = {
            let tasque = tasque.clone();
            tokio::spawn(async move {
                tasque.run(());
            })
        };

        // Add HP tasks periodically
        for i in 5..10 {
            add_task(&tasque, i, TasqPriority::High, Duration::from_millis(100))
                .await
                .unwrap();
            sleep(Duration::from_millis(300)).await;
        }

        // Wait longer to see promotion effects
        sleep(Duration::from_secs(8)).await;

        // Get and print metrics
        let (processed, failed, in_progress) = tasque.get_metrics().await;
        let promotions = tasque.metrics.total_promotions.load(Ordering::Relaxed);
        println!(
            "Processed: {}, Failed: {}, In Progress: {}, Promotions: {}",
            processed, failed, in_progress, promotions
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_starvation_prevention_under_load() {
        let tasque = setup_tasque().await;
        let metrics_map = Arc::new(Mutex::new(HashMap::new()));

        // Initial mix of tasks
        for i in 0..5 {
            add_task_with_tracking(
                &tasque,
                i,
                TasqPriority::Low,
                Duration::from_millis(100),
                &metrics_map,
            )
            .await
            .unwrap();
        }

        for i in 5..8 {
            add_task_with_tracking(
                &tasque,
                i,
                TasqPriority::Medium,
                Duration::from_millis(150),
                &metrics_map,
            )
            .await
            .unwrap();
        }

        // Start processing
        let handle = {
            let tasque = tasque.clone();
            tokio::spawn(async move {
                tasque.run(());
            })
        };

        // Continuously add HP tasks with varying execution times
        for i in 8..18 {
            let exec_time = if i % 3 == 0 {
                Duration::from_millis(50) // Quick
            } else if i % 3 == 1 {
                Duration::from_millis(150) // Medium
            } else {
                Duration::from_millis(250) // Slow
            };

            add_task_with_tracking(&tasque, i, TasqPriority::High, exec_time, &metrics_map)
                .await
                .unwrap();

            sleep(Duration::from_millis(200)).await;
        }

        // Wait for processing
        sleep(Duration::from_secs(10)).await;

        // Analyze results
        let metrics = metrics_map.lock().await;

        // Calculate statistics per priority
        let mut stats = HashMap::new();
        for (_, task_metrics) in metrics.iter() {
            let entry = stats
                .entry(task_metrics.original_priority)
                .or_insert_with(|| (0, Duration::default(), Duration::default(), 0));

            entry.0 += 1; // count
            entry.1 += task_metrics.wait_time; // total wait time
            entry.2 += task_metrics.execution_time; // total execution time
            if task_metrics.was_promoted {
                entry.3 += 1; // promotion count
            }
        }

        // Print detailed statistics
        println!("\nDetailed Task Statistics:");
        for (priority, (count, total_wait, total_exec, promotions)) in stats {
            println!("\nPriority {:?}:", priority);
            println!("  Total Tasks: {}", count);
            println!("  Average Wait Time: {:?}", total_wait / count as u32);
            println!("  Average Execution Time: {:?}", total_exec / count as u32);
            println!("  Tasks Promoted: {}", promotions);
            println!(
                "  Promotion Rate: {:.2}%",
                (promotions as f64 / count as f64) * 100.0
            );
        }

        // Print queue metrics
        let (processed, failed, in_progress) = tasque.get_metrics().await;
        println!("\nQueue Metrics:");
        println!("  Processed: {}", processed);
        println!("  Failed: {}", failed);
        println!("  In Progress: {}", in_progress);

        println!(
            "  Average HP Execution Time: {:?}",
            tasque
                .metrics
                .high_priority_stats
                .avg_exec_time
                .load(Ordering::Relaxed),
        );

        // Verify fairness conditions
        for (_, task_metrics) in metrics.iter() {
            if task_metrics.original_priority != TasqPriority::High {
                assert!(
                    task_metrics.wait_time < Duration::from_secs(30),
                    "Task {} waited too long: {:?}",
                    task_metrics.task_id,
                    task_metrics.wait_time
                );
            }
        }

        handle.abort();
    }

    #[tokio::test]
    async fn test_burst_handling() {
        let tasque = setup_tasque().await;
        let metrics_map = Arc::new(Mutex::new(HashMap::new()));

        // Start with some background tasks
        for i in 0..5 {
            add_task_with_tracking(
                &tasque,
                i,
                TasqPriority::Low,
                Duration::from_millis(100),
                &metrics_map,
            )
            .await
            .unwrap();
        }

        // Start processing
        let handle = {
            let tasque = tasque.clone();
            tokio::spawn(async move {
                tasque.run(());
            })
        };

        // Add burst of HP tasks
        for i in 5..15 {
            add_task_with_tracking(
                &tasque,
                i,
                TasqPriority::High,
                Duration::from_millis(50),
                &metrics_map,
            )
            .await
            .unwrap();
        }

        // Wait a bit
        sleep(Duration::from_secs(2)).await;

        // Add burst of MP tasks
        for i in 15..25 {
            add_task_with_tracking(
                &tasque,
                i,
                TasqPriority::Medium,
                Duration::from_millis(75),
                &metrics_map,
            )
            .await
            .unwrap();
        }

        // Wait for processing
        sleep(Duration::from_secs(10)).await;

        // Analyze results similar to previous test...
        let metrics = metrics_map.lock().await;

        // Print latency percentiles
        let mut wait_times: Vec<_> = metrics
            .values()
            .map(|m| (m.original_priority, m.wait_time))
            .collect();
        wait_times.sort_by_key(|&(_, wt)| wt);

        println!("\nLatency Percentiles:");
        for &p in &[50, 75, 90, 95, 99] {
            let idx = (wait_times.len() * p) / 100;
            println!("p{}: {:?}", p, wait_times[idx].1);
        }

        handle.abort();
    }
}
