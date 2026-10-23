//! **ADR-006 §18-1** — the random reference-model sequence.
//!
//! > 内存参考模型（路径 → 字节/目录）对照 workspace，随机 create/mkdir/write_at/truncate/
//! > rename/unlink/rmdir 序列，每步逐字节核对 stat/read_dir/read_at
//!
//! The model is in `reference_model/support.rs`: an **independent** implementation of the
//! same specification, not a second copy of `fs_namespace::Rules`. If both sides shared
//! code, this suite would only prove that the code agrees with itself — and P1's
//! `holes`/`shape_transitions` suites found real drift precisely because they did not.
//!
//! Every step of every sequence is checked in full: every path's `stat`, every directory's
//! `read_dir`, every file's bytes. A disagreement means the two implementations read the
//! specification differently.
//!
//! # Phase
//!
//! This is **phase 1**: the seven path operations, no `flush`/`snapshot`/`commit`, no crash.
//! Phases 2 and 3 (insert transitions, then a single crash-and-reopen per sequence) wait for
//! the P2b ADR to confirm their shape — see `docs/process/p2b-reference-model-prep.md`.
//!
//! # Reproducing a failure
//!
//! Every assertion message starts with `seed=<n> step=<k> op=<label>`, and the sequence is
//! fully determined by its seed, so a failure replays by rerunning the same test.

#[path = "reference/support.rs"]
mod reference;

use fs_api::ApiError;

use reference::{Fixture, Op, Reference, Refusal, Rng};

/// Steps in a routine sequence.
const STEPS: usize = 200;

/// **The sequence is the test.** Each seed is a different random walk over the same rules.
#[test]
fn random_sequences_match_the_reference_model() {
    for seed in 0..40u64 {
        run_sequence(seed, STEPS);
    }
}

/// A longer soak, for a nightly run or an explicit verification.
#[test]
#[ignore = "soak: 10 sequences x 1000 steps; run explicitly with --ignored"]
fn a_longer_soak_matches_the_reference_model() {
    for seed in 1000..1010u64 {
        run_sequence(seed, 1000);
    }
}

/// One random walk, checked step by step against the model.
fn run_sequence(seed: u64, steps: usize) {
    let fixture = Fixture::new(seed);
    let workspace = fixture.repo.workspace_create("w1", "main").expect("create");
    let mut model = Reference::new();
    let mut random = Rng::new(seed);

    for step in 0..steps {
        let op = reference::random_op(&mut random, &model);
        let expected = model.apply(&op);
        let actual = reference::perform(&workspace, &op);

        match (&expected, &actual) {
            (Ok(()), Ok(())) => {}
            (Ok(()), Err(error)) => panic!(
                "seed={seed} step={step} op={} {op:?}: the reference allows it, the workspace refused: {error:?}",
                op.label()
            ),
            (Err(refusal), Ok(())) => panic!(
                "seed={seed} step={step} op={} {op:?}: the reference refuses it ({refusal:?}), the workspace allowed it",
                op.label()
            ),
            (Err(expected), Err(actual)) => assert!(
                same_refusal(*expected, actual),
                "seed={seed} step={step} op={} {op:?}: refusal disagrees, reference {expected:?} vs workspace {actual:?}",
                op.label()
            ),
        }

        reference::assert_tree_matches(&workspace, &model, seed, step, &op);
    }
    workspace.close().expect("close");
}

/// Whether the workspace's error is the same *category* of refusal as the model's.
///
/// Only the category is compared. The two implementations may phrase a refusal differently,
/// and pinning the exact variant would turn this suite into a test of spelling.
fn same_refusal(expected: Refusal, actual: &ApiError) -> bool {
    let category = category_of(actual);
    // "Not a directory" and "is a directory" name the same disagreement from two sides: the
    // model picks whichever its own check needed, the workspace whichever its path walk hit.
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
        // The rules run behind three layers, and which one wraps a refusal depends on
        // which entry point was used: unwrap them all so the category is comparable.
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

/// `Op` is re-exported through the module for the driver's signature.
const _: fn(&Op) -> &'static str = Op::label;
