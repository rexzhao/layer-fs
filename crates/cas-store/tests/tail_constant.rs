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
/// The alphanumeric tokens of an expression, so `48` is compared as a token and not as a prefix.
fn value_tokens(value: &str) -> Vec<&str> {
    value
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .collect()
}

fn is_binding(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = if let Some(rest) = trimmed.strip_prefix("let ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("const ") {
        rest
    } else {
        return false;
    };
    // `let name[: Type] = 48` / `const NAME: T = 48;` — the value, not a mention. The check is on an
    // **independent token**: `480`, `x48` and `0x48` are different numbers and must not be caught, while `48 * 2`
    // and `48usize` are the tail literal written differently and must be.
    rest.split_once('=').is_some_and(|(_, value)| {
        value_tokens(value).contains(&"48") && !value.contains("PACK_TAIL_BYTES")
    })
}

/// The inline exemption: `// tail-allow: <reason>`. A marker without a reason is not an exemption (see F4).
const ALLOW_MARKER: &str = "// tail-allow:";

fn allow_reason(line: &str) -> Option<&str> {
    line.find(ALLOW_MARKER)
        .map(|index| line[index + ALLOW_MARKER.len()..].trim())
}

fn is_tail_arithmetic(line: &str) -> bool {
    [" + 48", " - 48", " * 48", "48 +", "48 *"]
        .iter()
        .any(|pattern| line.contains(pattern))
}

/// Criterion 6 (R1), final form: the tail size has **four allowed names**, and a binding with one of them must
/// be the constant.
///
/// ```text
/// tail | tail_bytes | tail_len | tail_size        // the only names that may denote the tail size
/// ```
///
/// The earlier version matched "any name containing `tail`", which is both too wide (a `tail_cursor` binding is
/// not a size) and unenforceable (nothing said which spellings are allowed, so every reviewer chose their own).
/// A whitelist plus a naming convention is checkable in both directions: a fifth spelling used as a size is then a
/// *naming* violation rather than a silent second definition.
const TAIL_NAMES: [&str; 4] = ["tail", "tail_bytes", "tail_len", "tail_size"];

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
    // A second copy of the *size* is what this rule forbids, so the value must be a number written out
    // (`4 + 4 + 8 + 32`, `48`); `let tail = pack::verify(..)` binds a tail **record** and is not in scope.
    !value_tokens(value).is_empty()
        && looks_like_a_number(value)
        && TAIL_NAMES.contains(&name.as_str())
        && !value.contains("PACK_TAIL_BYTES")
}

/// Whether the bound value is a numeric expression (digits, `+ * ( ) _`, whitespace) rather than a call or a
/// derived count. Kept separate so the controls can exercise it directly.
fn looks_like_a_number(value: &str) -> bool {
    let mut saw_digit = false;
    // A statement's trailing `;` is punctuation, not part of the value expression.
    for c in value.trim().trim_end_matches(';').trim().chars() {
        match c {
            '0'..='9' => saw_digit = true,
            '+' | '*' | '(' | ')' | '_' | ' ' => {}
            _ => return false,
        }
    }
    saw_digit
}

/// Names that *look* like the tail size but are outside the whitelist: using one for the size is a naming
/// violation, reported separately so the message says which rule was broken.
fn is_tail_like_but_unlisted(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = match trimmed
        .strip_prefix("let ")
        .or_else(|| trimmed.strip_prefix("const "))
    {
        Some(rest) => rest,
        None => return false,
    };
    let Some((lhs, _)) = rest.split_once('=') else {
        return false;
    };
    let name = lhs
        .split(':')
        .next()
        .unwrap_or(lhs)
        .trim()
        .to_ascii_lowercase();
    name.contains("tail") && !TAIL_NAMES.contains(&name.as_str())
}

