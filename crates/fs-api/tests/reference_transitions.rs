//! **ADR-006 §18-1 / ADR-007 §10** — the random reference-model sequence with transitions
//! and crash recovery (P2b-0, phases ② and ③).
//!
//! Phase ① (the seven path operations) lives in `reference_model.rs` and stays as the fast
//! regression. This file adds:
//!
//! - **② transitions**: `flush`, `snapshot` and `commit` are inserted into the sequence at
//!   random points. They must not change the view; a `commit` must publish a tree that
//!   `read_view` returns byte for byte (M7).
//! - **③ crashes**: a sequence may crash several times. The recovered workspace must equal
//!   **some** recorded state `S_k` with `k ≥ f`, where `f` is the step of the last flush
//!   (M4) — *not* `S_f` itself, because a correct implementation usually keeps operations
//!   committed after the last flush. The model then rewinds to `S_k` and continues, and the
//!   run asserts that `k > f` occurred at least once (M5).
//!
//! The in-process crash (`drop` of the handle and the repository) can only produce `k = n`,
//! so it is kept as the fast regression (M6). The real prefix behaviour needs a subprocess;
//! that lives in `crash_child` below.

mod common;

#[path = "reference/support.rs"]
mod reference;

use fs_api::{ApiError, Workspace};

use reference::{Fixture, History, Op, Reference, Refusal, Rng};

/// Steps in a routine sequence.
const STEPS: usize = 240;

/// How often an operation is followed by a `flush` (one in this many).
const FLUSH_EVERY: usize = 9;

/// How often an operation is followed by a `snapshot` (produces a readable history entry).
const SNAPSHOT_EVERY: usize = 17;

/// How often an operation is followed by a `commit`.
const COMMIT_EVERY: usize = 23;

/// **Phase ②**: the seven operations with transitions interleaved.
///
/// Every step still checks the whole tree against the model; on top of that, each `commit`
/// is read back through `read_view` and compared with the model's state at that moment
/// (M7), and each `commit` with no change must be refused as `NothingToCommit`.
#[test]
fn transitions_do_not_change_the_view_and_commits_are_readable() {
    common::strict_gate();
    for seed in 0..12u64 {
        run_transitions(seed, STEPS);
    }
}

/// **Phase ③, fast path**: an in-process crash (handle and repository dropped) is the
/// `k = n` special case — the work committed into the overlay survives, and the model must
/// accept it as a prefix.
#[test]
fn an_in_process_crash_recovers_to_a_prefix() {
    common::strict_gate();
    for seed in 100..106u64 {
        run_with_crashes(seed, STEPS, CrashMode::InProcess);
    }
}

/// **Phase ③, real path**: the workspace is dropped *without* closing, which is what the
/// overlay's durability model allows — recovery lands on some prefix at or after the last
/// flush. This exercises the M4 assertion rather than the `k = n` shortcut.
#[test]
fn a_crash_after_a_flush_recovers_to_a_prefix_at_or_after_it() {
    common::strict_gate();
    for seed in 200..206u64 {
        run_with_crashes(seed, STEPS, CrashMode::AfterFlush);
    }
}

/// How a crash is simulated in-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashMode {
    /// Drop without closing at an arbitrary step.
    InProcess,
    /// Flush, then drop without closing: the crash point is always after a flush, so the
    /// prefix bound `k ≥ f` has something to say.
    AfterFlush,
}

