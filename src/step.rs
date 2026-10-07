use crate::Priority;
use std::any::{type_name, Any, TypeId};

/// What a handler does next: hand off with [`Step::next`] or finish with [`Step::done`].
/// Handlers that never hand off can return `Ok(())` instead.
///
/// The next job inherits priority, retries and persistence unless overridden:
///
/// ```ignore
/// Ok(Step::next(email).retries(5).persist())
/// ```
pub struct Step<T> {
    job: Option<T>,
    priority: Option<Priority>,
    max_retries: Option<u8>,
    durable: bool,
}

impl<T> Step<T> {
    /// Hands `job` to its type's handler.
    pub fn next(job: T) -> Self {
        Self {
            job: Some(job),
            priority: None,
            max_retries: None,
            durable: false,
        }
    }

    /// Finishes the job.
    pub fn done() -> Self {
        Self {
            job: None,
            priority: None,
            max_retries: None,
            durable: false,
        }
    }

    /// Overrides the next job's priority.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Overrides the next job's maximum retries.
    pub fn max_retries(mut self, max_retries: u8) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Persists the next job. A persisted job's next job is always persisted.
    pub fn durable(mut self) -> Self {
        self.durable = true;
        self
    }
}

/// The job a step hands off to.
#[doc(hidden)]
pub struct Handoff {
    pub(crate) type_id: TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) payload: Box<dyn Any + Send>,
    pub(crate) priority: Option<Priority>,
    pub(crate) max_retries: Option<u8>,
    pub(crate) durable: bool,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for () {}
    impl<T> Sealed for super::Step<T> {}
}

/// What a handler may return on success: `()` or a [`Step`].
pub trait IntoStep: sealed::Sealed + Send + 'static {
    #[doc(hidden)]
    fn next_handler() -> Option<(TypeId, &'static str)>;

    #[doc(hidden)]
    fn into_handoff(self) -> Option<Handoff>;
}

impl IntoStep for () {
    fn next_handler() -> Option<(TypeId, &'static str)> {
        None
    }

    fn into_handoff(self) -> Option<Handoff> {
        None
    }
}

impl<T: Send + 'static> IntoStep for Step<T> {
    fn next_handler() -> Option<(TypeId, &'static str)> {
        Some((TypeId::of::<T>(), type_name::<T>()))
    }

    fn into_handoff(self) -> Option<Handoff> {
        let job = self.job?;
        Some(Handoff {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            payload: Box::new(job),
            priority: self.priority,
            max_retries: self.max_retries,
            durable: self.durable,
        })
    }
}
