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

    /// The id a `commit` printed.
    fn commit(&self, message: &str) -> String {
        let output = self.ok(&["commit", "-m", message]);
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

    // checkout makes a workspace current
    let checkout = root.ok(&["checkout", "main"]);
    assert!(checkout.contains("workspace main"), "got {checkout}");

    // mkdir / put / ls / stat / get
    root.ok(&["mkdir", "/docs"]);
    let a = root.file("a.txt").display().to_string();
    root.ok(&["put", &a, "/docs/a"]);
    let b = root.file("b.txt").display().to_string();
    root.ok(&["put", &b, "/docs/b"]);
    assert_eq!(root.ok(&["ls", "/docs"]).lines().count(), 2);
    assert!(root.ok(&["stat", "/docs/a"]).contains("size:      12"));
    let out = root.file("out.txt");
    root.ok(&["get", "/docs/a", &out.display().to_string()]);
    assert_eq!(std::fs::read(&out).expect("read back"), b"hello from a");

    // status before and after the first commit
    assert!(root.ok(&["status"]).contains("dirty:     yes"));
    let first = root.commit("first");
    assert_eq!(first.len(), 64, "a commit id is 64 hex characters");
    let log = root.ok(&["log"]);
    assert_eq!(log.lines().count(), 2, "genesis + first: {log}");
    assert!(log.contains(&first));
    assert!(root.ok(&["status"]).contains("dirty:     no"));

    // mv / rm, then a second commit
    root.ok(&["mv", "/docs/b", "/docs/c"]);
    root.ok(&["rm", "/docs/c"]);
    let second = root.commit("second");

    // History reads a commit, not the workspace.
    assert_eq!(
        root.ok(&["cat", &format!("{first}:/docs/a")]),
        "hello from a"
    );
    assert_eq!(
        root.ok(&["ls", &format!("{first}:/docs")]).lines().count(),
        2
    );
    assert_eq!(root.ok(&["ls"]).lines().count(), 1);
    assert_eq!(root.ok(&["log", "1"]).lines().count(), 1);

    // restore replaces the view; the branch does not move.
    root.ok(&["restore", &first]);
    assert_eq!(root.ok(&["ls", "/docs"]).lines().count(), 2);
    assert!(
        root.ok(&["log"]).contains(&second),
        "restore moves no branch"
    );

    // revert undoes the head with a new commit.
    let reverted = root.ok(&["revert"]);
    assert!(reverted.contains("revert"), "got {reverted}");

    // reset moves the head back without growing history.
    root.ok(&["reset", &second]);
    assert_eq!(root.ok(&["log"]).lines().count(), 3);

    // verify, deep verify, stats
    assert!(root.ok(&["verify"]).contains("ok"));
    let deep = root.ok(&["verify", "--deep"]);
    assert!(deep.contains("inodes:"), "got {deep}");
    let stats = root.ok(&["stats"]);
    assert!(stats.contains("branches:           2"), "got {stats}");
    assert!(stats.contains("commits:            4"), "got {stats}");
}

/// **Failure paths** — every "you cannot do that" is a non-zero exit with a message.
#[test]
fn failures_are_reported() {
    let root = TempRoot::new("failures");
    root.init();

    assert!(root.fails(&["ls"]).contains("no current workspace"));
    root.ok(&["checkout", "main"]);

    assert!(root.fails(&["get", "/nope"]).contains("no such path"));
    assert!(root
        .fails(&["commit", "-m", "empty"])
        .contains("already equals its base"));
    assert!(root.fails(&["checkout", "nope"]).contains("not found"));
    assert!(root.fails(&["cat", "00:/x"]).contains("not found"));
    assert!(root.fails(&["frobnicate"]).contains("unknown command"));
}

/// **A dirty workspace needs `--discard`** before `restore` replaces it.
#[test]
fn restore_refuses_to_drop_uncommitted_work() {
    let root = TempRoot::new("restore");
    std::fs::write(root.file("a.txt"), b"one").expect("host file");
    root.init();
    root.ok(&["checkout", "main"]);

    let a = root.file("a.txt").display().to_string();
    root.ok(&["put", &a, "/one"]);
    let head = root.commit("first");
    let genesis = root.ok(&["log", "2"]);
    let genesis = genesis
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .expect("genesis id")
        .to_owned();

    root.ok(&["put", &a, "/two"]);
    assert!(root.fails(&["restore", &genesis]).contains("changes"));
    root.ok(&["restore", &genesis, "--discard"]);
    assert_eq!(
        root.ok(&["ls"]).lines().count(),
        0,
        "back to the empty tree"
    );
    assert!(root.ok(&["log"]).contains(&head), "restore moved no branch");
}

/// A branch created from a commit inherits its tree.
#[test]
fn a_branch_can_start_from_a_commit() {
    let root = TempRoot::new("from-commit");
    std::fs::write(root.file("a.txt"), b"payload").expect("host file");
    root.init();
    root.ok(&["checkout", "main"]);
    let a = root.file("a.txt").display().to_string();
    root.ok(&["put", &a, "/f"]);
    let first = root.commit("first");

    root.ok(&["branch", "side", "--from", &first]);
    root.ok(&["checkout", "side"]);
    assert_eq!(
        root.ok(&["ls"]).lines().count(),
        1,
        "side starts at `first`"
    );
    assert_eq!(root.ok(&["cat", &format!("{first}:/f")]), "payload");
}
