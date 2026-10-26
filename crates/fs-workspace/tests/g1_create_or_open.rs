//! G1's eighteen cells, over a real workspace backend (P3-2 §1).
//!
//! The table (`fs_semantics::disposition`) says *what* happens; this file asserts the **third column**: what the call
//! leaves behind — the action, the node's kind, and that the returned `FileId` is the entry the name now carries.

use std::sync::Arc;

use fs_namespace::{CreateOutcome, FileId, Kind, NamespaceError, Path, Rules};
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

/// How long the holder stays inside the write closure.
const HOLD: std::time::Duration = std::time::Duration::from_millis(300);

/// How long the prober must have been blocked before we believe the closure is serialising.
///
/// Half of [`HOLD`], as a **separate** constant: the probe's falsifiability depends on being able to shrink the actual
/// hold without shrinking the expectation. Written as a literal (not `HOLD / 2`) so the two can be changed
/// independently and the difference is visible in review.
const MIN_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

/// Lock probe: the write closure really holds the store's mutex for its whole body.
///
/// Threshold is **half** the holder's hold time: a prober that returns faster than that did not wait for the lock, so
/// the closure would not be serialising anything. Returning a busy error counts as having waited (it is contention,
/// not a fast path), which is why the assertion is about elapsed time rather than about success.
#[test]
fn the_write_closure_holds_the_lock_for_its_whole_body() {
    use std::sync::mpsc;
    use std::time::Instant;

    let fixture = fixture("lock-probe");
    // Two constants on purpose: the wait threshold and the hold are separate decisions, and a single constant would
    // make the probe unfalsifiable — shrinking the hold would also shrink what it waits for.
    let hold = HOLD;
    let minimum_wait = MIN_WAIT;
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let store = fixture.workspace.store();
    let holder = std::thread::spawn(move || {
        use fs_namespace::{OpLabel, TreeStore};
        store
            .write(OpLabel::Create, |_tree| {
                ready_tx.send(()).expect("signal");
                std::thread::sleep(hold);
                Ok(())
            })
            .expect("holder");
    });
    ready_rx.recv().expect("the holder is inside the closure");

    let started = Instant::now();
    let rules = Rules::new(fixture.workspace.store());
    let outcome = rules.create_or_open(&p("/probe"), Disposition::Create);
    let elapsed = started.elapsed();
    holder.join().expect("holder thread");

    assert!(
        elapsed >= minimum_wait,
        "cell lock probe: the prober returned after {elapsed:?}, less than the {minimum_wait:?} it must wait while          the closure holds the store for {hold:?} — the closure is not serialising (outcome was {outcome:?})"
    );
}

// ---------------------------------------- concurrency: one winner, and a control that stays measured

/// Criterion 2 (behavioural): under contention exactly one entry is created and the loser sees the typed refusal.
#[test]
fn concurrent_creators_produce_exactly_one_entry() {
    use std::sync::Barrier;

    for round in 0..20usize {
        let fixture = fixture(&format!("barrier-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let mut handles: Vec<std::thread::JoinHandle<Result<FileId, NamespaceError>>> = Vec::new();
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
            matches!(refusals[0], NamespaceError::AlreadyExists { .. }),
            "cell barrier round={round}: the loser must see AlreadyExists, got {:?}",
            refusals[0]
        );
    }
}

/// A **genuine** two-step version: the decision in one transaction, the insert in another.
///
/// The earlier "control" still called `create_or_open`, so it was the atomic path with extra steps. This one really
/// separates them — `Rules::resolve` decides, `store.write(create_inode + bind)` acts — and the measurement shows
/// what happens: **nothing**. Both callers can hold a stale "the name is free" decision, yet `bind` re-checks
/// uniqueness inside the inserting transaction and refuses the second.
fn two_step_create(
    rules: &Rules<fs_workspace::WorkspaceStore>,
    store: &fs_workspace::WorkspaceStore,
    path: &Path,
    name: &'static str,
) -> Result<FileId, NamespaceError> {
    use fs_namespace::{OpLabel, TreeStore, TreeWrite};
    if rules.resolve(path).is_ok() {
        return Err(NamespaceError::AlreadyExists {
            path: path.display(),
        });
    }
    store.write(OpLabel::Create, |tree| {
        let id = tree.create_inode(Kind::File)?;
        tree.bind(FileId::ROOT, name.as_bytes(), id)?;
        Ok(id)
    })
}

/// The control, measured: the genuine two-step shape is **constructible**, and yet it produces **zero** rounds with
/// two winners (QA measured 0/20). The reason is `bind`'s uniqueness check inside the inserting transaction, so a
/// stale decision cannot create a duplicate.
///
/// This is why atomicity is asserted **structurally** (criterion 1) rather than behaviourally: with the mutex
/// covering each closure and `bind` re-checking, no arrangement of two threads produces the lost update that the
/// atomic call exists to prevent. If this test ever reports two winners, `bind` has stopped checking and the
/// structural criterion is no longer the whole story.
#[test]
fn the_two_step_version_is_constructible_but_produces_no_double_winner() {
    use std::sync::Barrier;

    let mut rounds_with_two_winners = 0usize;
    for round in 0..20usize {
        let fixture = fixture(&format!("genuine-{round}"));
        let barrier = Arc::new(Barrier::new(2));
        let mut handles: Vec<std::thread::JoinHandle<bool>> = Vec::new();
        for _ in 0..2 {
            let store = fixture.workspace.store();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let rules = Rules::new(store.clone());
                barrier.wait();
                two_step_create(&rules, &store, &p("/lost-update"), "lost-update").is_ok()
            }));
        }
        let mut winners = 0usize;
        for handle in handles {
            if handle.join().expect("thread") {
                winners += 1;
            }
        }
        match winners {
            2 => rounds_with_two_winners += 1,
            0 | 1 => {}
            other => panic!("cell round={round}: unexpected winner count {other}"),
        }
    }
    assert_eq!(
        rounds_with_two_winners, 0,
        "cell control: the constructible two-step shape must not produce two winners (QA 0/20); \
         a non-zero count means `bind` no longer re-checks inside the insert transaction"
    );
}
