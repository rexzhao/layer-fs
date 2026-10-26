//! G1's eighteen cells, over a real workspace backend (P3-2 §1).
//!
//! The table (`fs_semantics::disposition`) says *what* happens; this file asserts the **third column**: what the call
//! leaves behind — the action, the node's kind, and that the returned `FileId` is the entry the name now carries.

use std::sync::Arc;

use fs_namespace::{CreateOutcome, FileId, Kind, Path, Rules};
use fs_semantics::disposition::{decide, Action, Disposition, Exists, Outcome};
use fs_workspace::Repo;

struct Fixture {
    dir: std::path::PathBuf,
    /// Kept alive for the fixture's lifetime: `WorkspaceStore` holds `Arc`s into it, and dropping it early would
    /// leave the store half-wired rather than failing loudly.
    _repo: Arc<Repo>,
    workspace: fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "fs-workspace-g1-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let repo = Arc::new(Repo::open(dir.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    Fixture {
        dir,
        _repo: repo,
        workspace,
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

/// The cells that can be seeded without a second inode: absent, file, directory.
fn seed(
    workspace: &fs_workspace::Workspace,
    rules: &Rules<fs_workspace::WorkspaceStore>,
    exists: Exists,
) {
    match exists {
        Exists::Absent => {}
        Exists::File => {
            let seeded = rules
                .create_or_open(&p("/target"), Disposition::Create)
                .expect("seed file");
            // **Non-empty** on purpose: a zero-length seed cannot tell "truncated" from "never written".
            let _ = seeded;
            workspace
                .write_at("/target", 0, b"seeded bytes")
                .expect("seed content");
            workspace.truncate("/target", 12).expect("seed size");
        }
        Exists::Directory => rules.mkdir(&p("/target")).expect("seed dir"),
    }
}

#[test]
fn every_cell_has_an_action_a_kind_and_a_file_id() {
    let mut cells = 0usize;
    for disposition in Disposition::ALL {
        for exists in [Exists::Absent, Exists::File, Exists::Directory] {
            cells += 1;
            let fixture = fixture(&format!("{}-{}", disposition as u8, cells));
            let rules = Rules::new(fixture.workspace.store());
            seed(&fixture.workspace, &rules, exists);
            let before: Option<FileId> = rules.resolve(&p("/target")).ok();

            match (decide(disposition, exists), rules.create_or_open(&p("/target"), disposition)) {
                (Outcome::Failed(_), Err(_)) => {
                    assert_eq!(
                        before,
                        rules.resolve(&p("/target")).ok(),
                        "cell disposition={disposition:?} exists={exists:?}: a refusal changed the tree"
                    );
                }
                (Outcome::Failed(failure), Ok(outcome)) => panic!(
                    "cell disposition={disposition:?} exists={exists:?}: expected {failure:?}, got {outcome:?}"
                ),
                (Outcome::Opened(action), Err(error)) => panic!(
                    "cell disposition={disposition:?} exists={exists:?}: expected {action:?}, got {error:?}"
                ),
                (Outcome::Opened(action), Ok(CreateOutcome { file_id, kind, action: got })) => {
                    assert_eq!(
                        got, action,
                        "cell disposition={disposition:?} exists={exists:?}: action"
                    );
                    let expected_kind = if exists == Exists::Directory {
                        Kind::Directory
                    } else {
                        Kind::File
                    };
                    assert_eq!(
                        kind, expected_kind,
                        "cell disposition={disposition:?} exists={exists:?}: kind"
                    );
                    assert_eq!(
                        file_id,
                        rules.resolve(&p("/target")).expect("the name resolves"),
                        "cell disposition={disposition:?} exists={exists:?}: the outcome must name the entry the name carries"
                    );
                    // The truncating dispositions must leave an **empty** file, and must keep the same entry
                    // (that pair is the whole observable difference from SUPERSEDE).
                    if action == Action::TruncateExisting {
                        assert_eq!(
                            Some(file_id),
                            before,
                            "cell disposition={disposition:?} exists={exists:?}: overwriting reuses the entry"
                        );
                        assert_eq!(
                            fixture.workspace.content_size("/target").expect("size"),
                            0,
                            "cell disposition={disposition:?} exists={exists:?}: the file must be truncated to zero"
                        );
                    }
                    if action == Action::ReplaceExisting {
                        assert_ne!(
                            Some(file_id),
                            before,
                            "cell disposition=Supersede exists={exists:?}: replacing means a new entry"
                        );
                    }
                }
            }
            drop(rules);
            drop(fixture);
        }
    }
    // Real cardinality, the same shape as `fs-semantics/tests/disposition.rs`: rows, columns and the product are
    // asserted separately, with literals — a loop counter (`cells`) only proves the loops ran.
    assert_eq!(
        Disposition::ALL.len(),
        6,
        "enum cardinality: six dispositions"
    );
    assert_eq!(
        [Exists::Absent, Exists::File, Exists::Directory].len(),
        3,
        "column cardinality: three existence states"
    );
    assert_eq!(cells, 18, "cell count: 6 dispositions × 3 states");
    let rows: std::collections::BTreeSet<u8> = Disposition::ALL.iter().map(|d| *d as u8).collect();
    assert_eq!(
        rows.len(),
        6,
        "missing row: {} distinct dispositions exercised",
        rows.len()
    );
}

// ------------------------------------------------------- g1_atomicity (three structural criteria)

/// Criterion 1 — **one transaction by construction**.
///
/// The source of `create_or_open` must contain exactly one `store.write(` and no `resolve_parent`/`resolve` call
/// *outside* it: an implementation that resolved first and wrote second would still pass every behavioural cell above
/// (single-threaded), and would still be a TOCTOU. This is a source scan because the property is structural — no
/// amount of single-threaded behaviour distinguishes "atomic" from "two steps that happened not to interleave".
/// The rule the scan applies: the existence read must not precede the write closure.
///
/// Extracted so the positive control exercises **the same code** as the real scan — a control written as a separate
/// paraphrase proves only that its own copy works.
fn flags_early_resolve(body: &str) -> bool {
    let Some(write_at) = body.find("self.store.write(") else {
        return false;
    };
    let before = &body[..write_at];
    before.contains("self.resolve(") || before.contains("tree.lookup(")
}

#[test]
fn create_or_open_resolves_inside_the_single_write_closure() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("fs-namespace")
        .join("src")
        .join("rules.rs");
    // Normalise line endings first: the function-boundary search below is textual, and a CRLF checkout would move
    // every anchor by one byte (the earlier version "worked" only because the anchors happened to match anyway).
    let raw = std::fs::read_to_string(&path).expect("read rules.rs");
    let text = raw.replace("\r\n", "\n");
    let start = text
        .find("pub fn create_or_open")
        .expect("create_or_open must exist");
    let body = &text[start..];
    let end = body
        .find("\n    /// Removes an empty directory")
        .expect("end of the function");
    let body = &body[..end];

