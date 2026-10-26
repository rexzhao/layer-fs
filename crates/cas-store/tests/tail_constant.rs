//! The `PACK_TAIL_BYTES` acceptance rules, as **checks** rather than as greps somebody has to remember
//! (P3-2 §5, corrected per review): comment lines do not count as uses, and the literal rule has two patterns.
//!
//! Why a test and not a checklist: a checklist is what let one review comment drift a count, and the "fix" for
//! that drift was to reword prose — which is exactly backwards. The rules now run on every `cargo test`.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `crates/`, in a stable order — **except this scanner**.
///
/// It necessarily contains every pattern it looks for (that is what its positive controls are), so counting it
/// would be self-reference, not evidence. This exclusion is the only one; a second one would need a reason as good
/// as this.
fn is_scanner(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "tail_constant.rs")
}

/// Every `.rs` file under `crates/`, in a stable order.
fn rust_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .expect("read a source tree")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && !is_scanner(&path)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Whether a line is a comment (`//`, `///`, `//!`), which the count deliberately ignores.
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

fn relative(path: &Path) -> String {
    path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")).join(".."))
        .expect("inside crates/")
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn the_export_is_a_single_line() {
    let lib = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("read lib.rs");
    let hits: Vec<&str> = lib
        .lines()
        .filter(|line| {
            !is_comment(line) && line.contains("pub use") && line.contains("PACK_TAIL_BYTES")
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "cell export: exactly one `pub use` line, found {hits:?}"
    );
    assert!(
        !hits[0].contains("PACK_TAIL_MAGIC"),
        "cell export: the magic stays private (it has no consumer, card §5)"
    );
}

#[test]
fn no_test_defines_its_own_tail_constant() {
    let offenders: Vec<String> = rust_sources()
        .into_iter()
        .filter(|path| relative(path).contains("/tests/"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).expect("read a test file");
            text.lines()
                .any(|line| !is_comment(line) && line.contains("const TAIL_BYTES"))
                .then(|| relative(&path))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "cell local-constant: a test-local tail constant is a second definition: {offenders:?}"
    );
}

/// Criterion 3 and 4 together: one `== 48` assertion, and **eleven** non-comment uses in total.
#[test]
fn the_constant_is_used_exactly_eleven_times_outside_comments() {
    let mut total = 0usize;
    let mut per_file: Vec<(String, usize)> = Vec::new();
    let mut assertions = Vec::new();
    for path in rust_sources() {
        let text = std::fs::read_to_string(&path).expect("read a source file");
        let mut count = 0usize;
        for (index, line) in text.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            if line.contains("PACK_TAIL_BYTES") {
                count += 1;
            }
            if line.contains("PACK_TAIL_BYTES == 48") {
                assertions.push(format!("{}:{}", relative(&path), index + 1));
            }
        }
        if count > 0 {
            total += count;
            per_file.push((relative(&path), count));
        }
    }
    assert_eq!(
        assertions.len(),
        1,
        "cell assertion: exactly one `PACK_TAIL_BYTES == 48` line, found {assertions:?}"
    );
    assert_eq!(
        total, 11,
        "cell uses: eleven non-comment uses (1 definition + 1 export + 4 src + 4 tests + 1 assertion); \
         a missing one means an alias constant survived, an extra one means a copy: {per_file:?}"
    );
    let expected = [
        ("cas-store/src/lib.rs", 1usize),
        ("cas-store/src/pack.rs", 5),
        ("cas-store/tests/pack.rs", 4),
        ("cas-store/tests/sync_probe.rs", 1),
    ];
    assert_eq!(
        per_file,
        expected
            .iter()
            .map(|(path, count)| ((*path).to_owned(), *count))
            .collect::<Vec<_>>(),
        "cell per-file: the distribution is pinned, not just the total"
    );
}

/// Criterion 5, **both patterns**: no binding of the tail literal, and no tail arithmetic on it either.
fn is_binding(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = if let Some(rest) = trimmed.strip_prefix("let ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("const ") {
        rest
    } else {
        return false;
    };
    // `let name[: Type] = 48` / `const NAME: T = 48;` — the value, not a mention.
    rest.split_once('=').is_some_and(|(_, value)| {
        value.trim_start().starts_with("48") && !value.contains("PACK_TAIL_BYTES")
    })
}

fn is_tail_arithmetic(line: &str) -> bool {
    [" + 48", " - 48", " * 48", "48 +", "48 *"]
        .iter()
        .any(|pattern| line.contains(pattern))
}

/// Criterion 6: a binding **named after the tail** that is not the constant.
///
/// The first version of rule 5 only looked for the literal `48`, which misses the composed form
/// (`let tail = 4 + 4 + 8 + 32;`) — the same second definition, spelled arithmetically. The rule is therefore
/// about the *name*: if a `let`/`const` is called `tail…`, its value must be `PACK_TAIL_BYTES`, whatever else the
/// arithmetic says.
fn is_tail_binding(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = if let Some(rest) = trimmed.strip_prefix("let ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("const ") {
        rest
    } else {
        return false;
    };
    let Some((lhs, value)) = rest.split_once('=') else {
        return false;
    };
    let name = lhs
        .split(':')
        .next()
        .unwrap_or(lhs)
        .trim()
        .to_ascii_lowercase();
    name.contains("tail") && !value.contains("PACK_TAIL_BYTES")
}

#[test]
fn no_test_binds_or_arithmetics_the_tail_literal() {
    let mut bindings = Vec::new();
    let mut arithmetics = Vec::new();
    let mut tail_bindings = Vec::new();
    // **Tests only.** In production code `at + 48` appears as the digest's end offset (`at + 16 .. at + 48`), which
    // is the tail's layout rather than a second definition of its size; the card's rule is about tests, where a
    // literal silently bypasses the constant. Named here so the narrower scope is a decision, not an accident.
    for path in rust_sources()
        .into_iter()
        .filter(|path| relative(path).contains("/tests/"))
    {
        let text = std::fs::read_to_string(&path).expect("read a source file");
        for (index, line) in text.lines().enumerate() {
            if is_comment(line) {
                continue;
            }
            let where_ = format!("{}:{}", relative(&path), index + 1);
            if is_binding(line) {
                bindings.push(where_.clone());
            }
            if is_tail_arithmetic(line) {
                arithmetics.push(where_.clone());
            }
            if is_tail_binding(line) {
                tail_bindings.push(where_);
            }
        }
    }
    assert!(
        bindings.is_empty(),
        "cell binding: a `let`/`const` holding 48 bypasses the single source: {bindings:?}"
    );
    assert!(
        arithmetics.is_empty(),
        "cell arithmetic: tail maths on the literal 48 bypasses the single source: {arithmetics:?}"
    );
    assert!(
        tail_bindings.is_empty(),
        "cell tail-binding: anything named `tail*` must be bound to the constant, not to `48` or `4 + 4 + 8 + 32`:          {tail_bindings:?}"
    );
}

/// Both rules must be able to fire: each is checked against a planted line.
#[test]
fn both_literal_rules_have_a_positive_control() {
    assert!(is_binding("let tail = 48;"), "cell control=binding");
    assert!(
        is_binding("const TAIL: usize = 48;"),
        "cell control=binding-typed"
    );
    assert!(
        !is_binding("let tail = cas_store::PACK_TAIL_BYTES;"),
        "cell control=constant"
    );
    assert!(
        !is_binding("assert!(cas_store::PACK_TAIL_BYTES == 48);"),
        "cell control=comparison"
    );

    assert!(
        is_tail_arithmetic("len - 48"),
        "cell control=arithmetic-minus"
    );
    assert!(
        is_tail_arithmetic("48 * packs"),
        "cell control=arithmetic-times"
    );
    assert!(
        !is_tail_arithmetic("len - usize::try_from(PACK_TAIL_BYTES).expect(\"fits\")"),
        "cell control=constant-arith"
    );
}
