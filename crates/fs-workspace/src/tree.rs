//! The tree backend a workspace exposes, and the serial top-up that keeps it usable
//! (ADR-006 §6, §7.1).
//!
//! `fs_namespace::Rules` is written against [`TreeStore`] and takes the store **by
//! value**, so the workspace cannot hand out `&OverlayStore`. This newtype is that
//! adapter: a cheap clone of two `Arc`s, one trait impl, and the one piece of policy the
//! backend cannot own — reserving more serials from the main database before a workspace
//! runs out of them.

use std::ops::Deref;
use std::sync::Arc;

use fs_namespace::{NamespaceError, NamespaceResult, OpLabel, TreeStore};
use fs_overlay::{OverlayRead, OverlayStore, OverlayWrite};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::ids;
use crate::main_db::hex;
use crate::repo::Repo;
use crate::schema::SERIAL_LOW_WATER;

/// A workspace's tree backend: the overlay plus the allocator it draws serials from.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    overlay: Arc<OverlayStore>,
    repo: Arc<Repo>,
}

impl WorkspaceStore {
    pub(crate) fn new(overlay: Arc<OverlayStore>, repo: Arc<Repo>) -> Self {
        Self { overlay, repo }
    }

    /// The overlay behind this handle.
    #[must_use]
    pub fn overlay(&self) -> &OverlayStore {
        &self.overlay
    }

    /// Reserves another segment when the current one is nearly used up.
    ///
    /// ADR-006 §6 has the workspace hold a reserved range and ask the shared allocator for
    /// more when it runs low. That has to happen *before* the write transaction opens:
    /// the reservation is a main-database transaction, and the overlay's transaction must
    /// not be open across another database's lock.
    fn top_up(&self) -> WorkspaceResult<()> {
        let (next, end) = self.overlay.serial_range()?;
        if end.saturating_sub(next) >= SERIAL_LOW_WATER {
            return Ok(());
        }
        let scope = hex(&self.overlay.scope_id());
        let segment = ids::reserve_segment(self.repo.db(), &scope)?;
        self.overlay.extend_serials(segment.end)?;
        Ok(())
    }
}

impl Deref for WorkspaceStore {
    type Target = OverlayStore;

    fn deref(&self) -> &Self::Target {
        &self.overlay
    }
}

impl TreeStore for WorkspaceStore {
    type Read<'t> = OverlayRead<'t>;
    type Write<'t> = OverlayWrite<'t>;

    fn read<T>(
        &self,
        body: impl FnOnce(&Self::Read<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        TreeStore::read(&*self.overlay, body)
    }

    fn write<T>(
        &self,
        op: OpLabel,
        body: impl FnOnce(&mut Self::Write<'_>) -> NamespaceResult<T>,
    ) -> NamespaceResult<T> {
        self.top_up().map_err(as_namespace)?;
        TreeStore::write(&*self.overlay, op, body)
    }
}

/// Carries a main-database failure out through the tree contract.
///
/// The trait has no variant for "the allocator could not be reached", and inventing one
/// would change a contract two backends share for the convenience of one. `InvalidRecord`
/// is the namespace's existing "the state behind this tree is not what it claims" form.
fn as_namespace(error: WorkspaceError) -> NamespaceError {
    NamespaceError::InvalidRecord {
        field: "workspace",
        detail: error.to_string(),
    }
}
