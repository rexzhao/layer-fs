//! §3 criterion 7a: every scenario function is instantiated **exactly once**, and with the mock (P3-3a).
//!
//! The point of the check is that "runs against two backends" cannot be faked: a scenario written twice — once per
//! backend — or instantiated twice with the mock would let a passing suite imply coverage it does not have. Until P3-3b
//! lands the real backend, **the only legal instantiation is `MockBackend`**, and a `::<SomethingElse>` is reported as
//! "that backend has no adapter yet" rather than silently counted.
//!
//! The scanner has **two controls**, and both call [`hits`] — the same function used on the real sources. A needle that
//! never matches (a typo) would otherwise make every count zero and the comparison vacuously true.

use std::collections::BTreeMap;

/// Counts non-overlapping occurrences of `needle`.
fn hits(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

/// Every `.rs` file under `tests/`, excluding this scanner (a scan that counted itself would count its own needles).
fn test_sources() -> BTreeMap<String, String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("read tests dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name")
            .to_owned();
        if name == "criterion_7a_shape.rs" {
            continue;
        }
        out.insert(name, std::fs::read_to_string(&path).expect("read source"));
    }
    out
}

/// Scenario definitions and their instantiations, from one pass over the same sources.
fn definitions_and_instantiations(
    sources: &BTreeMap<String, String>,
) -> (usize, Vec<(String, String)>) {
    let mut definitions = 0usize;
    let mut instantiations = Vec::new();
    for (file, text) in sources {
        definitions += hits(text, "pub fn scenario_");
        for (index, _) in text.match_indices("scenario_") {
            let rest = &text[index..];
            // An instantiation is `scenario_name::<Backend>`; a definition is `pub fn scenario_name<T: …>`.
            // The instantiation must be on the **same line** as the name and before any `(`: otherwise the match found
            // for a *definition* (`pub fn scenario_x<B: …>`) would pick up a `::<` from a call further down the file —
            // which is exactly the bug this scanner had in its first version (it reported 9 instantiations for 4
            // scenarios, counting each definition once more).
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
            let after = &rest[arrow + 3..];
            let backend: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            instantiations.push((file.clone(), format!("{name}::<{backend}")));
        }
    }
    (definitions, instantiations)
}

/// The mechanical check itself.
#[test]
fn every_scenario_has_exactly_one_mock_instantiation() {
    let sources = test_sources();
    let (definitions, instantiations) = definitions_and_instantiations(&sources);
    assert!(
        definitions >= 4,
        "cell definitions: at least the four trait-level scenarios exist, found {definitions}"
    );
    assert_eq!(
        instantiations.len(),
        definitions,
        "cell count: {definitions} scenarios but {} instantiations — every scenario runs exactly once ({instantiations:?})",
        instantiations.len()
    );

    // Which scenarios are instantiated more than once, by name.
    let mut per_name: BTreeMap<String, usize> = BTreeMap::new();
    for (_, call) in &instantiations {
        let name = call.split("::<").next().unwrap_or_default().to_owned();
        *per_name.entry(name).or_default() += 1;
    }
    let repeated: Vec<(&String, &usize)> =
        per_name.iter().filter(|(_, count)| **count > 1).collect();
    assert!(
        repeated.is_empty(),
        "cell one-each: these scenarios are instantiated more than once: {repeated:?}"
    );

    // Until P3-3b, the only legal backend is the mock. Anything else is reported as "no adapter yet", not as coverage.
    let foreign: Vec<&(String, String)> = instantiations
        .iter()
        .filter(|(_, call)| !call.ends_with("::<MockBackend"))
        .collect();
    assert!(
        foreign.is_empty(),
        "cell backend: these instantiations are not the mock — P3-3b has no adapter yet, so they cannot be real: {foreign:?}"
    );
}

/// **Control 1** — a planted second instantiation must be **counted**, otherwise the check above proves nothing.
#[test]
fn the_scanner_counts_a_planted_second_instantiation() {
    let planted = "fn t() { scenario_identity_across_rename::<MockBackend>(&b); \
                   scenario_identity_across_rename::<MockBackend>(&b); }";
    let mut sources = test_sources();
    sources.insert("planted.rs".to_owned(), planted.to_owned());
    let (definitions, instantiations) = definitions_and_instantiations(&sources);
    let real = test_sources();
    let (real_definitions, real_instantiations) = definitions_and_instantiations(&real);
    assert_eq!(
        instantiations.len(),
        real_instantiations.len() + 2,
        "cell control-1: the two planted calls are counted (needle works: {})",
        hits(planted, "scenario_")
    );
    assert_eq!(
        definitions, real_definitions,
        "cell control-1: the planted text defines no scenario, so only the instantiation count moved"
    );
}

/// **Control 2** — an instantiation with a backend that has no adapter yet is **reported**, and the mock's is not.
///
/// P3-3b will flip this: the real backend becomes legal, and this control becomes the one that must be *updated*, which
/// is exactly the moment a human should look at the scanner again.
#[test]
fn the_scanner_reports_a_foreign_backend_and_accepts_the_mock() {
    let planted = "fn t() { scenario_removal_cases::<RealAdapter>(&b); }";
    let mut sources = test_sources();
    sources.insert("planted.rs".to_owned(), planted.to_owned());
    let (_, instantiations) = definitions_and_instantiations(&sources);
    let foreign: Vec<&(String, String)> = instantiations
        .iter()
        .filter(|(_, call)| !call.ends_with("::<MockBackend"))
        .collect();
    assert_eq!(
        foreign.len(),
        1,
        "cell control-2: exactly the planted `RealAdapter` instantiation is flagged ({foreign:?})"
    );
    assert!(
        hits(planted, "scenario_removal_cases::<RealAdapter>") == 1,
        "cell control-2: the needle matches the planted call, so the flag is not a coincidence"
    );
}
