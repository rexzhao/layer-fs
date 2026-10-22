//! Path parsing and validation (ADR-003 §3.1).
//!
//! A path is absolute, `/`-separated, and carries no normalization: what the
//! caller writes is what is looked up. Every rejection here corresponds to one
//! row of ADR-003 §3.1.

use crate::error::{InvalidPath, MAX_DEPTH, MAX_NAME_BYTES};

/// A parsed, validated path.
///
/// The root path `/` has no segments. Every other path has at least one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    segments: Vec<String>,
}

impl Path {
    /// Parses `text` into a validated path.
    ///
    /// # Errors
    ///
    /// The first rule that fails, in ADR-003 §3.1 order: absolute, segment
    /// emptiness, `.`, `..`, name length, forbidden bytes, depth, and UTF-8 is
    /// checked by the `&str` parameter itself (a caller holding raw bytes must
    /// convert first and report [`InvalidPath::NotUtf8`]).
    pub fn parse(text: &str) -> Result<Self, InvalidPath> {
        if !text.starts_with('/') {
            return Err(InvalidPath::NotAbsolute);
        }
        if text.len() == 1 {
            return Ok(Self {
                segments: Vec::new(),
            });
        }
        // A trailing '/' leaves an empty segment.
        let body = &text[1..];
        let mut segments = Vec::new();
        for segment in body.split('/') {
            if segment.is_empty() {
                return Err(InvalidPath::EmptySegment);
            }
            if segment == "." {
                return Err(InvalidPath::DotSegment);
            }
            if segment == ".." {
                return Err(InvalidPath::DotDotSegment);
            }
            if segment.len() > MAX_NAME_BYTES {
                return Err(InvalidPath::NameTooLong { len: segment.len() });
            }
            if let Some(byte) = segment.bytes().find(|byte| *byte == 0 || *byte == b'/') {
                return Err(InvalidPath::ForbiddenByte { byte });
            }
            segments.push(segment.to_owned());
        }
        if segments.len() > MAX_DEPTH {
            return Err(InvalidPath::TooDeep {
                len: segments.len(),
            });
        }
        Ok(Self { segments })
    }

    /// The path's segments, in order. Empty for the root.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Whether this is the root path.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// The last segment, or `None` for the root.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// The parent's segments: everything but the last.
    ///
    /// For the root this is empty, which is also the root, so callers must ask
    /// [`is_root`](Self::is_root) before assuming a parent exists.
    #[must_use]
    pub fn parent_segments(&self) -> &[String] {
        if self.segments.is_empty() {
            return &[];
        }
        &self.segments[..self.segments.len() - 1]
    }

    /// A display form with a leading `/`.
    #[must_use]
    pub fn display(&self) -> String {
        if self.segments.is_empty() {
            return "/".to_owned();
        }
        format!("/{}", self.segments.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_valid_and_has_no_segments() {
        let root = Path::parse("/").expect("root");
        assert!(root.is_root());
        assert!(root.segments().is_empty());
        assert_eq!(root.name(), None);
        assert_eq!(root.display(), "/");
    }

    #[test]
    fn a_normal_path_splits_into_segments() {
        let path = Path::parse("/a/b/c").expect("valid");
        assert_eq!(path.segments(), ["a", "b", "c"]);
        assert_eq!(path.name(), Some("c"));
        assert_eq!(path.parent_segments(), ["a", "b"]);
        assert_eq!(path.display(), "/a/b/c");
    }

    #[test]
    fn rejects_a_relative_path() {
        assert_eq!(Path::parse("a/b"), Err(InvalidPath::NotAbsolute));
        assert_eq!(Path::parse(""), Err(InvalidPath::NotAbsolute));
    }

    #[test]
    fn rejects_empty_segments() {
        assert_eq!(Path::parse("/a//b"), Err(InvalidPath::EmptySegment));
        assert_eq!(Path::parse("/a/"), Err(InvalidPath::EmptySegment));
        assert_eq!(Path::parse("//"), Err(InvalidPath::EmptySegment));
    }

    #[test]
    fn rejects_dot_segments() {
        assert_eq!(Path::parse("/a/./b"), Err(InvalidPath::DotSegment));
        assert_eq!(Path::parse("/."), Err(InvalidPath::DotSegment));
    }

    #[test]
    fn rejects_dotdot_segments() {
        assert_eq!(Path::parse("/a/../b"), Err(InvalidPath::DotDotSegment));
        assert_eq!(Path::parse("/.."), Err(InvalidPath::DotDotSegment));
    }

    #[test]
    fn rejects_a_name_longer_than_the_limit() {
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(
            Path::parse(&format!("/{long}")),
            Err(InvalidPath::NameTooLong {
                len: MAX_NAME_BYTES + 1
            })
        );
        let at_limit = "x".repeat(MAX_NAME_BYTES);
        assert!(Path::parse(&format!("/{at_limit}")).is_ok());
    }

    #[test]
    fn rejects_a_path_deeper_than_the_limit() {
        let deep = (0..=MAX_DEPTH).map(|_| "d").collect::<Vec<_>>().join("/");
        assert_eq!(
            Path::parse(&format!("/{deep}")),
            Err(InvalidPath::TooDeep { len: MAX_DEPTH + 1 })
        );
        let at_limit = (0..MAX_DEPTH).map(|_| "d").collect::<Vec<_>>().join("/");
        assert!(Path::parse(&format!("/{at_limit}")).is_ok());
    }

    #[test]
    fn keeps_case_and_non_ascii_names_verbatim() {
        let path = Path::parse("/A/b/中文").expect("valid");
        assert_eq!(path.segments(), ["A", "b", "中文"]);
    }
}