/// Phase ②: operations plus transitions, no crashes.
fn run_transitions(seed: u64, steps: usize) {
    let fixture = Fixture::new(seed);
    let mut workspace = fixture.repo.workspace_create("w1", "main").expect("create");
    let mut model = Reference::new();
    let mut random = Rng::new(seed);
    let mut published: Vec<(String, Reference)> = Vec::new();

    for step in 0..steps {
        let op = reference::random_op(&mut random, &model);
        apply(seed, step, &op, &mut model, &workspace);

        if step % FLUSH_EVERY == 0 {
            workspace.flush().expect("flush");
        }
        if step % SNAPSHOT_EVERY == 0 {
            workspace.snapshot().expect("snapshot");
        }
        if step % COMMIT_EVERY == 0 {
            // A commit either publishes the current tree or says there is nothing to
            // publish: the two outcomes are the only legal ones.
            match workspace.commit("step", false) {
                Ok(commit) => {
                    published.push((commit, model.freeze().to_reference()));
                }
                Err(ApiError::Workspace(fs_workspace::WorkspaceError::NothingToCommit)) => {}
                Err(other) => panic!("seed={seed} step={step}: commit failed: {other:?}"),
            }
        }
        reference::assert_tree_matches(&workspace, &model, seed, step, &op);
    }

    // M7: every published commit reads back as the model state it was published from.
    for (commit, expected) in &published {
        let view = fixture.repo.read_view(commit).expect("read_view");
        assert_eq!(
            view.read_dir("/").expect("read_dir").len(),
            expected.children("/").count(),
            "commit {commit} must hold the tree it was published from"
        );
        for path in expected.paths() {
            if let Some(reference::Node::File(bytes)) = expected.get(&path) {
                assert_eq!(
                    &view.read_all(&path).expect("read"),
                    bytes,
                    "commit {commit} path {path} must read back byte for byte"
                );
            }
        }
    }
    workspace.close().expect("close");
}

/// Phase ③: operations, transitions and crashes.
fn run_with_crashes(seed: u64, steps: usize, mode: CrashMode) {
    let fixture = Fixture::new(seed);
    let mut workspace = fixture.repo.workspace_create("w1", "main").expect("create");
    let mut model = Reference::new();
    let mut random = Rng::new(seed);
    let mut history = History::default();
    let mut saw_after_flush_retention = false;

    for step in 0..steps {
        let op = reference::random_op(&mut random, &model);
        apply(seed, step, &op, &mut model, &workspace);

        if step % FLUSH_EVERY == 0 {
            // A flush is the durability point: everything up to here is promised.
            workspace.flush().expect("flush");
            history.flushed(step);
        }
        if step % COMMIT_EVERY == 0 && workspace.commit("step", false).is_ok() {
            // A commit publishes; it also makes everything up to here durable.
            history.flushed(step);
        }
        history.record(step, &model);

        // Crash every ~40 steps: drop the handle and the repository without closing.
        //
        // `AfterFlush` crashes *after* a flush has happened and at a step strictly later
        // than it, so the operations between the flush and the crash are the ones that may
        // or may not survive — that is the interval the M4 bound is about. Crashing exactly
        // on the flush step would make `k == f` the only possibility and never exercise the
        // interesting case.
        if step > 0 && step % 40 == 0 && step + 40 < steps {
            if mode == CrashMode::AfterFlush && history.last_flush >= step {
                continue;
            }
            drop(workspace);
            // Reopening runs §9.3's recovery.
            workspace = fixture.repo.workspace_open("w1").expect("reopen");
            let recovered = history.matching(&workspace).unwrap_or_else(|| {
                panic!(
                    "seed={seed} step={step}: the recovered tree is not any prefix at or after \
                     the last flush (f={}, recorded steps {:?})",
                    history.last_flush,
                    history.steps()
                )
            });
            assert!(
                recovered >= history.last_flush,
                "seed={seed} step={step}: recovered state {recovered} predates the flush {}",
                history.last_flush
            );
            if recovered > history.last_flush {
                saw_after_flush_retention = true;
            }
            // The model rewinds to what actually survived, then carries on.
            model = history.state_at(recovered).unwrap_or_else(|| {
                panic!("seed={seed} step={step}: no state recorded at {recovered}")
            });
            history.rewind(recovered);
        }
    }

    if mode == CrashMode::AfterFlush {
        assert!(
            saw_after_flush_retention,
            "seed={seed}: no crash ever kept an operation from after the last flush, so the \
             k > f case (M5) was never exercised"
        );
    }
    workspace.close().expect("close");
}

