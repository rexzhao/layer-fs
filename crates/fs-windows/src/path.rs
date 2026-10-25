//! Windows path rules (P3-1 §1 `path`, ADR-009 §4.1).
//!
//! The model works on the paths WinFsp hands us (`\dir\file`) and answers two questions: what is the
//! canonical form, and is the shape legal at all. Everything here is a rule Windows enforces before the
//! filesystem is consulted — reserved device names, trailing dots/spaces, illegal characters, the 255-UTF-16
//! component limit — so getting them wrong means "Windows behaves differently than this frontend", which is
//! the class of bug that never reproduces on the machine of whoever wrote it.
//!
//! ## The table, and how a missing cell is red
//!
//! The tests are a **list of `(input, expected)` rows with a named rule per row**, and each rule has at
//! least one accept and one reject row. The completeness check is: every [`Rule`] variant must appear in the
//! table (asserted by walking [`Rule::ALL`]) — adding a rule without a case, or a case without a rule, fails.

use crate::error::Failure;

/// Which rule a table row exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// A plain path is returned unchanged.
    Plain,
    /// `.` components disappear.
    Dot,
    /// `..` pops a component, and may not escape the root.
    DotDot,
    /// A trailing dot or space in a component is refused (Windows silently strips them; we refuse).
    TrailingDotOrSpace,
    /// A reserved device name is refused, with or without an extension.
    ReservedName,
    /// An illegal character is refused.
    IllegalCharacter,
    /// A component longer than 255 UTF-16 units is refused.
    ComponentTooLong,
    /// A verbatim (`\\?\`) prefix is stripped and its payload is not otherwise processed.
    VerbatimPrefix,
    /// A UNC path is refused by this model (the frontend presents one workspace, not a network share).
    Unc,
    /// A drive-relative or empty path is refused.
    NotAbsolute,
}

impl Rule {
    /// Every rule the model enforces; the table must mention each of them.
    pub const ALL: [Rule; 10] = [
        Rule::Plain,
        Rule::Dot,
        Rule::DotDot,
        Rule::TrailingDotOrSpace,
        Rule::ReservedName,
        Rule::IllegalCharacter,
        Rule::ComponentTooLong,
        Rule::VerbatimPrefix,
        Rule::Unc,
        Rule::NotAbsolute,
    ];
}

/// The largest legal component, in UTF-16 units (not bytes, not chars).
pub const MAX_COMPONENT_UTF16: usize = 255;

/// A path that passed every structural rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// Components without separators; empty for the root.
    pub components: Vec<String>,
    /// The whole path with `\` separators and no trailing separator; empty for the root.
    pub text: String,
}

impl Normalized {
    /// The depth of the path (root is 0).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.components.len()
    }
}

/// Normalizes a Windows path, or says which rule it broke.
///
/// # Errors
///
/// [`Failure::InvalidName`] for every structural violation; the [`Rule`] that produced it is only visible in
/// the table tests, because the driver only needs the NTSTATUS.
pub fn normalize(input: &str) -> Result<Normalized, Failure> {
    let (verbatim, payload) = match input.strip_prefix(r"\\?\") {
        Some(rest) => (true, rest),
        None => (false, input),
    };
    if input.starts_with(r"\\") && !verbatim {
        return Err(Failure::InvalidName);
    }
    if !verbatim && !payload.starts_with('\\') {
        return Err(Failure::InvalidName);
    }

    let mut components: Vec<String> = Vec::new();
    for raw in payload.split('\\') {
        if raw.is_empty() {
            continue;
        }
        if verbatim {
            // A verbatim path's payload is taken literally: no `.`/`..`, no trailing-dot stripping,
            // no reserved-name rewriting. That is the point of the prefix.
            components.push(raw.to_owned());
            continue;
        }
        if raw == "." {
            continue;
        }
        if raw == ".." {
            if components.pop().is_none() {
                return Err(Failure::InvalidName);
            }
            continue;
        }
        validate_component(raw)?;
        components.push(raw.to_owned());
    }

    let text = components.join("\\");
    Ok(Normalized { components, text })
}

fn validate_component(component: &str) -> Result<(), Failure> {
    if component.ends_with('.') || component.ends_with(' ') {
        return Err(Failure::InvalidName);
    }
    if component
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20)
    {
        return Err(Failure::InvalidName);
    }
    if is_reserved(component) {
        return Err(Failure::InvalidName);
    }
    // Windows counts UTF-16 units: an emoji is two, and a 255-character string of them is 510 units.
    let units: usize = component.chars().map(char::len_utf16).sum();
    if units > MAX_COMPONENT_UTF16 {
        return Err(Failure::InvalidName);
    }
    Ok(())
}

/// Windows reserves these names for devices, **with or without** an extension ("CON.txt" is still CON).
fn is_reserved(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = upper.as_bytes();
    if bytes.len() == 4 {
        let (prefix, digit) = (bytes.split_at(3).0, bytes[3]);
        if (prefix == b"COM" || prefix == b"LPT") && digit.is_ascii_digit() && digit != b'0' {
            return true;
        }
    }
    false
}
