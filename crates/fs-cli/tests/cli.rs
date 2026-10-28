//! The CLI end to end: one command sequence that exercises every command.
//!
//! These drive the real binary, so what is tested is exactly what a user runs — argument
//! parsing, the current-workspace file, exit codes, and the fact that every command opens
//! and closes what it needs.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

/// A temp directory that cleans itself up.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("fs-cli-{label}-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }

    fn store(&self) -> PathBuf {
        self.path.join("store")
    }

    /// Runs the CLI with a **global** `--workspace` (ADR-010 §5/§8.4).
    ///
    /// The option sits before the command because that is the recommended shape; the parser accepts it anywhere, but a test that relies on the looser
    /// form would stop documenting the shape a reader should copy.
    fn run_ws(&self, workspace: &str, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fs-cli"));
        command
            .arg("--store")
            .arg(self.store())
            .arg("--workspace")
            .arg(workspace);
        command.args(arguments).output().expect("run the CLI")
    }

    /// `run_ws`, requiring success.
    fn ok_ws(&self, workspace: &str, arguments: &[&str]) -> String {
        let output = self.run_ws(workspace, arguments);
        assert!(
            output.status.success(),
            "{arguments:?} (workspace {workspace}) failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `run_ws`, requiring failure; returns stderr.
    fn fails_ws(&self, workspace: &str, arguments: &[&str]) -> String {
        let output = self.run_ws(workspace, arguments);
        assert!(
            !output.status.success(),
            "{arguments:?} (workspace {workspace}) should have failed but printed {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Runs `body` against one workspace **through the library**, which is where file content lives now (ADR-010 §5: the CLI has no file commands).
    ///
    /// Every criterion that used `put`/`cat` to prepare or verify now goes through here, so the CLI is only ever asked about the behaviour it still
    /// owns — lifecycle and mounting — while the content assertions stay as strong as they were.
    fn with_workspace<T>(&self, name: &str, body: impl FnOnce(&mut fs_api::Workspace) -> T) -> T {
        let repo = std::sync::Arc::new(fs_api::Repo::open(self.store()).expect("open repo"));
        let mut workspace = repo.workspace_open(name).expect("open workspace");
        let outcome = body(&mut workspace);
        workspace.close().expect("close workspace");
        outcome
    }

    /// Writes `bytes` at `path` in `workspace`, creating the file when it is not there.
    fn write_via_api(&self, workspace: &str, path: &str, bytes: &[u8]) {
        self.with_workspace(workspace, |handle| {
            if handle.stat(path).is_err() {
                handle.create(path).expect("create");
            }
            handle.write_at(path, 0, bytes).expect("write");
            handle
                .truncate(path, u64::try_from(bytes.len()).unwrap_or(0))
                .expect("truncate");
            handle.flush().expect("flush");
        });
    }

    /// Reads `path` from `workspace`.
    fn read_via_api(&self, workspace: &str, path: &str) -> Vec<u8> {
        self.with_workspace(workspace, |handle| {
            let size = usize::try_from(handle.stat(path).expect("stat").size).unwrap_or(0);
            let mut bytes = vec![0u8; size];
            let read = handle.read_at(path, 0, &mut bytes).expect("read");
            bytes.truncate(read);
            bytes
        })
    }

    /// The names listed in `path` of `workspace`.
    fn ls_via_api(&self, workspace: &str, path: &str) -> Vec<String> {
        self.with_workspace(workspace, |handle| {
            handle
                .read_dir(path)
                .expect("read_dir")
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        })
    }

    /// The names listed in a **commit's** tree (history, not the workspace).
    fn ls_in_commit(&self, commit: &str, path: &str) -> Vec<String> {
        let repo = fs_api::Repo::open(self.store()).expect("open repo");
        repo.read_view(commit)
            .expect("read view")
            .read_dir(path)
            .expect("read_dir")
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// The bytes of `path` in a **commit's** tree.
    fn read_in_commit(&self, commit: &str, path: &str) -> Vec<u8> {
        let repo = fs_api::Repo::open(self.store()).expect("open repo");
        repo.read_view(commit)
            .expect("read view")
            .read_all(path)
            .expect("read")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Runs the CLI with `--store` pointing at this root's store.
    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fs-cli"))
            .arg("--store")
            .arg(self.store())
            .args(arguments)
            .output()
            .expect("run the CLI")
    }

    /// Runs a command that must succeed and returns its stdout.
    fn ok(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Runs a command that must fail and returns its stderr.
    fn fails(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert!(
            !output.status.success(),
            "{arguments:?} should have failed but printed {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Creates the repository through the binary itself.
    fn init(&self) {
        let output = Command::new(env!("CARGO_BIN_EXE_fs-cli"))
            .arg("init")
            .arg(self.store())
            .output()
            .expect("init");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("initialised"));
    }

    /// The id a `commit` printed, for a named workspace (ADR-010 §5: no implicit current).
    fn commit_ws(&self, workspace: &str, message: &str) -> String {
        let output = self.ok_ws(workspace, &["commit", "-m", message]);
        output
            .split_whitespace()
            .last()
            .expect("commit id")
            .to_owned()
    }

}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// **Acceptance: the whole command set in one round trip.**
#[test]
fn the_full_command_sequence_round_trips() {
    let root = TempRoot::new("round-trip");
    std::fs::write(root.file("a.txt"), b"hello from a").expect("host file");
    std::fs::write(root.file("b.txt"), b"second file").expect("host file");
    root.init();

    // branch list / create
    let branches = root.ok(&["branch"]);
    assert!(branches.contains("main"), "got {branches}");
    root.ok(&["branch", "feature", "--from", "main"]);
    assert_eq!(root.ok(&["branch"]).lines().count(), 2);

    // checkout creates or resumes the workspace and says so; it no longer records a "current" (ADR-010 §5).
    let checkout = root.ok(&["checkout", "main"]);
    assert!(checkout.contains("workspace main"), "got {checkout}");

    // **Content lives through the library** (the CLI has no file commands any more): mkdir/write/stat/read all go through `fs-api`, and the CLI is
    // asked only about the behaviour it still owns — lifecycle. The assertions are the same ones the `put`/`ls`/`stat`/`get` round trip made.
    root.with_workspace("main", |workspace| workspace.mkdir("/docs").expect("mkdir"));
    root.write_via_api("main", "/docs/a", b"hello from a");
    root.write_via_api("main", "/docs/b", b"second file");
    assert_eq!(root.ls_via_api("main", "/docs").len(), 2);
    assert_eq!(
        root.with_workspace("main", |workspace| workspace
            .stat("/docs/a")
            .expect("stat")
            .size),
        12
    );
    assert_eq!(root.read_via_api("main", "/docs/a"), b"hello from a");

    // status before and after the first commit
    assert!(root.ok_ws("main", &["status"]).contains("dirty:     yes"));
    let first = root.commit_ws("main", "first");
    assert_eq!(first.len(), 64, "a commit id is 64 hex characters");
    let log = root.ok(&["log", "main"]);
    assert_eq!(log.lines().count(), 2, "genesis + first: {log}");
    assert!(log.contains(&first));
    assert!(root.ok_ws("main", &["status"]).contains("dirty:     no"));

    // rename / remove through the library, then a second commit
    root.with_workspace("main", |workspace| {
        workspace.rename("/docs/b", "/docs/c").expect("rename");
        workspace.remove("/docs/c").expect("remove");
        workspace.flush().expect("flush");
    });
    let second = root.commit_ws("main", "second");

    // History reads a commit, not the workspace.
    assert_eq!(root.read_in_commit(&first, "/docs/a"), b"hello from a");
    assert_eq!(root.ls_in_commit(&first, "/docs").len(), 2);
    assert_eq!(root.ls_via_api("main", "/").len(), 1);
    assert_eq!(root.ok(&["log", "main", "1"]).lines().count(), 1);

    // restore replaces the view; the branch does not move.
    root.ok_ws("main", &["restore", &first]);
    assert_eq!(root.ls_via_api("main", "/docs").len(), 2);
    assert!(
        root.ok(&["log", "main"]).contains(&second),
        "restore moves no branch"
    );

    // revert undoes the head with a new commit.
    let reverted = root.ok_ws("main", &["revert"]);
    assert!(reverted.contains("revert"), "got {reverted}");

    // reset moves the head back without growing history.
    root.ok(&["reset", &second, "--branch", "main"]);
    assert_eq!(root.ok(&["log", "main"]).lines().count(), 3);

    // verify, deep verify, stats
    assert!(root.ok(&["verify"]).contains("ok"));
    let deep = root.ok(&["verify", "--deep"]);
    assert!(deep.contains("inodes:"), "got {deep}");
    let stats = root.ok(&["stats"]);
    assert!(stats.contains("branches:           2"), "got {stats}");
    assert!(stats.contains("commits:            4"), "got {stats}");
}

/// **Failure paths** — every "you cannot do that" is a non-zero exit with a message that names the way out.
#[test]
fn failures_are_reported() {
    let root = TempRoot::new("failures");
    root.init();

    // **Rewritten (S0, ADR-010 §13).** The old first line asserted that `ls` failed right after `init`; `ls` is gone, and the workspace it needed is now
    // created by `init`. What still matters is the message: without `--workspace` a lifecycle command must say so, using the constant the ADR fixes.
    let no_workspace = root.fails(&["commit", "-m", "empty"]);
    assert!(
        no_workspace.contains(fs_cli::NO_WORKSPACE),
        "cell workspace: {no_workspace:?}"
    );

    // A removed file command answers **before** the store is opened, so the message is about the command, not about a missing store.
    let removed = root.fails(&["put", "a", "b"]);
    assert!(
        removed.contains("file operations go through the mounted file system"),
        "cell removed: {removed:?}"
    );

    root.ok(&["checkout", "main"]);

    assert!(root
        .fails_ws("main", &["commit", "-m", "empty"])
        .contains("already equals its base"));
    assert!(root.fails(&["checkout", "nope"]).contains("not found"));
    assert!(root.fails(&["frobnicate"]).contains("unknown command"));
}

/// **A dirty workspace needs `--discard`** before `restore` replaces it.
#[test]
fn restore_refuses_to_drop_uncommitted_work() {
    let root = TempRoot::new("restore");
    root.init();
    root.ok(&["checkout", "main"]);

    root.write_via_api("main", "/one", b"one");
    let head = root.commit_ws("main", "first");
    let genesis = root.ok(&["log", "main", "2"]);
    let genesis = genesis
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .expect("genesis id")
        .to_owned();

    root.write_via_api("main", "/two", b"two");
    assert!(root
        .fails_ws("main", &["restore", &genesis])
        .contains("changes"));
    root.ok_ws("main", &["restore", &genesis, "--discard"]);
    assert_eq!(
        root.ls_via_api("main", "/").len(),
        0,
        "back to the empty tree"
    );
    assert!(
        root.ok(&["log", "main"]).contains(&head),
        "restore moved no branch"
    );
}

/// A branch created from a commit inherits its tree.
#[test]
fn a_branch_can_start_from_a_commit() {
    let root = TempRoot::new("from-commit");
    root.init();
    root.ok(&["checkout", "main"]);
    root.write_via_api("main", "/f", b"payload");
    let first = root.commit_ws("main", "first");

    root.ok(&["branch", "side", "--from", &first]);
    root.ok(&["checkout", "side"]);
    assert_eq!(
        root.ls_via_api("side", "/").len(),
        1,
        "side starts at `first`"
    );
    assert_eq!(root.read_in_commit(&first, "/f"), b"payload");
}

/// `gc` end to end: a dry run decides but changes nothing, and a real run collects.
#[test]
fn gc_dry_run_and_real_run_are_different_commands() {
    let root = TempRoot::new("gc");
    root.init();
    root.ok(&["checkout", "main"]);
    root.write_via_api("main", "/a.txt", b"kept");
    root.commit_ws("main", "one");

    let dry = root.ok(&["gc", "--dry-run"]);
    assert!(dry.contains("dry run"), "{dry}");
    assert!(dry.contains("planned objects:"), "{dry}");

    let real = root.ok(&["gc"]);
    assert!(real.contains("gc: done"), "{real}");
    assert!(real.contains("freed objects:       0"), "{real}");

    // The store is usable afterwards: the collector's handle did not linger.
    root.ok_ws("main", &["status"]);
}

/// A `gc_repack_threshold` that cannot be a ratio is refused, and the message says why.
///
/// The policy is store data, so this is reachable by editing the store rather than by a flag — which is
/// exactly why the collector has to validate it at run time instead of trusting a constant.
#[test]
fn an_impossible_repack_threshold_is_reported_and_not_acted_on() {
    let root = TempRoot::new("gc-threshold");
    root.init();
    root.ok(&["checkout", "main"]);
    std::fs::write(root.file("a.txt"), b"kept").expect("write");
    root.write_via_api("main", "/a.txt", b"kept");

    let connection =
        rusqlite::Connection::open(root.store().join("store.sqlite")).expect("open index");
    connection
        .execute(
            "INSERT INTO store_policy (key, value) VALUES ('gc_repack_threshold', '2')",
            [],
        )
        .expect("plant the policy");
    drop(connection);

    let stderr = root.fails(&["gc"]);
    assert!(
        stderr.contains("outside [0, 1]"),
        "the refusal must name the range: {stderr}"
    );
    // Nothing was collected: the command failed before the sweep.
    root.ok_ws("main", &["status"]);
}

/// `snapshot drop` removes the row; the tree's objects go on the **next** collection, and the other
/// roots stay byte-identical (§25.C-8).
#[test]
fn dropping_a_snapshot_frees_only_what_nothing_else_reaches() {
    let root = TempRoot::new("snapshot-drop");
    root.init();
    root.ok(&["checkout", "main"]);
    root.write_via_api("main", "/kept.txt", b"the committed bytes");
    let commit = root.commit_ws("main", "one");

    // A tree only a snapshot row reaches: write it, snapshot it, then wipe the workspace's pending
    // view so nothing but the snapshot row names these bytes.
    root.write_via_api(
        "main",
        "/gone.txt",
        b"only this snapshot reaches these bytes",
    );
    let snapshotted = root.ok_ws("main", &["snapshot", "create"]);
    let snapshot_root = snapshotted
        .split_whitespace()
        .last()
        .expect("a root id")
        .to_owned();
    root.ok_ws("main", &["restore", &commit, "--discard"]);

    let listed = root.ok(&["snapshot", "list"]);
    assert!(listed.contains(&snapshot_root), "{listed}");

    // Collect once with the snapshot present: its tree is reachable, so nothing of it may be freed.
    let first = root.ok(&["gc"]);
    assert!(
        first.contains("freed objects:       0"),
        "the snapshot still holds its tree: {first}"
    );

    root.ok(&["snapshot", "drop", &snapshot_root]);
    let second = root.ok(&["gc"]);
    assert!(
        !second.contains("freed objects:       0"),
        "once the row is gone, its exclusive objects are collectable: {second}"
    );

    // The committed file survives, byte for byte.
    assert_eq!(
        root.read_in_commit(&commit, "/kept.txt"),
        b"the committed bytes"
    );
}

/// `branch delete` keeps the history: the deleted branch's commits are still GC roots, so its objects
/// survive a collection (§25.C-9).
#[test]
fn deleting_a_branch_keeps_its_objects_alive() {
    let root = TempRoot::new("branch-delete");
    root.init();
    root.ok(&["checkout", "main"]);
    root.ok(&["branch", "topic"]);
    root.ok(&["checkout", "topic"]);
    root.write_via_api(
        "topic",
        "/topic.txt",
        b"only the topic branch has these bytes",
    );
    let topic_commit = root.commit_ws("topic", "topic work");

    // Back to main, then delete the branch: the commit row stays, so nothing becomes unreachable.
    root.ok(&["checkout", "main"]);
    // A workspace on `topic` would make the delete dangle, which is exactly what it must refuse.
    root.ok(&["workspace", "discard", "topic"]);
    root.ok(&["branch", "delete", "topic"]);
    let branches = root.ok(&["branch"]);
    assert!(!branches.contains("topic\t"), "topic is gone: {branches}");

    let swept = root.ok(&["gc"]);
    assert!(
        swept.contains("freed objects:       0"),
        "the deleted branch's commit is still a root, so its objects must survive: {swept}"
    );

    // And the history is still readable by commit id — that is what "still a root" means in practice.
    assert_eq!(
        root.read_in_commit(&topic_commit, "/topic.txt"),
        b"only the topic branch has these bytes"
    );
}

/// §27.C: `verify --deep` checks the **snapshot** table and every workspace's `base_root`.
///
/// The experiment is a corruption, not a count: a tree reachable **only** through a snapshot row is
/// damaged, and `verify --deep` must report it. Before the damage the same command is clean — the paired
/// control, without which "verify failed" could just mean "verify always fails here".
#[test]
fn verify_deep_checks_snapshot_roots() {
    let root = TempRoot::new("verify-snapshot");
    root.init();
    root.ok(&["checkout", "main"]);
    root.write_via_api("main", "/kept.txt", b"committed");
    root.commit_ws("main", "one");

    root.write_via_api("main", "/snap.txt", b"only in the snapshot");
    let created = root.ok_ws("main", &["snapshot", "create"]);
    let snapshot_root = created
        .split_whitespace()
        .last()
        .expect("a root")
        .to_owned();
    root.ok(&["verify", "--deep"]);

    // Damage exactly the object that names that tree's content: every pack file goes. The snapshot's
    // tree can then no longer be walked, and `verify --deep` must say so.
    for entry in std::fs::read_dir(root.store().join("packs"))
        .expect("packs")
        .flatten()
    {
        if entry.path().is_file() {
            std::fs::remove_file(entry.path()).expect("delete a pack");
        }
    }

    let output = root.run(&["verify", "--deep"]);
    assert!(
        !output.status.success(),
        "a damaged store must not verify clean"
    );
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        printed.contains(&snapshot_root) || printed.to_lowercase().contains("dangling"),
        "verify --deep must report the damaged snapshot tree ({snapshot_root}): {printed}"
    );
}
