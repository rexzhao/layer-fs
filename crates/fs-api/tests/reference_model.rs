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
//! fully determined by its seed, so a failure replays by rerunning the same test. For a
//! failure seen in the 10×1000 soak, replay just that seed in seconds (M9), optionally
//! printing every step (the first byte that differs is in the trace):
//!
//! ```text
//! WCAS_SEED=1000 WCAS_STEPS=1000 WCAS_TRACE=1 \
//!   cargo test --release -p fs-api --test reference_model a_single_seed_can_be_replayed -- --nocapture
//! ```
//!
//! The soak itself is `--ignored` and runs **every** seed before failing, listing the first
//! failing step of each; one run therefore answers "how many shapes are still broken":
//!
//! ```text
//! cargo test --release -p fs-api --test reference_model -- --ignored
//! ```

#[path = "reference/support.rs"]
mod reference;

use fs_api::{ApiError, Workspace};

use reference::{Fixture, Op, Reference, Refusal, Rng};

/// Steps in a routine sequence.
const STEPS: usize = 200;

/// The soak's seeds: 10 × 1000 steps.
const SOAK_SEEDS: std::ops::Range<u64> = 1000..1010;

/// Steps in one soak sequence.
const SOAK_STEPS: usize = 1000;

/// **The sequence is the test.** Each seed is a different random walk over the same rules.
#[test]
fn random_sequences_match_the_reference_model() {
    for seed in 0..40u64 {
        run_sequence(seed, STEPS);
    }
}

/// A longer soak, for a nightly run or an explicit verification.
///
/// Every seed runs: a failure is **collected**, not thrown, so one run answers "how many
/// shapes are still broken" instead of stopping at the first. Seed 1000 failed alone on the
/// first R3-1 run, and the fix could only be called complete once the other nine had also
/// been seen to pass.
#[test]
#[ignore = "soak: 10 sequences x 1000 steps; run explicitly with --ignored"]
fn a_longer_soak_matches_the_reference_model() {
    let seeds: Vec<u64> = SOAK_SEEDS.collect();
    let failures = soak(&seeds, SOAK_STEPS);
    assert!(
        failures.is_empty(),
        "{} of {} soak seeds failed:\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n")
    );
}

/// Runs every seed to completion, returning one line per failure.
///
/// A panic is the suite's only failure channel (`assert_tree_matches`), so each sequence runs
/// under `catch_unwind` and the message is taken from the payload. Deliberately **no panic
/// hook**: a hook is process-global, so silencing it to keep the output short would also
/// silence a test running in parallel — `--include-ignored` runs the soak beside the routine
/// sequences, and a nightly failure must never go mute. libtest printing the message a second
/// time is the cheap side of that trade.
fn soak(seeds: &[u64], steps: usize) -> Vec<String> {
    let mut failures = Vec::new();
    for &seed in seeds {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_sequence(seed, steps);
        }));
        if let Err(payload) = outcome {
            failures.push(summarise(seed, payload.as_ref()));
        }
    }
    failures
}

/// The first line of a panic, which is where `seed=… step=… op=…` lives.
fn summarise(seed: u64, payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("(non-string panic payload)");
    let line = message.lines().next().unwrap_or(message);
    let line: String = line.chars().take(240).collect();
    format!("seed={seed}: {line}")
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
        trace(seed, step, &op, &expected, &actual, &model, &workspace);

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

// ----------------------------------------------------------------------------- M9

/// The seed to replay, when `WCAS_SEED` asks for one sequence only.
fn replay_seed() -> Option<u64> {
    std::env::var("WCAS_SEED").ok()?.parse().ok()
}

/// How many steps the replay runs; `WCAS_STEPS` overrides the routine length.
fn replay_steps() -> usize {
    std::env::var("WCAS_STEPS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(STEPS)
}

/// M9: `WCAS_SEED=<n>` replays exactly one seed.
///
/// Without the variable this test does nothing, so the suite stays fast; with it, a failure
/// reported as `seed=… step=…` — including one from the 10×1000 soak — replays in a second
/// instead of the soak's half-minute: `WCAS_SEED=1000 WCAS_STEPS=1000 cargo test --release
/// -p fs-api --test reference_model a_single_seed_can_be_replayed`. Add `WCAS_TRACE=1` to
/// see every step.
#[test]
fn a_single_seed_can_be_replayed() {
    let Some(seed) = replay_seed() else {
        return;
    };
    run_sequence(seed, replay_steps());
}

/// `WCAS_TRACE=1` prints every step and the shape of the path it touched (M9's companion).
///
/// The seed alone reproduces a sequence, but a byte mismatch says nothing about *which* step
/// introduced it. Printing the model's size against the workspace's, and the first byte
/// that differs, localises it without a debugger.
fn trace(
    seed: u64,
    step: usize,
    op: &Op,
    expected: &Result<(), Refusal>,
    actual: &Result<(), ApiError>,
    model: &Reference,
    workspace: &Workspace,
) {
    if std::env::var_os("WCAS_TRACE").is_none() {
        return;
    }
    eprintln!(
        "seed={seed} step={step} {:<9} model={:<14} ws={:<14} {op:?} | {}",
        op.label(),
        format!("{expected:?}"),
        format!("{:?}", actual.as_ref().err()),
        shape(op, model, workspace),
    );
}

/// The state of the path an operation touched: kind, size, and first differing byte.
fn shape(op: &Op, model: &Reference, workspace: &Workspace) -> String {
    let path = match op {
        Op::Mkdir(path) | Op::Create(path) | Op::Unlink(path) | Op::Rmdir(path) => path,
        Op::WriteAt { path, .. } | Op::Truncate { path, .. } => path,
        Op::Rename { from, .. } => from,
    };
    let Some(node) = model.get(path) else {
        return format!("{path}: absent in model");
    };
    let Ok(stat) = workspace.stat(path) else {
        return format!("{path}: {node:?} in model, absent in workspace");
    };
    match node {
        reference::Node::Dir => format!("{path}: dir, workspace {kind:?}", kind = stat.kind),
        reference::Node::File(expected) => {
            let mut buffer = vec![0u8; expected.len()];
            let Ok(read) = workspace.read_at(path, 0, &mut buffer) else {
                return format!("{path}: unreadable, workspace size {}", stat.size);
            };
            buffer.truncate(read);
            let first = expected
                .iter()
                .zip(&buffer)
                .position(|(left, right)| left != right);
            format!(
                "{path}: model size {} content {}, workspace size {} first_diff={first:?}",
                expected.len(),
                if first.is_some() { "differs" } else { "equal" },
                stat.size,
            )
        }
    }
}
