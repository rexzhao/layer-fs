//! The object role tag.
//!
//! The tag is written into every object envelope and is therefore part of the
//! canonical bytes and of the identity. Roles are a closed set for format
//! version 1: a byte outside it is a decode error rather than an unknown
//! object, so a corrupted or future-version role can never be mistaken for a
//! known one.

/// What an object's value means.
///
/// The numeric values are the on-disk tags and are frozen by format version 1
/// (see `docs/adr/ADR-001-object-encoding.md`). Adding a variant is a format
/// change, not an additive edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Role {
    /// Arbitrary canonical bytes, including the content root of a whole file.
    Bytes = 1,
    /// A content-defined chunk of a large file (Phase 2).
    Chunk = 2,
    /// A node of a file's extent index (Phase 2).
    ExtentLeaf = 3,
    /// An interior node of a file's extent index (Phase 2).
    ExtentBranch = 4,
    /// A page of directory entries.
    DirectoryLeaf = 5,
    /// An interior page of a directory index.
    DirectoryBranch = 6,
    /// The inode table of a namespace.
    InodeTable = 7,
    /// The immutable root of a filesystem view.
    FilesystemRoot = 8,
}

impl Role {
    /// Every role defined by format version 1, in tag order.
    pub const ALL: [Role; 8] = [
        Role::Bytes,
        Role::Chunk,
        Role::ExtentLeaf,
        Role::ExtentBranch,
        Role::DirectoryLeaf,
        Role::DirectoryBranch,
        Role::InodeTable,
        Role::FilesystemRoot,
    ];

    /// The on-disk tag for this role.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// The role with tag `tag`, or `None` when no version-1 role uses it.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Role> {
        match tag {
            1 => Some(Role::Bytes),
            2 => Some(Role::Chunk),
            3 => Some(Role::ExtentLeaf),
            4 => Some(Role::ExtentBranch),
            5 => Some(Role::DirectoryLeaf),
            6 => Some(Role::DirectoryBranch),
            7 => Some(Role::InodeTable),
            8 => Some(Role::FilesystemRoot),
            _ => None,
        }
    }

    /// A short, stable name for diagnostics. Not part of the encoding.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Role::Bytes => "bytes",
            Role::Chunk => "chunk",
            Role::ExtentLeaf => "extent-leaf",
            Role::ExtentBranch => "extent-branch",
            Role::DirectoryLeaf => "directory-leaf",
            Role::DirectoryBranch => "directory-branch",
            Role::InodeTable => "inode-table",
            Role::FilesystemRoot => "filesystem-root",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    const ALL: [Role; 8] = [
        Role::Bytes,
        Role::Chunk,
        Role::ExtentLeaf,
        Role::ExtentBranch,
        Role::DirectoryLeaf,
        Role::DirectoryBranch,
        Role::InodeTable,
        Role::FilesystemRoot,
    ];

    /// QA round 2, C5: every arm has a distinct, non-empty diagnostic name and
    /// `Display` repeats it. Enumerating the arms is the point — a spot check of
    /// two roles cannot see an unnamed seventh.
    #[test]
    fn every_role_has_a_distinct_name_that_display_repeats() {
        let mut names = Vec::new();
        for role in ALL {
            let name = role.name();
            assert!(!name.is_empty(), "{role:?} has an empty name");
            assert_eq!(
                role.to_string(),
                name,
                "{role:?}: Display disagrees with name"
            );
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{role:?}: name {name:?} is not lowercase kebab-case"
            );
            names.push(name);
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "role names must be distinct");
    }

    /// Every role survives a round trip through its numeric tag.
    #[test]
    fn every_role_round_trips_through_its_tag() {
        for role in ALL {
            let tag = role.tag();
            assert_eq!(Role::from_tag(tag), Some(role), "{role:?} lost its tag");
        }
    }
}