    assert_eq!(
        body.matches("self.store.write(").count(),
        1,
        "cell atomicity: exactly one write closure (one transaction) may carry the decision and the action"
    );
    // What must **not** happen before the transaction is the existence test: that read is what the action depends
    // on, so doing it outside the closure is the TOCTOU. Resolving the *parent* first is fine — it answers "which
    // directory", not "is the name taken", and the closure re-reads the name itself.
    assert!(
        !flags_early_resolve(body),
        "cell atomicity: the existence test must happen inside the transaction, found a read before it"
    );
    assert!(
        body.contains("tree.lookup("),
        "cell atomicity: the closure itself asks the tree whether the name is taken"
    );

    // **The scanner must be able to fire.** The same rule, applied to a planted function body that resolves first,
    // has to report a violation — otherwise "no early resolve" is a claim about a dead check.
    let planted = "pub fn create_or_open(x) {
        let _ = self.resolve(x);
        self.store.write(op, |t| Ok(()))
    }";
    assert!(
        flags_early_resolve(planted),
        "cell atomicity control: a body that resolves before the write must be reported"
    );
    assert!(
        !flags_early_resolve(body),
        "cell atomicity: the real body must not be reported by the control's own rule"
    );
}

/// Criterion 2 — **two creators, exactly one entry**.
///
/// What this proves, stated precisely: under contention exactly one `Create` wins and the loser sees `AlreadyExists`.
///
/// What a two-step (`resolve` then `create`) version would do is **not** "two successes" here: QA measured **0/20**
/// rounds with two successes, and the code says why — the store's mutex is held for the whole write closure, and
/// `bind` re-checks uniqueness inside that same transaction, so the loser is refused even in the two-step shape.
///
/// That makes the race **unconstructible at this layer**, which is why atomicity is asserted *structurally*
/// (criterion 1): a behavioural test cannot distinguish "atomic" from "accidentally serialised" when the lock already
/// serialises everything. Earlier versions of this comment claimed two successes; that claim was wrong in both
/// directions and is kept only as this note, so nobody re-derives it.
#[test]
fn a_two_step_version_cannot_produce_two_successes_at_this_layer() {
    use std::sync::Barrier;

    // The control, measured rather than asserted from theory: 20 rounds of "resolve, then create", two threads each.
    let mut both = 0usize;
    for round in 0..20 {
        let fixture = fixture(&format!("twostep-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let mut handles: Vec<std::thread::JoinHandle<bool>> = Vec::new();
        for _ in 0..2 {
            let store = fixture.workspace.store();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let rules = Rules::new(store);
                barrier.wait();
                // The two steps: ask first (its own transaction), then act (another).
                let seen = rules.resolve(&p("/twostep")).is_ok();
                let _ = seen;
                rules
                    .create_or_open(&p("/twostep"), Disposition::Create)
                    .is_ok()
            }));
        }
        let mut successes = 0usize;
        for handle in handles {
            if handle.join().expect("thread") {
                successes += 1;
            }
        }
        if successes == 2 {
            both += 1;
        }
    }
    assert_eq!(
        both, 0,
        "cell control: the two-step shape must not produce two successes at this layer (QA measured 0/20);          if this ever reports more, the mutex no longer covers the whole closure"
    );
}

