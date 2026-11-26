//! Namespace Init over the content, storage and history contracts.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
mod import;
pub use error::{ProjectError, ProjectResult, RetainedAcquisition};
#[cfg(any(unix, windows))]
pub(crate) use import::backing;
#[cfg(any(unix, windows))]
pub(crate) use import::batch;
pub(crate) use import::error;
#[cfg(any(unix, windows))]
pub(crate) use import::files;
#[cfg(any(unix, windows))]
pub(crate) use import::host;
pub(crate) use import::init;
#[cfg(any(unix, windows))]
pub(crate) use import::namespace;
pub use import::observe::{InitCounters, InitCounts, InitStep};
#[cfg(any(unix, windows))]
pub(crate) use import::scan;
pub use init::{init, init_observed, InitRequest, Initialized};

pub(crate) use import::work as namespace_work;
pub use namespace_work::NamespaceWork;
