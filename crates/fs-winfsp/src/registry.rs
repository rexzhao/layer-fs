//! The mount registry: `FSP_FILE_SYSTEM*` → `Adapter` (P3-4 batch (i), the 299 clause coming due).
//!
//! WinFsp hands every callback the `FSP_FILE_SYSTEM*` it was created for, and nothing else identifies the mount. Until now the
//! configuration lived in process-wide `OnceLock`s, which worked for one mount and made every test share one configuration —
//! the sequential-dependency tax the write chain kept paying. The registry is the structural answer: **configuration belongs
//! to the mount**, and the pointer is the key it can be found by.
//!
//! The lifetime rule is the point of this module, so it is written as code rather than as a comment:
//!
//! - **register after `create`**, because that is when the pointer exists;
//! - **unregister after `stop` and before `delete`**, in that order;
//! - after `delete` the key must be **gone** — asserted by the caller, see [`Registry::contains`].
//!
//! A callback that arrives after the `delete` would otherwise read a freed key: that is the dangling-registry failure the
//! ordering exists to make impossible, and the assertion is what makes it checkable.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::adapter::Adapter;
use crate::ops::WorkspaceOps;

/// How many lookups missed — the **only**现场 a dangling callback leaves behind.
///
/// The ordering (`remove` after `stop`) plus the `contains == false` assertion argue that a miss cannot happen; this counter is
/// for the case where it happens anyway. **Defence and observation are different jobs**: an assertion says "this must not
/// occur", a counter says "if it did, here it is" — a refusal that shares a status code with every other refusal would hide it.
static REGISTRY_MISSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many lookups have missed since process start.
#[must_use]
pub fn miss_count() -> usize {
    REGISTRY_MISSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Mounts by the pointer WinFsp identifies them with.
///
/// `Arc` plus an explicit [`remove`](Registry::remove) rather than `Box::leak`: a leaked table can never be shown to be gone,
/// and "we never remove" is precisely the shape that turns a lifetime rule into a leak.
pub struct Registry<B: WorkspaceOps> {
    mounts: Mutex<BTreeMap<usize, Arc<Adapter<B>>>>,
}

impl<B: WorkspaceOps> Default for Registry<B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<B: WorkspaceOps> Registry<B> {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounts: Mutex::new(BTreeMap::new()),
        }
    }

    /// Registers the adapter for `file_system`. Returns the previous adapter if the key was already taken — a second mount on
    /// the same pointer would mean WinFsp reused an address while a mount was live, which is worth spotting rather than
    /// overwriting silently.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned: a panic while holding it means a callback panicked, and continuing would run callbacks
    /// against unknown state.
    pub fn insert(&self, file_system: usize, adapter: Arc<Adapter<B>>) -> Option<Arc<Adapter<B>>> {
        self.mounts
            .lock()
            .expect("registry lock")
            .insert(file_system, adapter)
    }

    /// Looks the adapter up. `None` means "this pointer is not a live mount" — the answer a callback must treat as fatal
    /// rather than as "no configuration".
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn get(&self, file_system: usize) -> Option<Arc<Adapter<B>>> {
        let found = self
            .mounts
            .lock()
            .expect("registry lock")
            .get(&file_system)
            .cloned();
        if found.is_none() {
            REGISTRY_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        found
    }

    /// Unregisters **after `stop`**. Returns whether a mount was there.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn remove(&self, file_system: usize) -> bool {
        self.mounts
            .lock()
            .expect("registry lock")
            .remove(&file_system)
            .is_some()
    }

    /// Whether a mount is registered — the assertion the teardown path makes after `delete`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn contains(&self, file_system: usize) -> bool {
        self.mounts
            .lock()
            .expect("registry lock")
            .contains_key(&file_system)
    }

    /// How many mounts are registered.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mounts.lock().expect("registry lock").len()
    }

    /// Whether the registry holds no mounts.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
