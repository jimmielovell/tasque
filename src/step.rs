use crate::Priority;
use std::any::{Any, TypeId, type_name};

/// What a handler does next: hand off with [`Step::next`] or finish with [`Step::done`].
/// Handlers that never hand off can return `Ok(())` instead.
///
/// The next job inherits priority, retries and persistence unless overridden:
///
/// ```ignore
/// Ok(Step::next(email).retries(5).persist())
/// ```
pub struct Step<T> {
    next: Option<T>,
    priority: Option<Priority>,
    retries: Option<u8>,
    persist: bool,
}

impl<T> Step<T> {
    /// Hands `job` to its type's handler.
    pub fn next(job: T) -> Self {
        Self {
            next: Some(job),
            priority: None,
            retries: None,
            persist: false,
        }
    }

    /// Finishes the job.
    pub fn done() -> Self {
        Self {
            next: None,
            priority: None,
            retries: None,
            persist: false,
        }
    }

    /// Overrides the next job's priority.
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Overrides the next job's retries.
    pub fn retries(mut self, retries: u8) -> Self {
        self.retries = Some(retries);
        self
    }

    /// Persists the next job. A persisted job's next job is always persisted.
    pub fn persist(mut self) -> Self {
        self.persist = true;
        self
    }
}

/// The job a step hands off to.
#[doc(hidden)]
pub struct Handoff {
    pub(crate) type_id: TypeId,
    pub(crate) type_name: &'static str,
    pub(crate) value: Box<dyn Any + Send>,
    pub(crate) priority: Option<Priority>,
    pub(crate) retries: Option<u8>,
    pub(crate) persist: bool,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for () {}
    impl<T> Sealed for super::Step<T> {}
}

/// What a handler may return on success: `()` or a [`Step`].
pub trait IntoStep: sealed::Sealed + Send + 'static {
    #[doc(hidden)]
    fn next_type() -> Option<(TypeId, &'static str)>;

    #[doc(hidden)]
    fn into_next(self) -> Option<Handoff>;
}

impl IntoStep for () {
    fn next_type() -> Option<(TypeId, &'static str)> {
        None
    }

    fn into_next(self) -> Option<Handoff> {
        None
    }
}

impl<T: Send + 'static> IntoStep for Step<T> {
    fn next_type() -> Option<(TypeId, &'static str)> {
        Some((TypeId::of::<T>(), type_name::<T>()))
    }

    fn into_next(self) -> Option<Handoff> {
        let job = self.next?;
        Some(Handoff {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            value: Box::new(job),
            priority: self.priority,
            retries: self.retries,
            persist: self.persist,
        })
    }
}