/// Criterion 2 (behavioural) — under contention exactly one entry is created and the loser sees the typed refusal.
#[test]
fn concurrent_creators_produce_exactly_one_entry() {
    use std::sync::Barrier;

    for round in 0..20usize {
        let fixture = fixture(&format!("barrier-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let mut handles: Vec<
            std::thread::JoinHandle<Result<FileId, fs_namespace::NamespaceError>>,
        > = Vec::new();
        for _ in 0..2 {
            let store = fixture.workspace.store();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let rules = Rules::new(store);
                barrier.wait();
                rules
                    .create_or_open(&p("/race"), Disposition::Create)
                    .map(|outcome| outcome.file_id)
            }));
        }
        let mut created = 0usize;
        let mut refusals = Vec::new();
        for handle in handles {
            match handle.join().expect("thread") {
                Ok(_) => created += 1,
                Err(error) => refusals.push(error),
            }
        }
        assert_eq!(
            (created, refusals.len()),
            (1, 1),
            "cell barrier round={round}: exactly one creator may win, refusals were {refusals:?}"
        );
        assert!(
            matches!(
                refusals[0],
                fs_namespace::NamespaceError::AlreadyExists { .. }
            ),
            "cell barrier round={round}: the loser must see AlreadyExists, got {:?}",
            refusals[0]
        );
    }
}

/// Criterion 3 — **a refusal changes nothing**, asserted on the two things a caller can see.
#[test]
fn a_refused_disposition_leaves_the_entry_and_its_bytes_alone() {
    let fixture = fixture("refusal");
    let rules = Rules::new(fixture.workspace.store());
    seed(&fixture.workspace, &rules, Exists::File);
    let before_id = rules.resolve(&p("/target")).expect("resolved");
    let before_size = fixture.workspace.content_size("/target").expect("size");
    let before_bytes = fixture.workspace.read_at("/target", 0, 64).expect("bytes");

    // CREATE on an existing file is the plain collision; nothing about the entry may move.
    assert!(rules
        .create_or_open(&p("/target"), Disposition::Create)
        .is_err());
    assert_eq!(
        rules.resolve(&p("/target")).expect("resolved"),
        before_id,
        "cell refusal: the entry keeps its FileId"
    );
    assert_eq!(
        fixture.workspace.content_size("/target").expect("size"),
        before_size,
        "cell refusal: the size is untouched"
    );
    assert_eq!(
        fixture.workspace.read_at("/target", 0, 64).expect("bytes"),
        before_bytes,
        "cell refusal: the bytes are untouched"
    );
}
