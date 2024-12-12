use async_trait::async_trait;

/// Trait that defines a task to be executed by the `Tasque` queue.
///
/// Types implementing this trait can be added to the `Tasque` with a specified priority.
/// The `run` method will be called asynchronously.
#[async_trait]
pub trait Tasq {
    /// Argument type passed to the task during execution.
    type A: Sync + Send;

    /// Asynchronously run the task.
    ///
    /// # Arguments
    /// * `arg` - A reference to an argument passed to the task.
    ///
    /// # Returns
    /// `Result<(), ()>` indicating success or failure.
    async fn run(&self, arg: &Self::A) -> Result<(), ()>;
}
