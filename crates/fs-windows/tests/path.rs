//! `path` table tests (P3-1 §1, §5).
//!
//! One row **per [`Rule`]**, each carrying an accept case and (where the rule can refuse) a reject case.
//! The table's cardinality is asserted against `Rule::ALL.len()`, so deleting a row — or adding a rule
//! without a row — is red. That is the mutation QA runs (`删掉表里的一行`).

use fs_windows::error::Failure;
use fs_windows::path::{normalize, Rule, MAX_COMPONENT_UTF16};

/// A path to try, possibly generated (the length rule needs a 255/256-unit input).
#[derive(Clone, Copy)]
enum Case {
    /// A literal path.
    Lit(&'static str),
    /// `n` ASCII `a` characters, as one component.
    Ascii(usize),
}

impl Case {
    fn build(self) -> String {
        match self {
            Self::Lit(text) => text.to_owned(),
            Self::Ascii(count) => format!(r"\{}", "a".repeat(count)),
        }
    }
}

/// What one rule's row asserts.
struct Row {
    rule: Rule,
    /// A case the rule must accept, and the normalized text it must produce (`None` when the value is
    /// generated and checked for shape only).
    accept: (Case, Option<&'static str>),
    /// A case the rule must refuse, if the rule has one.
    reject: Option<Case>,
}

const TABLE: [Row; 10] = [
    Row {
        rule: Rule::Plain,
        accept: (Case::Lit(r"\a\b.txt"), Some(r"a\b.txt")),
        reject: None,
    },
    Row {
        rule: Rule::Dot,
        accept: (Case::Lit(r"\.\a\.\b"), Some(r"a\b")),
        reject: None,
    },
    Row {
        rule: Rule::DotDot,
        accept: (Case::Lit(r"\a\..\b"), Some(r"b")),
        reject: Some(Case::Lit(r"\..\b")),
    },
    Row {
        rule: Rule::TrailingDotOrSpace,
        accept: (Case::Lit(r"\a"), Some(r"a")),
        reject: Some(Case::Lit(r"\a.\b")),
    },
    Row {
        rule: Rule::ReservedName,
        accept: (Case::Lit(r"\com0"), Some(r"com0")),
        reject: Some(Case::Lit(r"\dir\con.txt")),
    },
    Row {
        rule: Rule::IllegalCharacter,
        accept: (Case::Lit(r"\ok"), Some(r"ok")),
        reject: Some(Case::Lit(r"\a\b|c")),
    },
    Row {
        rule: Rule::ComponentTooLong,
        accept: (Case::Ascii(MAX_COMPONENT_UTF16), None),
        reject: Some(Case::Ascii(MAX_COMPONENT_UTF16 + 1)),
    },
    Row {
        rule: Rule::VerbatimPrefix,
        accept: (Case::Lit(r"\\?\C:\x.\y"), Some(r"C:\x.\y")),
        reject: None,
    },
    Row {
        rule: Rule::Unc,
        accept: (Case::Lit(r"\server\share"), Some(r"server\share")),
        reject: Some(Case::Lit(r"\\server\share")),
    },
    Row {
        rule: Rule::NotAbsolute,
        accept: (Case::Lit(r"\"), Some("")),
        reject: Some(Case::Lit(r"relative\path")),
    },
];

/// **Cardinality = enum cardinality** (§5): one row per rule, exactly.
#[test]
fn the_table_has_exactly_one_row_per_rule() {
    assert_eq!(
        TABLE.len(),
        Rule::ALL.len(),
        "one row per rule: deleting a row (QA's mutation) must be red"
    );
    for rule in Rule::ALL {
        assert_eq!(
            TABLE.iter().filter(|row| row.rule == rule).count(),
            1,
            "{rule:?} must have exactly one row"
        );
    }
}

#[test]
fn every_accept_case_normalizes_as_the_row_says() {
    for row in &TABLE {
        let input = row.accept.0.build();
        let normalized = normalize(&input).unwrap_or_else(|failure| {
            panic!("{:?} accept case {input:?} refused: {failure:?}", row.rule)
        });
        if let Some(expected) = row.accept.1 {
            assert_eq!(normalized.text, expected, "{:?} accept case", row.rule);
        }
    }
}

#[test]
fn every_reject_case_is_refused_with_invalid_name() {
    for row in &TABLE {
        let Some(case) = row.reject else { continue };
        let input = case.build();
        assert_eq!(
            normalize(&input),
            Err(Failure::InvalidName),
            "{:?} reject case {input:?} must be refused",
            row.rule
        );
    }
}

/// The component limit counts **UTF-16 units**, not characters: 128 emoji are 256 units and must be refused
/// even though `chars().count()` says 128.
#[test]
fn the_component_limit_counts_utf16_units_not_characters() {
    // 127 emoji are 254 units; one trailing ASCII character makes it exactly the limit.
    let at_limit = format!("{}{}{}", "\\", "\u{1F600}".repeat(127), "a");
    assert_eq!(
        at_limit
            .trim_start_matches('\\')
            .chars()
            .map(char::len_utf16)
            .sum::<usize>(),
        MAX_COMPONENT_UTF16
    );
    assert!(normalize(&at_limit).is_ok(), "255 units is legal");

    let over = format!("{}{}{}", "\\", "\u{1F600}".repeat(127), "ab");
    assert_eq!(
        normalize(&over),
        Err(Failure::InvalidName),
        "256 units must be refused"
    );
}

/// Reserved device names are matched case-insensitively and also when an extension is present.
#[test]
fn reserved_names_are_case_insensitive_and_extension_independent() {
    for name in [
        r"\CON",
        r"\con",
        r"\Con.txt",
        r"\NUL",
        r"\aux.log",
        r"\PRN",
        r"\COM1",
        r"\lpt9.txt",
    ] {
        assert_eq!(
            normalize(name),
            Err(Failure::InvalidName),
            "{name} must be refused"
        );
    }
    for name in [r"\CONS", r"\com0", r"\lpt10", r"\nul_"] {
        assert!(normalize(name).is_ok(), "{name} is not a device name");
    }
}
