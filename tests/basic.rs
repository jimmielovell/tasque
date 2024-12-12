#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tasque::{Tasq, TasqPriority, Tasque};
    use tokio::sync::Mutex;
    use tokio::time::sleep;

    // Mock task implementation
    #[derive(Clone, Debug)]
    struct MockTask {
        should_fail: bool,
        execution_time: Duration,
        execution_count: Arc<AtomicU32>,
        last_execution: Arc<Mutex<Option<Instant>>>,
    }

    impl MockTask {
        fn new(should_fail: bool, execution_time: Duration) -> Self {
            Self {
                should_fail,
                execution_time,
                execution_count: Arc::new(AtomicU32::new(0)),
                last_execution: Arc::new(Mutex::new(None)),
            }
        }

        fn get_execution_count(&self) -> u32 {
            self.execution_count.load(Ordering::SeqCst)
        }

        async fn get_last_execution(&self) -> Option<Instant> {
            *self.last_execution.lock().await
        }
    }

    #[async_trait]
    impl Tasq for MockTask {
        type A = ();

        async fn run(&self, _: &Self::A) -> Result<(), ()> {
            self.execution_count.fetch_add(1, Ordering::SeqCst);
            *self.last_execution.lock().await = Some(Instant::now());
            sleep(self.execution_time).await;

            if self.should_fail {
                Err(())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn test_successful_task_execution() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(2),
            Some(100),
        );

        let task = MockTask::new(false, Duration::from_millis(100));
        tasque
            .add(task.clone(), TasqPriority::Medium, 3, None)
            .await
            .unwrap();

        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        sleep(Duration::from_millis(500)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!(processed, 1);
        assert_eq!(failed, 0);
        assert_eq!(in_progress, 0);
        assert_eq!(task.get_execution_count(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn test_task_retry_mechanism() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(2),
            Some(100),
        );

        let task = MockTask::new(true, Duration::from_millis(50));
        let max_retries = 2;
        tasque
            .add(task.clone(), TasqPriority::High, max_retries, None)
            .await
            .unwrap();

        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        // Wait for initial execution and retries
        sleep(Duration::from_secs(8)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!(processed, 0);
        assert_eq!(failed, max_retries as usize + 1); // Initial attempt + retries
        assert_eq!(in_progress, 0);
        assert_eq!(task.get_execution_count(), max_retries as u32 + 1);

        handle.abort();
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(1), // Single worker to ensure sequential processing
            Some(100),
        );

        let high_priority = MockTask::new(false, Duration::from_millis(50));
        let medium_priority = MockTask::new(false, Duration::from_millis(50));
        let low_priority = MockTask::new(false, Duration::from_millis(50));

        // Add tasks in reverse priority order
        tasque
            .add(low_priority.clone(), TasqPriority::Low, 0, None)
            .await
            .unwrap();
        tasque
            .add(medium_priority.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();
        tasque
            .add(high_priority.clone(), TasqPriority::High, 0, None)
            .await
            .unwrap();

        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        sleep(Duration::from_millis(500)).await;

        // Check execution order through timestamps
        let high_time = high_priority.get_last_execution().await.unwrap();
        let medium_time = medium_priority.get_last_execution().await.unwrap();
        let low_time = low_priority.get_last_execution().await.unwrap();

        assert!(high_time < medium_time);
        assert!(medium_time < low_time);

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!(processed, 3);
        assert_eq!(failed, 0);
        assert_eq!(in_progress, 0);

        handle.abort();
    }

    #[tokio::test]
    async fn test_queue_capacity() {
        let capacity = 2;
        let tasque = Tasque::new(
            Some(Duration::from_secs(5)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(1),
            Some(capacity),
        );

        let task1 = MockTask::new(false, Duration::from_millis(100));
        let task2 = MockTask::new(false, Duration::from_millis(100));
        let task3 = MockTask::new(false, Duration::from_millis(100));

        // First two tasks should succeed
        assert!(tasque
            .add(task1, TasqPriority::Medium, 0, None)
            .await
            .is_ok());
        assert!(tasque
            .add(task2, TasqPriority::Medium, 0, None)
            .await
            .is_ok());

        // Third task should fail due to capacity
        assert!(matches!(
            tasque.add(task3, TasqPriority::Medium, 0, None).await,
            Err(QueueFullError)
        ));
    }

    #[tokio::test]
    async fn test_task_timeout() {
        let timeout = Duration::from_millis(100);
        let tasque = Tasque::new(
            Some(timeout),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(1),
            Some(100),
        );

        let task = MockTask::new(false, Duration::from_millis(200)); // Task takes longer than timeout
        tasque
            .add(task.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();

        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        sleep(Duration::from_millis(500)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!(processed, 0);
        assert_eq!(failed, 1);
        assert_eq!(in_progress, 0);
        assert_eq!(task.get_execution_count(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn test_worker_sleep_and_wake() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(5)),
            Some(1),
            Some(100),
        );

        let context = Arc::new(());
        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        // Give it time to enter sleep state
        sleep(Duration::from_millis(100)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!((processed, failed, in_progress), (0, 0, 0));

        let task = MockTask::new(false, Duration::from_millis(50));
        tasque
            .add(task.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();

        sleep(Duration::from_secs(5)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!((processed, failed, in_progress), (1, 0, 0));

        tasque.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_delayed_task_processing() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(1),
            Some(100),
        );

        let task = MockTask::new(false, Duration::from_millis(50));

        // Add task with future next_run time
        let next_run = Instant::now() + Duration::from_millis(500);
        tasque
            .add(task.clone(), TasqPriority::Medium, 0, Some(next_run))
            .await
            .unwrap();

        let context = Arc::new(());
        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        // Verify task doesn't process immediately
        sleep(Duration::from_millis(100)).await;
        assert_eq!(task.get_execution_count(), 0);

        // Wait for scheduled time and verify execution
        sleep(Duration::from_millis(500)).await;
        assert_eq!(task.get_execution_count(), 1);

        tasque.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_processing() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(2), // Two workers
            Some(100),
        );

        let task1 = MockTask::new(false, Duration::from_millis(200));
        let task2 = MockTask::new(false, Duration::from_millis(200));

        tasque
            .add(task1.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();
        tasque
            .add(task2.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();

        let context = Arc::new(());
        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        // Wait for tasks to start and verify concurrent execution
        sleep(Duration::from_millis(50)).await;
        let (_, _, in_progress) = tasque.get_metrics().await;
        assert_eq!(in_progress, 2);

        // Wait for completion
        sleep(Duration::from_millis(300)).await;

        let (processed, failed, in_progress) = tasque.get_metrics().await;
        assert_eq!((processed, failed, in_progress), (2, 0, 0));

        // Verify execution times are close (indicating concurrent execution)
        let time1 = task1.get_last_execution().await.unwrap();
        let time2 = task2.get_last_execution().await.unwrap();
        assert!(time1.duration_since(time2) < Duration::from_millis(50));

        tasque.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_shutdown_with_pending_tasks() {
        let tasque = Tasque::new(
            Some(Duration::from_secs(1)),
            Some(Duration::from_secs(30)),
            Some(Duration::from_secs(60)),
            Some(1),
            Some(100),
        );

        let task = MockTask::new(false, Duration::from_millis(500));
        tasque
            .add(task.clone(), TasqPriority::Medium, 0, None)
            .await
            .unwrap();

        let context = Arc::new(());
        let handle = tokio::spawn({
            let tasque = tasque.clone();
            async move {
                tasque.run(Arc::new(())).await;
            }
        });

        // Give task time to start
        sleep(Duration::from_millis(100)).await;

        tasque.shutdown().await;
        handle.await.unwrap();

        // Verify task completed despite shutdown
        assert_eq!(task.get_execution_count(), 1);
    }
}
