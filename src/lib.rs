#![doc = include_str!("../README.md")]

mod codec;
mod error;
mod step;
mod store;
mod tasque;

pub use error::{BoxError, Error};
pub use step::{IntoStep, Step};
#[cfg(feature = "memory")]
pub use store::MemoryStore;
pub use store::{Record, Store};
pub use tasque::{Builder, Ctx, Tasque};

/// Which jobs go first when a handler's slots are full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    High = 2,
    Medium = 1,
    Low = 0,
}
