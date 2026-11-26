//! Responsibility-scoped implementation modules and reexports.
#[cfg(any(unix, windows))]
pub(crate) mod backing;
#[cfg(any(unix, windows))]
pub(crate) mod batch;
pub(crate) mod error;
#[cfg(any(unix, windows))]
pub(crate) mod files;
pub(crate) mod hints;
#[cfg(any(unix, windows))]
pub(crate) mod host;
pub(crate) mod init;
#[cfg(any(unix, windows))]
pub(crate) mod namespace;
pub(crate) mod observe;
#[cfg(any(unix, windows))]
pub(crate) mod scan;
#[cfg(any(unix, windows))]
pub(crate) mod source;
pub(crate) mod work;
