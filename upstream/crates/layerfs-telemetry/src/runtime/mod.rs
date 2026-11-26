//! Explicit platform monitor and application lifetime assembly.
#[cfg(feature = "platform")]
mod monitor;
#[cfg(feature = "platform")]
pub use monitor::{Monitor, MonitorConfig};
#[cfg(feature = "platform")]
mod session;
#[cfg(feature = "platform")]
pub use session::{Configuration, Runtime};

pub mod observation;
pub mod operation;
