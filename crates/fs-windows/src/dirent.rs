//! Directory enumeration cursor (P3-1 §1 `dirent`, ADR-009 §5.3).
//!
//! WinFsp hands the driver a **marker** (the previous name) and asks for the next page, so enumeration must be
//! resumable by name, must filter `.`/`..`, and must not repeat or skip an entry when a page boundary lands in
//! the middle. The tests below concatenate pages over a 1000-entry directory and compare the whole result with
//! the expected list — a lost or duplicated entry shows up as a set difference, not as a count.

use crate::error::Failure;

/// One page of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The entries in this page, in order.
    pub entries: Vec<T>,
    /// The marker to pass for the next page, or `None` when this was the last page.
    pub next: Option<String>,
}

/// Entries the frontend hides from enumeration.
///
/// `.` and `..` are synthesized by Windows, so a store that happens to contain a literal `..` name must not
/// leak it; `$RECYCLE.BIN`-style names are *not* filtered, because inventing extra rules here would be a
/// frontend-specific behaviour nobody documented.
fn is_hidden(name: &str) -> bool {
    matches!(name, "." | "..")
}

/// Reads one page after `after`, at most `limit` entries.
///
/// The marker is a **name**, and the next page starts strictly after it: that is the only definition that
/// survives an entry being deleted between two pages (the driver may then see one fewer entry, which is
/// allowed, but never the same entry twice).
///
/// # Errors
///
/// [`Failure::InvalidParameter`] when `limit` is zero — a page that can make no progress would trap the
/// caller in an endless loop, so it is refused rather than honoured.
pub fn page(names: &[&str], after: Option<&str>, limit: usize) -> Result<Page<String>, Failure> {
    if limit == 0 {
        return Err(Failure::InvalidParameter);
    }
    let visible: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| !is_hidden(name))
        .collect();
    let start = match after {
        None => 0,
        Some(marker) => match visible.iter().position(|name| *name == marker) {
            Some(index) => index + 1,
            // The marker vanished (deleted or renamed between pages). The contract Windows actually offers is
            // "resume after this name", so resuming from the first entry that sorts after it is the closest
            // honest answer; returning an error would make a legal race fatal.
            None => visible
                .iter()
                .position(|name| *name > marker)
                .unwrap_or(visible.len()),
        },
    };
    let end = (start + limit).min(visible.len());
    let entries: Vec<String> = visible[start..end]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let next = if end < visible.len() {
        entries.last().cloned()
    } else {
        None
    };
    Ok(Page { entries, next })
}

/// Walks the whole listing, page by page, and returns everything it saw.
///
/// Used by the tests (and by the driver's debugging path) to state "pages concatenate to the directory" as one
/// expression instead of asserting it per page.
///
/// # Errors
///
/// As [`page`].
pub fn all_pages(names: &[&str], limit: usize) -> Result<Vec<String>, Failure> {
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let page = page(names, marker.as_deref(), limit)?;
        out.extend(page.entries.iter().cloned());
        match page.next {
            Some(next) => marker = Some(next),
            None => return Ok(out),
        }
    }
}
