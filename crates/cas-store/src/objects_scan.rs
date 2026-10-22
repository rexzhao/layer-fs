//! Checking that `objects/` holds only well-named object files (ADR-002 §3).
//!
//! A published object's file name must be the 64-character lowercase hex form of
//! its [`ObjectId`]. Anything else under `objects/` is a damaged Store, not an
//! object: the directory is never used as a junk drawer, so a stray name means
//! something wrote outside this crate's rules, or a filesystem damaged an entry.
//!
//! Orphans — correctly named files with no index row — are **not** a defect and
//! are left alone (ADR-002 §10); this module never looks at the index.

use std::path::Path;

use cas_core::ObjectId;

use crate::error::StoreResult;

/// One entry under `objects/` whose name is not a valid object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadObjectName {
    /// The path relative to the Store root.
    pub path: String,
    /// Why the name is rejected.
    pub reason: &'static str,
}

/// Walks `objects/` and reports every entry whose name is not an object id.
///
/// Returns the offending entries in path order. An empty vector means every
/// entry is a correctly named object file (which may still be an unpublished
/// orphan, which is allowed).
///
/// # Errors
///
/// [`StoreError::Io`] when the directory tree cannot be read.
pub(crate) fn scan_names(objects: &Path) -> StoreResult<Vec<BadObjectName>> {
    let mut bad = Vec::new();
    if !objects.is_dir() {
        return Ok(bad);
    }
    walk(objects, objects, &mut bad)?;
    bad.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(bad)
}

fn walk(root: &Path, directory: &Path, bad: &mut Vec<BadObjectName>) -> StoreResult<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let relative = relative_name(root, &path);
        if file_type.is_dir() {
            walk(root, &path, bad)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            bad.push(BadObjectName {
                path: relative,
                reason: "file name is not valid UTF-8",
            });
            continue;
        };
        if let Some(reason) = name_defect(name) {
            bad.push(BadObjectName {
                path: relative,
                reason,
            });
            continue;
        }
        // The name is a valid id; the *location* must be the two prefix levels
        // its own id implies. A correctly named file in the wrong place is just
        // as invisible to a lookup as a badly named one — and a garbage
        // collector would never find it either — so it is reported too.
        if let Some(reason) = placement_defect(root, &path, name) {
            bad.push(BadObjectName {
                path: relative,
                reason,
            });
        }
    }
    Ok(())
}

/// Checks one file name against the object-id rule.
fn name_defect(name: &str) -> Option<&'static str> {
    if name.len() != cas_core::ID_HEX_LEN {
        return Some("object file name is not 64 characters long");
    }
    if name.parse::<ObjectId>().is_err() {
        return Some("object file name is not 64 lowercase hex characters");
    }
    // `FromStr` accepts uppercase too, so reject it explicitly: ADR-002 §3 fixes
    // the on-disk form as lowercase, and two spellings of one id would let the
    // same object exist twice on a case-insensitive filesystem.
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Some("object file name is not lowercase");
    }
    None
}

/// Checks that a correctly named file sits in the two prefix directories its own
/// id implies (`objects/<name[0..2]>/<name[2..4]>/<name>`).
fn placement_defect(root: &Path, path: &Path, name: &str) -> Option<&'static str> {
    let Some(parent) = path.parent() else {
        return Some("object file has no parent directory");
    };
    let expected = root.join(&name[0..2]).join(&name[2..4]);
    if parent != expected {
        return Some("object file is not under its id's two prefix directories");
    }
    None
}

fn relative_name(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rest) => format!("objects/{}", rest.to_string_lossy().replace('\\', "/")),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_lowercase_id() {
        let name = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(name_defect(name), None);
    }

    #[test]
    fn rejects_wrong_length_case_and_non_hex() {
        assert!(name_defect("abc").is_some());
        assert!(
            name_defect("0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_some()
        );
        assert!(
            name_defect("zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_some()
        );
    }
}
