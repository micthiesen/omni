//! Foundation primitives shared by every omni crate: the clock seam, JS
//! semantics for persisted derivations, digests, ids, the email model and the
//! sanctioned helpers for background work.

pub mod clock;
pub mod digest;
pub mod email;
pub mod error;
pub mod ids;
pub mod js;
mod js_date;
pub mod log;
pub mod mail_source;
pub mod process;
pub mod secrets;
pub mod spawn;

/// `futures::future::BoxFuture`, the future type used by every object-safe trait.
pub use futures::future::BoxFuture;
pub use log::LogLevel;

/// A boxed, thread-safe error used as an opaque `#[source]`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
