//! G4: conditional `rename(from, to, replace)` (P3-2 §4, criteria 1–9).
//!
//! Every test asserts a **pair**: what the outcome says and what the tree then answers. That is the whole point of the
//! outcome — `replaced = Some(old)` is worthless if `stat_id(old)` still finds a live inode, and `binding` is worthless
//! if `remove_at` with it deletes the wrong entry.
//!
//! The card's premise — "when the destination name has an open handle, the FSD does not send Rename" — is **not
//! verified**; these tests do not rely on it, because they only ever assert what the core does. The consequence for a
//! future L2 measurement is recorded in the card, not here.

mod common;

use std::sync::Arc;

use fs_namespace::{FileId, NamespaceError, Path, RenameOutcome, Rules};
use fs_semantics::disposition::Disposition;
use fs_workspace::Repo;

struct Fixture {
    dir: std::path::PathBuf,
    /// Held so the repo outlives the workspace (the workspace lock is released by its own `close`/drop).
    _repo: Arc<Repo>,
    workspace: fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `(path, length, hash)` per durable file.
type StoreFiles = Vec<(String, u64, String)>;
/// `(name bytes as text, FileId)` per directory entry.
type Listing = Vec<(String, FileId)>;

fn fixture(label: &str) -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "fs-workspace-g4-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&path).expect("temp dir");
    let repo = Arc::new(Repo::open(path.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    Fixture {
        dir: path,
        _repo: repo,
        workspace,
    }
}

fn p(text: &str) -> Path {
    Path::parse(text).expect("path")
}

fn rules(fixture: &Fixture) -> Rules<fs_workspace::WorkspaceStore> {
    Rules::new(fixture.workspace.store())
}

/// `(path, length, BLAKE3)` over everything durable in the store, plus the parent's enumeration.
///
/// A criterion that only compared lengths would treat "same size, different bytes" as unchanged, and one that only
/// compared the enumeration would miss an inode that lost its content while keeping its name.
fn fingerprint(fixture: &Fixture, parent: FileId) -> (StoreFiles, Listing) {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut StoreFiles) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                let relative = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                // `-shm` is shared memory between connections, not durable content (ADR-008 §6.1).
                if !relative.ends_with("-shm") {
                    out.push((
                        relative,
                        bytes.len() as u64,
                        blake3::hash(&bytes).to_hex().to_string(),
                    ));
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &fixture.dir.join("store"),
        &fixture.dir.join("store"),
        &mut files,
    );
    files.sort();
    let listing = fixture
        .workspace
        .read_dir_id(parent, None, 1024)
        .expect("read_dir")
        .into_iter()
        .map(|(name, id)| (String::from_utf8_lossy(&name).into_owned(), id))
        .collect();
    (files, listing)
}

fn create_file(fixture: &Fixture, path: &str, content: &[u8]) -> FileId {
    let rules = rules(fixture);
    let created = rules
        .create_or_open(&p(path), Disposition::Create)
        .expect("create");
    fixture.workspace.write_at(path, 0, content).expect("write");
    created.file_id
}

/// Criterion 1 — `replace = true` replaces: the destination answers with the source's content and the source name is
/// gone. Both halves are asserted, because "the destination has the right bytes" is also true when the source was
/// renamed *onto* a name that happened to already exist with those bytes.
#[test]
fn replace_true_moves_the_content_and_frees_the_source_name() {
    common::strict_gate();
    let fixture = fixture("c1");
    let source = create_file(&fixture, "/src", b"source");
    let target = create_file(&fixture, "/dst", b"target");

    let outcome = rules(&fixture)
        .rename_with(&p("/src"), &p("/dst"), true)
        .expect("replace");
    assert_eq!(
        outcome,
        RenameOutcome {
            binding: (FileId::ROOT, b"dst".to_vec()),
            replaced: Some(target),
            file_id: source,
        },
        "cell outcome: the binding names the destination and the replaced inode is the old destination"
    );
    assert_eq!(
        fixture.workspace.read_at_id(source, 0, 6).expect("read"),
        b"source",
        "cell content: the destination now holds the source's bytes"
    );
    assert!(
        matches!(
            rules(&fixture).resolve(&p("/src")),
            Err(NamespaceError::NotFound { .. })
        ),
        "cell source: the source name is gone"
    );
    assert!(
        fixture.workspace.read_at_id(target, 0, 1).is_err(),
        "cell replaced: the old destination is a tombstone, not a live inode"
    );
}

/// Criterion 2 — `replace = false` on a taken name is `NameCollision` **and nothing changed**: the durable store, the
/// parent's enumeration and the two inodes are identical before and after.
#[test]
fn replace_false_collision_changes_nothing() {
    common::strict_gate();
    let fixture = fixture("c2");
    let source = create_file(&fixture, "/src", b"source");
    let target = create_file(&fixture, "/dst", b"target");
    let before = fingerprint(&fixture, FileId::ROOT);

    let error = rules(&fixture)
        .rename_with(&p("/src"), &p("/dst"), false)
        .expect_err("a taken destination must be refused");
    assert!(
        matches!(error, NamespaceError::NameCollision { .. }),
        "cell type: the refusal is NameCollision, not a generic failure: {error:?}"
    );

    assert_eq!(
        fingerprint(&fixture, FileId::ROOT),
        before,
        "cell fingerprint: a refused rename must not touch the store or the directory"
    );
    assert_eq!(
        fixture.workspace.read_at_id(source, 0, 6).expect("source"),
        b"source"
    );
    assert_eq!(
        fixture.workspace.read_at_id(target, 0, 6).expect("target"),
        b"target"
    );
}

/// Criterion 4 — the returned `binding` is usable: `remove_at(binding, source)` removes exactly the moved entry, and the
/// bytes are the **raw** destination name (case preserved).
#[test]
fn the_binding_can_be_handed_to_remove_at() {
    common::strict_gate();
    let fixture = fixture("c4");
    let _source = create_file(&fixture, "/src", b"payload");
    rules(&fixture).mkdir(&p("/dir")).expect("mkdir");
    let dir = rules(&fixture).resolve(&p("/dir")).expect("dir id");

    let outcome = rules(&fixture)
        .rename_with(&p("/src"), &p("/dir/MiXeD"), true)
        .expect("rename");
    assert_eq!(
        outcome.binding,
        (dir, b"MiXeD".to_vec()),
        "cell binding: destination parent id plus the destination bytes verbatim (case preserved)"
    );

    rules(&fixture)
        .remove_at(outcome.binding.0, &outcome.binding.1, outcome.file_id)
        .expect("remove_at with the returned binding");
    assert!(
        matches!(
            rules(&fixture).resolve(&p("/dir/MiXeD")),
            Err(NamespaceError::NotFound { .. })
        ),
        "cell removal: the binding addressed the moved entry"
    );
    assert!(
        fixture.workspace.read_at_id(outcome.file_id, 0, 1).is_err(),
        "cell inode: remove_at with the binding tombstoned the moved inode"
    );
}

/// Criterion 5 — `replaced` is `None` when nothing was replaced, **including the self case**.
///
/// The self case is the one that can silently destroy data: a naive implementation reads "the destination name is taken
/// by id X" and retires X, which here *is* the inode being moved.
#[test]
fn replaced_is_none_unless_a_different_inode_lost_the_name() {
    common::strict_gate();
    let fixture = fixture("c5");
    let file = create_file(&fixture, "/A", b"body");

    // Same path twice: a no-op, no replacement, same FileId.
    let same = rules(&fixture)
        .rename_with(&p("/A"), &p("/A"), true)
        .expect("rename onto itself");
    assert_eq!(same.replaced, None, "cell x,x: nothing was replaced");
    assert_eq!(same.file_id, file, "cell x,x: the same inode, not a copy");
    assert_eq!(
        fixture.workspace.read_at_id(file, 0, 4).expect("read"),
        b"body",
        "cell x,x: the inode is still live and readable (retiring self would have emptied it)"
    );

    // Case-only rename with no occupant: `None`, FileId unchanged, name bytes updated.
    let case_only = rules(&fixture)
        .rename_with(&p("/A"), &p("/a"), true)
        .expect("case-only rename");
    assert_eq!(
        case_only.replaced, None,
        "cell case-only: nothing was replaced"
    );
    assert_eq!(case_only.file_id, file, "cell case-only: FileId unchanged");
    assert_eq!(
        case_only.binding,
        (FileId::ROOT, b"a".to_vec()),
        "cell case-only: the binding carries the new spelling"
    );
    // **Inverted (480).** This used to assert that the old spelling stopped resolving — true while bindings were raw bytes and the lookup compared
    // them as such. Lookup now matches on the folded key, so `/A` and `/a` are the same entry, which is what Windows does. The assertion therefore
    // becomes the **stronger** one: the old spelling still resolves (folded matching) *and* the entry now reports the new spelling (display kept).
    //
    // The old form only proved "the old shell is dead"; it said nothing about whether the new one is alive.
    let resolved = rules(&fixture)
        .resolve(&p("/A"))
        .expect("cell case-only: the old spelling still resolves to the same entry");
    assert_eq!(
        resolved, file,
        "cell case-only: same inode under either spelling"
    );
    let listing = rules(&fixture).read_dir(&p("/")).expect("list");
    let names: Vec<String> = listing.into_iter().map(|entry| entry.name).collect();
    assert_eq!(
        names,
        vec!["a".to_string()],
        "cell case-only: exactly one entry, listed under the new spelling"
    );
}

/// Criterion 6 — directory replacement: an empty destination directory is replaced and **its subtree keeps its ids**; a
/// non-empty one is refused with no outcome and no change.
#[test]
fn directory_replace_respects_emptiness_and_keeps_subtree_ids() {
    common::strict_gate();
    let fixture = fixture("c6");
    let rules = rules(&fixture);
    rules.mkdir(&p("/from")).expect("mkdir from");
    rules.mkdir(&p("/to")).expect("mkdir to");
    let empty_id = rules.resolve(&p("/to")).expect("to id");

    let outcome = rules
        .rename_with(&p("/from"), &p("/to"), true)
        .expect("replace an empty directory");
    assert_eq!(
        outcome.replaced,
        Some(empty_id),
        "cell empty target: the empty directory is the replaced inode"
    );

    // Now a non-empty destination blocks the move under both flags.
    let rules = self::rules(&fixture);
    rules.mkdir(&p("/full")).expect("mkdir full");
    let child = create_file(&fixture, "/full/child", b"kept");
    let full_id = rules.resolve(&p("/full")).expect("full id");
    let before = fingerprint(&fixture, FileId::ROOT);

    let error = rules
        .rename_with(&p("/to"), &p("/full"), true)
        .expect_err("a non-empty destination blocks the move");
    assert!(
        matches!(error, NamespaceError::DirectoryNotEmpty { .. }),
        "cell non-empty: DirectoryNotEmpty (R6): {error:?}"
    );
    assert_eq!(
        fingerprint(&fixture, FileId::ROOT),
        before,
        "cell non-empty: refused means unchanged"
    );
    assert_eq!(
        fixture.workspace.read_at_id(child, 0, 4).expect("child"),
        b"kept",
        "cell non-empty: the destination's child is untouched"
    );
    assert!(
        fixture.workspace.read_dir_id(full_id, None, 8).is_ok(),
        "cell non-empty: the destination directory is still live"
    );
}

/// Criterion 7 — a case-only rename onto a name held by **another** inode is `NameCollision` with `replace = false` and
/// a replacement with `replace = true`.
#[test]
fn two_spellings_of_one_name_cannot_coexist_and_rename_replaces_ordinals() {
    common::strict_gate();
    let fixture = fixture("c7");

    // **Rewritten (480).** The original premise was that `/A` and `/a` can be two different inodes, and it then asked what a case-only rename
    // does when the folded name is taken. That premise is now unreachable through the API — lookup compares folded keys, so the second create
    // finds the first entry, which is exactly the protection 471 asks for. The half that premise was carrying moves to a positive criterion.
    let upper = create_file(&fixture, "/A", b"upper");
    let collision = rules(&fixture)
        .create_or_open(&p("/a"), Disposition::Create)
        .expect_err("the folded name is already taken");
    assert!(
        matches!(collision, NamespaceError::AlreadyExists { .. }),
        "cell coexist: AlreadyExists: {collision:?}"
    );

    // The replace flag still has its ordinary work to do — on names that differ by more than case, which is the standard rename path on Windows.
    create_file(&fixture, "/ab", b"lower");
    let error = rules(&fixture)
        .rename_with(&p("/A"), &p("/ab"), false)
        .expect_err("replace = false refuses a taken name");
    assert!(
        matches!(error, NamespaceError::NameCollision { .. }),
        "cell false: NameCollision: {error:?}"
    );

    let outcome = rules(&fixture)
        .rename_with(&p("/A"), &p("/ab"), true)
        .expect("replace");
    assert_eq!(
        outcome.file_id, upper,
        "cell true: the source inode moved onto the destination name"
    );
    assert!(
        outcome.replaced.is_some(),
        "cell true: the previous occupant lost the name"
    );
    assert_eq!(
        fixture.workspace.read_at_id(upper, 0, 5).expect("upper"),
        b"upper",
        "cell true: the surviving name holds the source's bytes"
    );
}

/// Criterion 8 — every refusal path returns a typed error and **no outcome**: cross-type, non-empty directory, into its
/// own subtree, and the `replace = false` collision. The store fingerprint is part of each assertion.
#[test]
fn refusals_are_typed_and_leave_no_trace() {
    common::strict_gate();
    let fixture = fixture("c8");
    let rules = rules(&fixture);
    rules.mkdir(&p("/dir")).expect("mkdir");
    create_file(&fixture, "/file", b"file");
    rules.mkdir(&p("/dir/sub")).expect("mkdir sub");
    let before = fingerprint(&fixture, FileId::ROOT);

    for (from, to, replace) in [
        ("/file", "/dir", true),           // R4/R5: file onto directory
        ("/dir", "/file", true),           // R4/R5: directory onto file
        ("/dir", "/dir/sub/deeper", true), // R9: into its own subtree
        ("/file", "/dir", false),          // replace = false, name taken
    ] {
        let error = rules
            .rename_with(&p(from), &p(to), replace)
            .expect_err("this rename must be refused");
        // The **variant** is asserted, never the message text: a message can be reworded, the variant cannot.
        assert!(
            matches!(
                error,
                NamespaceError::AlreadyExists { .. }
                    | NamespaceError::NameCollision { .. }
                    | NamespaceError::IntoOwnSubtree { .. }
                    | NamespaceError::DirectoryNotEmpty { .. }
                    | NamespaceError::NotFound { .. }
            ),
            "cell {from} -> {to}: a typed refusal, got {error:?}"
        );
        assert_eq!(
            fingerprint(&fixture, FileId::ROOT),
            before,
            "cell {from} -> {to}: a refusal must not change the store"
        );
    }
}

/// Criterion 3 and 9 — twenty rounds of two concurrent `replace = false` renames onto the same free name: exactly one
/// wins per round, the winner's outcome matches the final directory, and the loser neither wins nor changes anything.
///
/// The counter is deliberately **not** `>= 1`: a criterion that accepts "both succeeded" is not testing exclusion, so
/// each round asserts the pair (winners == 1, losers == 1) and the final view.
#[test]
fn concurrent_replace_false_renames_have_exactly_one_winner() {
    common::strict_gate();
    let fixture = fixture("c9");
    const ROUNDS: usize = 20;
    let mut wins = 0usize;
    let mut losses = 0usize;

    for round in 0..ROUNDS {
        let target = format!("/t{round}");
        let left = create_file(&fixture, &format!("/l{round}"), b"left");
        let right = create_file(&fixture, &format!("/r{round}"), b"right");
        let barrier = std::sync::Barrier::new(2);

        let results: Vec<(FileId, Result<RenameOutcome, NamespaceError>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = [left, right]
                    .into_iter()
                    .map(|id| {
                        let barrier = &barrier;
                        let rules = rules(&fixture);
                        let from = if id == left {
                            Path::parse(&format!("/l{round}")).expect("path")
                        } else {
                            Path::parse(&format!("/r{round}")).expect("path")
                        };
                        let to = Path::parse(&target).expect("path");
                        scope.spawn(move || {
                            barrier.wait();
                            let outcome = rules.rename_with(&from, &to, false);
                            (id, outcome)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("join"))
                    .collect()
            });

        let winners: Vec<_> = results
            .iter()
            .filter(|(_, outcome)| outcome.is_ok())
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "round {round}: exactly one replace = false rename may win, got {}",
            winners.len()
        );
        wins += 1;
        losses += results.len() - winners.len();

        let (winner_id, outcome) = winners[0];
        let outcome = outcome.as_ref().expect("winner");
        assert_eq!(
            outcome.replaced, None,
            "round {round}: nothing was replaced, so no inode may be retired"
        );
        assert_eq!(
            rules(&fixture).resolve(&p(&target)).expect("resolve"),
            *winner_id,
            "round {round}: the winner's outcome matches the final directory"
        );
        // `read_at_id` pads short reads with zeros up to the requested length (that is the EOF contract), so the
        // request length has to be the *expected* length rather than a fixed 5 — the first version asked for 5 bytes
        // from a 4-byte file and compared the zero pad as data.
        let expected: &[u8] = if *winner_id == left {
            b"left"
        } else {
            b"right"
        };
        assert_eq!(
            fixture
                .workspace
                .read_at_id(*winner_id, 0, expected.len())
                .expect("winner readable"),
            expected,
            "round {round}: the winner's bytes are intact"
        );
    }

    assert_eq!(
        (wins, losses),
        (ROUNDS, ROUNDS),
        "cell totals: every round produced one winner and one loser"
    );
}
