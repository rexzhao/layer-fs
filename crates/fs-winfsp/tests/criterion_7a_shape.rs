//! Criterion 7a, P3-3b form: every scenario runs against **both** backends — and a name-based report says which one is
//! missing.
//!
//! P3-3a asserted "exactly one instantiation, the mock". Now the real backend exists, the rule becomes "exactly two: mock
//! and real", with two properties that a total count cannot express:
//!
//! - **only add, never remove**: dropping the mock instantiation to make room for the real one would quietly delete the
//!   coverage the mock provides (it is the only backend that can be built without SQLite);
//! - **per name**: "5 of 8 instantiations found" hides *which* scenario lost its backend, so the failure message names
//!   both facts (scenario, missing backend).
//!
//! The scenario **definitions** moved from `tests/contract.rs` to `src/scenarios.rs` in P3-3b (the real backend lives in
//! another crate and must be able to call them), so definitions are scanned there while instantiations are scanned in both
//! `tests/` directories.

use std::collections::BTreeSet;

/// The two directories instantiations live in. Both are asserted to exist: a moved directory would otherwise leave the
/// scanner checking one half and passing.
const INSTANTIATION_DIRS: [&str; 2] = ["tests", "../fs-api/tests"];
/// Where the scenario functions are defined.
const DEFINITIONS_FILE: &str = "src/scenarios.rs";
/// The two backends every scenario must be instantiated with.
const BACKENDS: [&str; 2] = ["MockBackend", "ApiBackend"];

fn hits(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

fn read(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cell {relative}: {error}"))
}

fn instantiations() -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for dir in INSTANTIATION_DIRS {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
        assert!(
            path.is_dir(),
            "cell dirs: {dir} must exist — a missing directory would silently halve the scan"
        );
        for entry in std::fs::read_dir(&path).expect("read dir") {
            let file = entry.expect("entry").path();
            if file.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            // Skip this scanner: it contains needle literals (`::<MockBackend>`) that would count as instantiations.
            if file.file_name().and_then(|name| name.to_str()) == Some("criterion_7a_shape.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read");
            for (index, _) in text.match_indices("scenario_") {
                let rest = &text[index..];
                // The instantiation must be on the same line as the name and before any `(`: a definition cannot be
                // mistaken for a call (`pub fn scenario_x<B: …>` has no `::<` on its own line).
                let window_end = rest
                    .find(|c| c == '(' || c == char::from(10))
                    .unwrap_or(rest.len());
                let Some(arrow) = rest[..window_end].find("::<") else {
                    continue;
                };
                let name: String = rest[..arrow]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.starts_with("scenario_") {
                    continue;
                }
                let backend: String = rest[arrow + 3..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                out.insert((name, backend));
            }
        }
    }
    out
}

fn definitions() -> BTreeSet<String> {
    let text = read(DEFINITIONS_FILE);
    text.match_indices("pub fn scenario_")
        .map(|(index, _)| {
            text[index + "pub fn ".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .collect()
}

/// Every scenario is instantiated with **both** backends.
#[test]
fn every_scenario_runs_against_both_backends() {
    let definitions = definitions();
    assert!(
        definitions.len() >= 4,
        "cell definitions: at least the four trait-level scenarios exist in {DEFINITIONS_FILE}, found {}",
        definitions.len()
    );

    let found = instantiations();
    let mut missing: Vec<String> = Vec::new();
    for scenario in &definitions {
        for backend in BACKENDS {
            if !found.contains(&(scenario.clone(), backend.to_owned())) {
                missing.push(format!("{scenario} <-> {backend}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "cell pairs: these scenario/backend pairs are missing: {missing:?}. Both backends must run every scenario — \
         dropping the mock to make room for the real one deletes the only backend that builds without SQLite"
    );
    assert_eq!(
        found.len(),
        definitions.len() * BACKENDS.len(),
        "cell total: {} instantiations for {} scenarios x {} backends",
        found.len(),
        definitions.len(),
        BACKENDS.len()
    );
    // No third backend sneaks in unnoticed: a new one must be added to `BACKENDS` deliberately.
    let unexpected: Vec<&(String, String)> = found
        .iter()
        .filter(|(_, backend)| !BACKENDS.contains(&backend.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "cell backends: unrecognised backends in instantiations: {unexpected:?}"
    );
}

/// Control: a planted missing pair is reported **by name**, and a planted extra backend is noticed.
#[test]
fn the_scanner_reports_missing_pairs_by_name() {
    let definitions = definitions();
    let found = instantiations();
    assert!(
        found.len() >= 2,
        "cell control: the real scan found instantiations"
    );

    // Simulate a dropped pair by removing one from the observed set.
    let victim = definitions
        .iter()
        .next()
        .expect("a scenario exists")
        .clone();
    let without: BTreeSet<(String, String)> = found
        .iter()
        .filter(|(name, backend)| !(name == &victim && backend == "ApiBackend"))
        .cloned()
        .collect();
    let missing: Vec<String> = definitions
        .iter()
        .flat_map(|scenario| {
            BACKENDS
                .iter()
                .filter(|backend| !without.contains(&(scenario.clone(), (**backend).to_owned())))
                .map(move |backend| format!("{scenario} <-> {backend}"))
        })
        .collect();
    assert_eq!(
        missing,
        vec![format!("{victim} <-> ApiBackend")],
        "cell control: exactly the planted pair is named — the message is per (scenario, backend), not a total"
    );

    // Positive control for the needle: it matches a real instantiation.
    let sample = read("tests/contract.rs");
    assert!(
        hits(&sample, "::<MockBackend>") > 0,
        "cell control: the needle matches a real instantiation, so the scan is measuring something"
    );
}
