//! End-to-end CLI tests for `layerfs` (non-mount operations, help, exit codes, branching, diff, merge, publish).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "layerfs-cli-e2e-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_layerfs"))
}

fn run_cli(args: &[&str], mounts_dir: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env("LAYERFS_MOUNTS_DIR", mounts_dir)
        .output()
        .expect("run layerfs")
}

#[test]
fn help_and_subcommand_help_cover_every_command() {
    let tmp = TempDir::new("help");
    let out = run_cli(&["--help"], &tmp.0);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);

    let commands: Vec<String> = text
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .filter(|word| !word.starts_with('-'))
        .collect();

    assert!(commands.len() >= 12, "expected >=12 commands, got {commands:?}");
    for cmd in &commands {
        let sub = run_cli(&["help", cmd], &tmp.0);
        assert!(
            sub.status.success(),
            "`layerfs help {cmd}` failed: {}",
            String::from_utf8_lossy(&sub.stderr)
        );
        assert!(!sub.stdout.is_empty());
    }

    // Removed file verbs must exit with code 2 (usage).
    let removed = run_cli(&["cat", "foo.txt"], &tmp.0);
    assert_eq!(removed.status.code(), Some(2));
}

#[test]
fn init_profiles_branch_fork_log_diff_stats_and_exit_codes() {
    let tmp = TempDir::new("lifecycle");
    let mounts = tmp.0.join("mounts");
    let repo_dir = tmp.0.join("repo");
    let repo_str = repo_dir.to_string_lossy().into_owned();

    // 1. Init with --durable and custom --size.
    let out = run_cli(&["init", &repo_str, "--durable", "--size", "64G"], &mounts);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("profile=durable"), "stdout: {stdout}");
    assert!(stdout.contains("default branch=work"), "stdout: {stdout}");

    // Re-init on existing repo fails with conflict (5).
    let again = run_cli(&["init", &repo_str], &mounts);
    assert_eq!(again.status.code(), Some(5));

    // 2. Branch listing shows `main` and `work`.
    let branches = run_cli(&["--store", &repo_str, "branch"], &mounts);
    assert!(branches.status.success());
    let b_out = String::from_utf8_lossy(&branches.stdout);
    assert!(b_out.contains("main"), "branches: {b_out}");
    assert!(b_out.contains("work"), "branches: {b_out}");

    // 3. Fork a new branch `dev` from `work`.
    let fork = run_cli(&["--store", &repo_str, "fork", "dev", "--from", "work"], &mounts);
    assert!(
        fork.status.success(),
        "fork failed: {}",
        String::from_utf8_lossy(&fork.stderr)
    );

    // 4. Stats shows profile=durable and 64 GiB.
    let stats = run_cli(&["--store", &repo_str, "stats"], &mounts);
    assert!(stats.status.success());
    let s_out = String::from_utf8_lossy(&stats.stdout);
    assert!(s_out.contains("profile:         durable"), "stats: {s_out}");
    assert!(
        s_out.contains(&(64u64 * 1024 * 1024 * 1024).to_string()),
        "stats: {s_out}"
    );

    // 5. Diff identical branches reports no differences.
    let diff = run_cli(&["--store", &repo_str, "diff", "work", "dev"], &mounts);
    assert!(diff.status.success());
    assert!(String::from_utf8_lossy(&diff.stdout).contains("no differences"));

    // 6. Checkout missing branch exits 3 (not found).
    let missing = run_cli(&["--store", &repo_str, "checkout", "nope"], &mounts);
    assert_eq!(missing.status.code(), Some(3));

    // 7. Disposable profile init works too.
    let disp_dir = tmp.0.join("repo-disp");
    let disp_str = disp_dir.to_string_lossy().into_owned();
    let disp = run_cli(&["init", &disp_str, "--disposable"], &mounts);
    assert!(disp.status.success());
    assert!(String::from_utf8_lossy(&disp.stdout).contains("profile=disposable"));

    // 8. Version and list succeed cleanly.
    let ver = run_cli(&["version"], &mounts);
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("upstream layerfs"));
    let list = run_cli(&["list"], &mounts);
    assert!(list.status.success());
}