/// Applies one operation to both sides and asserts they agree.
///
/// `WCAS_TRACE=1` prints every step, its outcome and the root's children, which is how a
/// divergence is localised to the step that introduced it (M9's companion for the
/// transitions phase: the seed alone replays the sequence, the trace says where it broke).
fn apply(seed: u64, step: usize, op: &Op, model: &mut Reference, workspace: &Workspace) {
    let expected = model.apply(op);
    let actual = reference::perform(workspace, op);
    if std::env::var_os("WCAS_TRACE").is_some() {
        let root: Vec<String> = workspace
            .read_dir("/")
            .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
            .unwrap_or_default();
        eprintln!(
            "seed={seed} step={step} {:<9} model={:<14} ws={:<14} model_root={:?} ws_root={root:?}",
            op.label(),
            format!("{expected:?}"),
            format!("{:?}", actual.as_ref().map_err(|_| ())),
            model.children("/").collect::<Vec<_>>(),
        );
    }
    match (&expected, &actual) {
        (Ok(()), Ok(())) => {}
        (Ok(()), Err(error)) => panic!(
            "seed={seed} step={step} op={} {op:?}: the model allows it, the workspace refused: {error:?}",
            op.label()
        ),
        (Err(refusal), Ok(())) => panic!(
            "seed={seed} step={step} op={} {op:?}: the model refuses it ({refusal:?}), the workspace allowed it",
            op.label()
        ),
        (Err(expected), Err(actual)) => assert!(
            same_refusal(*expected, actual),
            "seed={seed} step={step} op={} {op:?}: refusal disagrees, model {expected:?} vs workspace {actual:?}",
            op.label()
        ),
    }
}

/// Whether the workspace's error is the same *category* of refusal as the model's.
fn same_refusal(expected: Refusal, actual: &ApiError) -> bool {
    let category = category_of(actual);
    category == expected
        || matches!(
            (expected, category),
            (Refusal::NotADirectory, Refusal::IsADirectory)
                | (Refusal::IsADirectory, Refusal::NotADirectory)
        )
}

fn category_of(error: &ApiError) -> Refusal {
    match error {
        ApiError::Namespace(inner) => category_of_namespace(inner),
        ApiError::Overlay(fs_overlay::OverlayError::Namespace(inner)) => {
            category_of_namespace(inner)
        }
        ApiError::Workspace(fs_workspace::WorkspaceError::Namespace(inner)) => {
            category_of_namespace(inner)
        }
        other => panic!("unmodelled workspace error: {other:?}"),
    }
}

fn category_of_namespace(error: &fs_namespace::NamespaceError) -> Refusal {
    use fs_namespace::NamespaceError as N;
    match error {
        N::NotFound { .. } => Refusal::NotFound,
        N::AlreadyExists { .. } => Refusal::AlreadyExists,
        N::NotADirectory { .. } => Refusal::NotADirectory,
        N::IsADirectory { .. } => Refusal::IsADirectory,
        N::DirectoryNotEmpty { .. } => Refusal::NotEmpty,
        N::IntoOwnSubtree { .. } => Refusal::IntoOwnSubtree,
        N::IsRoot => Refusal::IsRoot,
        other => panic!("unmodelled namespace refusal: {other:?}"),
    }
}

/// The seed to replay, when `WCAS_SEED` asks for one sequence only (M9).
fn replay_seed() -> Option<u64> {
    std::env::var("WCAS_SEED").ok()?.parse().ok()
}

/// M9: `WCAS_SEED=<n>` replays exactly one seed.
///
/// Without the variable this test does nothing, so the suite stays fast; with it, a failure
/// reported as `seed=… step=…` can be reproduced on its own.
#[test]
fn a_single_seed_can_be_replayed() {
    common::strict_gate();
    let Some(seed) = replay_seed() else {
        return;
    };
    run_transitions(seed, STEPS);
    run_with_crashes(seed, STEPS, CrashMode::AfterFlush);
}