#[test]
fn no_test_binds_or_arithmetics_the_tail_literal() {
    let mut bindings = Vec::new();
    let mut arithmetics = Vec::new();
    let mut tail_bindings = Vec::new();
    let mut unlisted_names = Vec::new();
    let mut markers = Vec::new();
    // **Scope: `crates/*/tests/` only, and that is a limit of this check, not a claim about the sources.**
    //
    // In production code `at + 48` appears as the digest's end offset (`at + 16 .. at + 48`), which is the tail's
    // **Scope: `src` and `tests` alike.** A legitimate literal in `src` (the digest's end offset) carries an inline
    // `// tail-allow:` marker with its reason; nothing else may, and F2 pins the inventory at exactly one.
    for path in rust_sources() {
        let text = std::fs::read_to_string(&path).expect("read a source file");
        for (index, line) in text.lines().enumerate() {
            let where_ = format!("{}:{}", relative(&path), index + 1);
            // The marker is checked **before** the comment filter: it lives at the end of a code line, and an
            // exemption written on its own comment line would exempt nothing (F3 rejects that).
            if allow_reason(line).is_some() {
                markers.push(where_);
                continue;
            }
            if is_comment(line) {
                continue;
            }
            if is_binding(line) {
                bindings.push(where_.clone());
            }
            if is_tail_arithmetic(line) {
                arithmetics.push(where_.clone());
            }
            if is_tail_binding(line) {
                tail_bindings.push(where_.clone());
            }
            if is_tail_like_but_unlisted(line) {
                unlisted_names.push(where_);
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
    // F2: today exactly one exemption exists, and it is the digest offset in `pack.rs`.
    assert_eq!(
        markers.len(),
        1,
        "cell F2 exemptions: exactly one `{ALLOW_MARKER}` marker is allowed, found {markers:?}"
    );
    assert!(
        markers[0].starts_with("cas-store/src/pack.rs:"),
        "cell F2 location: the single exemption is the digest offset, found {}",
        markers[0]
    );
    assert!(
        unlisted_names.is_empty(),
        "cell naming: only {TAIL_NAMES:?} may name the tail size; rename these bindings or use the constant:          {unlisted_names:?}"
    );
    assert!(
        tail_bindings.is_empty(),
        "cell tail-binding: a binding named {TAIL_NAMES:?} must hold the constant, not `48` or `4 + 4 + 8 + 32`:          {tail_bindings:?}"
    );
}

/// F3 and F4: an exemption must be **necessary** (the line would otherwise be flagged) and must carry a reason.
///
/// A marker that exempts nothing is how a whitelist rots — it stays, the code changes, and nobody notices that the
/// exemption is now covering a real violation.
#[test]
fn every_exemption_is_necessary_and_explained() {
    let mut checked = 0usize;
    for path in rust_sources() {
        let text = std::fs::read_to_string(&path).expect("read a source file");
        for (index, line) in text.lines().enumerate() {
            let Some(reason) = allow_reason(line) else {
                continue;
            };
            checked += 1;
            // F4: a reason, not an empty marker.
            assert!(
                !reason.is_empty(),
                "cell F4 reason: {}:{} has an exemption without a reason",
                relative(&path),
                index + 1
            );
            // F3: strip the marker and the line must trip at least one rule.
            let bare = line.split(ALLOW_MARKER).next().unwrap_or(line);
            assert!(
                is_binding(bare) || is_tail_binding(bare) || is_tail_arithmetic(bare),
                "cell F3 necessity: {}:{} is exempted but violates nothing — remove the marker",
                relative(&path),
                index + 1
            );
        }
    }
    assert_eq!(
        checked, 1,
        "cell F2/F3/F4: the marker inventory is pinned at one"
    );
}

/// Both rules must be able to fire: each is checked against a planted line.
/// The whitelist is data, so it is pinned: a fifth name must be a deliberate change to this test.
#[test]
fn the_whitelist_has_exactly_four_names() {
    assert_eq!(TAIL_NAMES.len(), 4, "cell whitelist size");
    assert_eq!(
        TAIL_NAMES,
        ["tail", "tail_bytes", "tail_len", "tail_size"],
        "cell whitelist contents (naming convention)"
    );
}

/// The whitelist is checked **in both directions**: every entry must be able to fire the rule (otherwise it is a
/// name nobody enforces), and a name outside it must be reported by the naming rule instead.
#[test]
fn every_whitelisted_name_fires_and_no_other_name_does() {
    let mut fired = Vec::new();
    for name in TAIL_NAMES {
        let line = format!("let {name} = 4 + 4 + 8 + 32;");
        assert!(
            is_tail_binding(&line),
            "cell whitelist-entry={name}: must be caught when it holds a composed size"
        );
        assert!(
            !is_tail_binding(&format!("let {name} = cas_store::PACK_TAIL_BYTES;")),
            "cell whitelist-entry={name}: the constant is the only correct value"
        );
        fired.push(name);
    }
    assert_eq!(
        fired.len(),
        TAIL_NAMES.len(),
        "cell coverage: entries == exercised names"
    );

    let outside = "let tail_hint = 4 + 4 + 8 + 32;";
    assert!(
        !is_tail_binding(outside),
        "cell outside-whitelist: not this rule's business"
    );
    assert!(
        is_tail_like_but_unlisted(outside),
        "cell outside-whitelist: the naming rule must still report it"
    );
}

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
