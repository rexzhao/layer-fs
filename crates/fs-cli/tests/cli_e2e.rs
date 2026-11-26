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

    // 8. Version, compact, and list succeed cleanly.
    let compact = run_cli(&["--store", &repo_str, "compact"], &mounts);
    assert!(
        compact.status.success(),
        "compact failed: {}",
        String::from_utf8_lossy(&compact.stderr)
    );
    assert!(String::from_utf8_lossy(&compact.stdout).contains("compacted "));

    let ver = run_cli(&["version"], &mounts);
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("upstream layerfs"));
    let list = run_cli(&["list"], &mounts);
    assert!(list.status.success());

    // 9. `init --from <dir>` imports an existing directory and refuses overlapping store.
    let src = tmp.0.join("import-src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    std::fs::write(src.join("sub").join("b.txt"), b"world").unwrap();
    let imported_repo = tmp.0.join("imported-repo");
    let imp = run_cli(
        &[
            "init",
            imported_repo.to_str().unwrap(),
            "--from",
            src.to_str().unwrap(),
            "--disposable",
        ],
        &mounts,
    );
    assert!(
        imp.status.success(),
        "init --from failed: {}",
        String::from_utf8_lossy(&imp.stderr)
    );
    let imp_out = String::from_utf8_lossy(&imp.stdout);
    assert!(imp_out.contains("entries=4"), "stdout: {imp_out}");

    let bad_inside = src.join("nested-store");
    let bad = run_cli(
        &[
            "init",
            bad_inside.to_str().unwrap(),
            "--from",
            src.to_str().unwrap(),
        ],
        &mounts,
    );
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn auto_discovers_store_from_cwd_subdir_and_env() {
    let tmp = TempDir::new("autodiscover");
    let mounts = tmp.0.join("mounts");
    let repo_dir = tmp.0.join("repo");
    let nested_dir = repo_dir.join("notes").join("sub");
    let outside_dir = tmp.0.join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();

    let init = run_cli(&["init", repo_dir.to_str().unwrap(), "--disposable"], &mounts);
    assert!(init.status.success(), "init failed");
    std::fs::create_dir_all(&nested_dir).unwrap();

    // 1. Inside the store directory itself, all commands work without `--store`.
    for args in [
        &["branch"][..],
        &["fork", "auto-feat", "--from", "work"][..],
        &["diff", "work", "auto-feat"][..],
        &["status"][..],
        &["stats"][..],
        &["log"][..],
        &["compact"][..],
    ] {
        let out = Command::new(bin())
            .args(args)
            .current_dir(&repo_dir)
            .env("LAYERFS_MOUNTS_DIR", &mounts)
            .env_remove("LAYERFS_STORE")
            .output()
            .expect("run inside store dir");
        assert!(
            out.status.success(),
            "command {args:?} failed inside store dir: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 2. Inside a nested subdirectory of the store, ancestor discovery finds the store.
    let nested_branch = Command::new(bin())
        .args(["branch"])
        .current_dir(&nested_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts)
        .env_remove("LAYERFS_STORE")
        .output()
        .expect("branch from nested dir");
    assert!(
        nested_branch.status.success(),
        "branch from nested dir failed: {}",
        String::from_utf8_lossy(&nested_branch.stderr)
    );
    assert!(String::from_utf8_lossy(&nested_branch.stdout).contains("auto-feat"));

    // 3. Outside the store, `LAYERFS_STORE` env var resolves it; without it, exit code 2.
    let from_env = Command::new(bin())
        .args(["stats"])
        .current_dir(&outside_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts)
        .env("LAYERFS_STORE", &repo_dir)
        .output()
        .expect("stats via LAYERFS_STORE");
    assert!(
        from_env.status.success(),
        "stats via LAYERFS_STORE failed: {}",
        String::from_utf8_lossy(&from_env.stderr)
    );

    let missing = Command::new(bin())
        .args(["stats"])
        .current_dir(&outside_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts)
        .env_remove("LAYERFS_STORE")
        .output()
        .expect("stats outside store");
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn init_from_multiple_directories_custom_branch_and_duplicate_refusal() {
    let tmp = TempDir::new("init-multi");
    let mounts = tmp.0.join("mounts");
    let v1_app = tmp.0.join("v1").join("app");
    let v2_app = tmp.0.join("v2").join("app");
    let tools = tmp.0.join("tools");
    std::fs::create_dir_all(&v1_app).unwrap();
    std::fs::create_dir_all(&v2_app).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(v1_app.join("a.txt"), b"v1\n").unwrap();
    std::fs::write(v2_app.join("b.txt"), b"v2\n").unwrap();
    std::fs::write(tools.join("t.txt"), b"tools\n").unwrap();

    // 1. Duplicate inferred branch names (`app` vs `app`) fail with exit 5 and create NO directory.
    let dup_repo = tmp.0.join("dup-repo");
    let dup = run_cli(
        &[
            "init",
            dup_repo.to_str().unwrap(),
            "--from",
            v1_app.to_str().unwrap(),
            "--from",
            v2_app.to_str().unwrap(),
            "--disposable",
        ],
        &mounts,
    );
    assert_eq!(
        dup.status.code(),
        Some(5),
        "expected exit 5 on duplicate branch, got {:?}, stderr={}",
        dup.status.code(),
        String::from_utf8_lossy(&dup.stderr)
    );
    assert!(
        !dup_repo.exists(),
        "failed init must leave zero side effects"
    );

    // 2. Reserved branch name `:main` fails with exit 2 and creates NO directory.
    let main_repo = tmp.0.join("main-repo");
    let reserved = run_cli(
        &[
            "init",
            main_repo.to_str().unwrap(),
            "--from",
            &format!("{}:main", v1_app.display()),
        ],
        &mounts,
    );
    assert_eq!(reserved.status.code(), Some(2));
    assert!(!main_repo.exists());

    // 3. Invalid 2nd source directory fails with exit 2 and creates NO directory.
    let bad_repo = tmp.0.join("bad-repo");
    let bad_second = run_cli(
        &[
            "init",
            bad_repo.to_str().unwrap(),
            "--from",
            v1_app.to_str().unwrap(),
            "--from",
            tmp.0.join("does-not-exist").to_str().unwrap(),
        ],
        &mounts,
    );
    assert_eq!(bad_second.status.code(), Some(2));
    assert!(!bad_repo.exists());

    // 4. Multi-dir import using both `path:branch` disambiguation and space-separated `--from`.
    let repo_dir = tmp.0.join("multi-repo");
    let ok = run_cli(
        &[
            "init",
            repo_dir.to_str().unwrap(),
            "--from",
            &format!("{}:app-v1", v1_app.display()),
            &format!("{}:app-v2", v2_app.display()),
            "--from",
            tools.to_str().unwrap(),
            "--disposable",
        ],
        &mounts,
    );
    assert!(
        ok.status.success(),
        "multi-dir init failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let ok_out = String::from_utf8_lossy(&ok.stdout);
    assert!(ok_out.contains("app-v1"), "stdout: {ok_out}");
    assert!(ok_out.contains("app-v2"), "stdout: {ok_out}");
    assert!(ok_out.contains("tools"), "stdout: {ok_out}");

    // 5. Verify branches exist and `diff` / `merge` work directly inside the store directory.
    let branches = Command::new(bin())
        .args(["branch"])
        .current_dir(&repo_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts)
        .env_remove("LAYERFS_STORE")
        .output()
        .expect("branch");
    assert!(branches.status.success());
    let b_out = String::from_utf8_lossy(&branches.stdout);
    assert!(b_out.contains("app-v1"), "branches: {b_out}");
    assert!(b_out.contains("app-v2"), "branches: {b_out}");
    assert!(b_out.contains("tools"), "branches: {b_out}");

    let merge = Command::new(bin())
        .args(["merge", "app-v1", "tools", "merged", "-m", "combine"])
        .current_dir(&repo_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts)
        .env_remove("LAYERFS_STORE")
        .output()
        .expect("merge");
    assert!(
        merge.status.success(),
        "merge imported branches failed: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
}
