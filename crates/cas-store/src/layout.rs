//! Where things live on disk (ADR-002 §3).
//!
//! A [`Layout`] is the only place that knows the Store's directory shape. Paths
//! are derived from an [`ObjectId`], never stored, so a Store directory can be
//! moved as a whole.
//!
//! ```text
//! <store>/
//!   store.sqlite
//!   objects/ab/cd/<oid>     two hex-character levels, file name = ObjectId
//!   packs/<pack_id>.pack    immutable pack files (ADR-008 §3)
//!   tmp/<random>            never published; cleared by the next open
//! ```

use std::path::{Path, PathBuf};

use cas_core::ObjectId;

/// Names of the fixed entries under a Store root.
pub(crate) const DATABASE_FILE: &str = "store.sqlite";
pub(crate) const OBJECTS_DIR: &str = "objects";
pub(crate) const PACKS_DIR: &str = "packs";
pub(crate) const TEMP_DIR: &str = "tmp";

/// The directory shape of one Store.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Describes the Store rooted at `root`. Touches nothing on disk.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The Store root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The SQLite metadata database.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.root.join(DATABASE_FILE)
    }

    /// The directory holding published object files.
    #[must_use]
    pub fn objects(&self) -> PathBuf {
        self.root.join(OBJECTS_DIR)
    }

    /// The directory holding immutable pack files.
    #[must_use]
    pub fn packs(&self) -> PathBuf {
        self.root.join(PACKS_DIR)
    }

    /// The absolute path of one pack file.
    ///
    /// The name carries no content identity (ADR-008 §3): a pack is a storage
    /// container, and what it holds is the index `pack` row plus its
    /// `object_location` rows.
    #[must_use]
    pub fn pack_path(&self, pack_id: &str) -> PathBuf {
        self.packs().join(format!("{pack_id}.pack"))
    }

    /// The staging directory for objects that are not published yet.
    #[must_use]
    pub fn temporaries(&self) -> PathBuf {
        self.root.join(TEMP_DIR)
    }

    /// The `objects/ab/cd` directory that holds `oid`'s file: its first two and
    /// next two hex characters.
    #[must_use]
    pub fn object_directory(&self, oid: ObjectId) -> PathBuf {
        let text = oid.to_string();
        self.objects().join(&text[0..2]).join(&text[2..4])
    }

    /// The absolute path of `oid`'s published object file.
    #[must_use]
    pub fn object_path(&self, oid: ObjectId) -> PathBuf {
        self.object_directory(oid).join(oid.to_string())
    }

    /// The path of `oid` relative to the Store root, with `/` separators as the
    /// index stores it (ADR-002 §4).
    #[must_use]
    pub fn object_relative(&self, oid: ObjectId) -> String {
        let text = oid.to_string();
        format!("{OBJECTS_DIR}/{}/{}/{text}", &text[0..2], &text[2..4])
    }

    /// The fixed directories a Store needs, creating them if `create` is set.
    ///
    /// Returns them in parent-before-child order so a durability step can flush
    /// each one that a write touched.
    ///
    /// With `create`, every directory is created if absent. Both modes treat a
    /// directory that a concurrent opener is creating at the same moment as
    /// present: `create_dir_all` is idempotent, and another process may remove a
    /// `tmp/` entry between our check and our use.
    pub(crate) fn ensure(&self, create: bool) -> std::io::Result<()> {
        for directory in [
            self.root.clone(),
            self.objects(),
            self.packs(),
            self.temporaries(),
        ] {
            if create {
                std::fs::create_dir_all(&directory)?;
            } else if !directory.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("store directory {} does not exist", directory.display()),
                ));
            }
        }
        Ok(())
    }
}

/// Creates a unique-enough temporary file name.
///
/// Uniqueness only has to hold between concurrent writers of *different*
/// objects or of the same object in different processes: both then rename onto
/// the same final path, and the bytes are identical because the identity covers
/// them (ADR-002 §8). A counter plus the process id and a monotonic clock
/// reading is enough and needs no dependency.
pub(crate) fn temporary_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("{}-{nanos:016x}-{sequence:08x}.tmp", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_paths_use_two_prefix_levels() {
        let layout = Layout::new("root");
        let oid = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse::<ObjectId>()
            .expect("valid id");
        assert_eq!(
            layout.object_relative(oid).split('/').next().unwrap(),
            OBJECTS_DIR
        );
        let path = layout.object_path(oid);
        let text = path.to_string_lossy().replace('\\', "/");
        assert!(text.ends_with(
            "objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert_eq!(
            layout.object_relative(oid),
            "objects/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn temporary_names_differ() {
        let first = temporary_name();
        let second = temporary_name();
        assert_ne!(first, second);
        assert!(first.ends_with(".tmp"));
    }
}
