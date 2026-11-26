//! Live Windows WinFsp mount + real `git init` / `git commit` + control-pipe `commit`/`umount` + persistence test.
//!
//! Uses a temporary **directory mount point** (never touches host drive letters) and an isolated
//! `LAYERFS_MOUNTS_DIR`. Skips gracefully if WinFsp is not installed on the host.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "layerfs-live-{label}-{}-{seq}",
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

struct MountGuard(Option<Child>);

impl Drop for MountGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_layerfs"))
}

fn wait_for_mount(mount_point: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("mount process exited early with {status}");
        }
        if std::fs::read_dir(mount_point).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "mount point {} did not become readable within 15s",
        mount_point.display()
    );
}

#[test]
fn live_mount_file_io_git_init_commit_unmount_and_reopen() {
    let dll = fs_winfsp::loader::default_library();
    if fs_winfsp::handshake::read_version(&dll).is_err() {
        eprintln!("SKIP: WinFsp DLL not loadable at {}", dll.display());
        return;
    }

    let tmp = TempDir::new("git");
    let mounts_dir = tmp.0.join("mounts");
    let repo_dir = tmp.0.join("repo");
    // WinFsp directory mount point must NOT exist beforehand (parent exists).
    let mount_dir = tmp.0.join("mnt");

    // 1. Initialize a durable LayerFS repository.
    let init = Command::new(bin())
        .args(["init", repo_dir.to_str().unwrap(), "--durable"])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("init");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // 2. Mount `work` branch to directory mount point.
    let mut guard = MountGuard(Some(
        Command::new(bin())
            .args([
                "--store",
                repo_dir.to_str().unwrap(),
                "mount",
                mount_dir.to_str().unwrap(),
                "--branch",
                "work",
            ])
            .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    ));
    wait_for_mount(&mount_dir, guard.0.as_mut().unwrap());

    // 3. Real Windows file I/O: create dir, write, case-insensitive read, share-mode exclusion, rename, delete.
    std::fs::create_dir(mount_dir.join("Docs")).expect("mkdir Docs");
    std::fs::write(mount_dir.join("Docs").join("Hello.txt"), b"hello layerfs\n").expect("write");
    let read_ci = std::fs::read_to_string(mount_dir.join("docs").join("hello.txt"))
        .expect("case-insensitive read");
    assert_eq!(read_ci, "hello layerfs\n");

    // Verify Windows share-mode enforcement on the mounted volume (WinFsp kernel FSD).
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _exclusive = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(mount_dir.join("Docs").join("Hello.txt"))
            .expect("exclusive open");
        let second = std::fs::OpenOptions::new()
            .read(true)
            .open(mount_dir.join("Docs").join("Hello.txt"));
        assert_eq!(
            second.unwrap_err().raw_os_error(),
            Some(32),
            "second open against share_mode(0) must fail with ERROR_SHARING_VIOLATION (32)"
        );
    }

    std::fs::write(mount_dir.join("temp.txt"), b"remove me").expect("write temp");
    std::fs::rename(mount_dir.join("temp.txt"), mount_dir.join("renamed.txt")).expect("rename");
    std::fs::remove_file(mount_dir.join("renamed.txt")).expect("remove");
    assert!(!mount_dir.join("renamed.txt").exists());

    // 4. Real `git init`, `git add`, `git commit` inside the WinFsp mount!
    if Command::new("git").arg("--version").output().is_ok() {
        let git_init = Command::new("git")
            .arg("init")
            .current_dir(&mount_dir)
            .output()
            .expect("git init");
        assert!(
            git_init.status.success(),
            "git init failed: stdout={} stderr={}",
            String::from_utf8_lossy(&git_init.stdout),
            String::from_utf8_lossy(&git_init.stderr)
        );

        let cfg1 = Command::new("git")
            .args(["config", "user.name", "LayerFS Tester"])
            .current_dir(&mount_dir)
            .output()
            .expect("git config user.name");
        assert!(
            cfg1.status.success(),
            "git config user.name failed: {}",
            String::from_utf8_lossy(&cfg1.stderr)
        );
        let cfg2 = Command::new("git")
            .args(["config", "user.email", "tester@layerfs.local"])
            .current_dir(&mount_dir)
            .output()
            .expect("git config user.email");
        assert!(cfg2.status.success());

        let add = Command::new("git")
            .args(["add", "."])
            .current_dir(&mount_dir)
            .output()
            .expect("git add");
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let commit = Command::new("git")
            .args(["commit", "-m", "initial git commit inside layerfs"])
            .current_dir(&mount_dir)
            .output()
            .expect("git commit");
        assert!(
            commit.status.success(),
            "git commit failed: stdout={} stderr={}",
            String::from_utf8_lossy(&commit.stdout),
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    // 5. Commit via `layerfs commit` over the control pipe (discovered from CWD = mount_dir!).
    let lfs_status = Command::new(bin())
        .arg("status")
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("layerfs status");
    assert!(
        lfs_status.status.success(),
        "layerfs status failed: {}",
        String::from_utf8_lossy(&lfs_status.stderr)
    );
    let st_out = String::from_utf8_lossy(&lfs_status.stdout);
    assert!(st_out.contains("dirty:     yes"), "status: {st_out}");

    let lfs_commit = Command::new(bin())
        .args(["commit", "-m", "snapshot after git commit"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("layerfs commit");
    assert!(
        lfs_commit.status.success(),
        "layerfs commit failed: {}",
        String::from_utf8_lossy(&lfs_commit.stderr)
    );

    // 6. Unmount cleanly via `layerfs umount`.
    let umount = Command::new(bin())
        .args(["umount", mount_dir.to_str().unwrap()])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("layerfs umount");
    assert!(
        umount.status.success(),
        "umount failed: stdout={} stderr={}",
        String::from_utf8_lossy(&umount.stdout),
        String::from_utf8_lossy(&umount.stderr)
    );
    let exit = guard.0.take().unwrap().wait().expect("wait mount child");
    assert!(exit.success(), "mount process did not exit 0: {exit}");

    // 7. Verify commit appears in `layerfs log` and survives remount!
    let log = Command::new(bin())
        .args(["--store", repo_dir.to_str().unwrap(), "log", "work"])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("layerfs log");
    assert!(log.status.success());
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("snapshot after git commit"),
        "log output: {}",
        String::from_utf8_lossy(&log.stdout)
    );

    let mount_dir2 = tmp.0.join("mnt2");
    let mut guard2 = MountGuard(Some(
        Command::new(bin())
            .args([
                "--store",
                repo_dir.to_str().unwrap(),
                "mount",
                mount_dir2.to_str().unwrap(),
                "--branch",
                "work",
            ])
            .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn remount"),
    ));
    wait_for_mount(&mount_dir2, guard2.0.as_mut().unwrap());

    assert_eq!(
        std::fs::read_to_string(mount_dir2.join("Docs").join("Hello.txt")).expect("read persisted"),
        "hello layerfs\n"
    );
    if Command::new("git").arg("--version").output().is_ok() {
        let git_log = Command::new("git")
            .args(["log", "-1", "--oneline"])
            .current_dir(&mount_dir2)
            .output()
            .expect("git log");
        assert!(
            git_log.status.success(),
            "git log after remount failed: {}",
            String::from_utf8_lossy(&git_log.stderr)
        );
        assert!(
            String::from_utf8_lossy(&git_log.stdout).contains("initial git commit inside layerfs"),
            "git log: {}",
            String::from_utf8_lossy(&git_log.stdout)
        );
    }

    let umount2 = Command::new(bin())
        .args(["umount", mount_dir2.to_str().unwrap()])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("umount2");
    assert!(umount2.status.success());
    let exit2 = guard2.0.take().unwrap().wait().expect("wait mount child 2");
    assert!(exit2.success());
}

#[test]
fn live_mount_hot_switch_merge_conflict_and_concurrent_io() {
    let dll = fs_winfsp::loader::default_library();
    if fs_winfsp::handshake::read_version(&dll).is_err() {
        eprintln!("SKIP: WinFsp DLL not loadable at {}", dll.display());
        return;
    }

    let tmp = TempDir::new("advanced");
    let mounts_dir = tmp.0.join("mounts");
    let seed_dir = tmp.0.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::write(seed_dir.join("shared.txt"), b"base line\n").unwrap();
    let repo_dir = tmp.0.join("repo");
    let mount_dir = tmp.0.join("mnt");

    // 1. Initialize from seed directory (`init --from`).
    let init = Command::new(bin())
        .args([
            "init",
            repo_dir.to_str().unwrap(),
            "--from",
            seed_dir.to_str().unwrap(),
            "--disposable",
        ])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("init --from");
    assert!(
        init.status.success(),
        "init --from failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // Fork three branches from `work` before mounting.
    for br in ["feat-a", "feat-b", "feat-c"] {
        let fork = Command::new(bin())
            .args([
                "--store",
                repo_dir.to_str().unwrap(),
                "fork",
                br,
                "--from",
                "work",
            ])
            .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
            .output()
            .expect("fork");
        assert!(
            fork.status.success(),
            "fork {br} failed: {}",
            String::from_utf8_lossy(&fork.stderr)
        );
    }

    // 2. Mount `work` and verify imported file is visible through WinFsp.
    let mut guard = MountGuard(Some(
        Command::new(bin())
            .args([
                "--store",
                repo_dir.to_str().unwrap(),
                "mount",
                mount_dir.to_str().unwrap(),
                "--branch",
                "work",
            ])
            .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mount"),
    ));
    wait_for_mount(&mount_dir, guard.0.as_mut().unwrap());
    assert_eq!(
        std::fs::read_to_string(mount_dir.join("shared.txt")).expect("read imported shared.txt"),
        "base line\n"
    );

    // 3. First live hot-switch (`work` -> `feat-a`) from inside the mount!
    let sw_a = Command::new(bin())
        .args(["checkout", "feat-a"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("checkout feat-a");
    assert!(
        sw_a.status.success(),
        "checkout feat-a failed: stdout={} stderr={}",
        String::from_utf8_lossy(&sw_a.stdout),
        String::from_utf8_lossy(&sw_a.stderr)
    );
    wait_for_mount(&mount_dir, guard.0.as_mut().unwrap());

    let st_a = Command::new(bin())
        .arg("status")
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("status feat-a");
    assert!(st_a.status.success());
    assert!(
        String::from_utf8_lossy(&st_a.stdout).contains("branch:    feat-a"),
        "status after switch 1: {}",
        String::from_utf8_lossy(&st_a.stdout)
    );

    std::fs::write(mount_dir.join("only_a.txt"), b"branch a file\n").unwrap();
    std::fs::write(mount_dir.join("shared.txt"), b"edited by a\n").unwrap();
    let cm_a = Command::new(bin())
        .args(["commit", "-m", "commit on feat-a"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("commit feat-a");
    assert!(
        cm_a.status.success(),
        "commit feat-a failed: {}",
        String::from_utf8_lossy(&cm_a.stderr)
    );

    // 4. Second live hot-switch (`feat-a` -> `feat-b`): proves RwLock adapter slot works across multiple switches!
    let sw_b = Command::new(bin())
        .args(["checkout", "feat-b"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("checkout feat-b");
    assert!(
        sw_b.status.success(),
        "checkout feat-b failed: stdout={} stderr={}",
        String::from_utf8_lossy(&sw_b.stdout),
        String::from_utf8_lossy(&sw_b.stderr)
    );
    wait_for_mount(&mount_dir, guard.0.as_mut().unwrap());
    assert!(!mount_dir.join("only_a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(mount_dir.join("shared.txt")).unwrap(),
        "base line\n"
    );

    std::fs::write(mount_dir.join("only_b.txt"), b"branch b file\n").unwrap();
    std::fs::write(mount_dir.join("shared.txt"), b"edited by b\n").unwrap();
    let cm_b = Command::new(bin())
        .args(["commit", "-m", "commit on feat-b"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("commit feat-b");
    assert!(
        cm_b.status.success(),
        "commit feat-b failed: {}",
        String::from_utf8_lossy(&cm_b.stderr)
    );

    // 5. Conflicting 3-way merge (`feat-a` vs `feat-b` on `shared.txt`) must report conflict with exit code 5.
    let conflict = Command::new(bin())
        .args([
            "--store",
            repo_dir.to_str().unwrap(),
            "merge",
            "feat-a",
            "feat-b",
            "conflicted",
            "-m",
            "should conflict",
        ])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("merge conflict");
    assert_eq!(
        conflict.status.code(),
        Some(5),
        "expected conflict exit code 5, got {:?}, stdout={} stderr={}",
        conflict.status.code(),
        String::from_utf8_lossy(&conflict.stdout),
        String::from_utf8_lossy(&conflict.stderr)
    );
    assert!(
        String::from_utf8_lossy(&conflict.stdout).contains("CONFLICT"),
        "stdout: {}",
        String::from_utf8_lossy(&conflict.stdout)
    );

    // 6. Third live hot-switch (`feat-b` -> `feat-c`) + concurrent multi-thread I/O stress (8 threads x 10 files).
    let sw_c = Command::new(bin())
        .args(["checkout", "feat-c"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("checkout feat-c");
    assert!(
        sw_c.status.success(),
        "checkout feat-c failed: {}",
        String::from_utf8_lossy(&sw_c.stderr)
    );
    wait_for_mount(&mount_dir, guard.0.as_mut().unwrap());
    std::fs::create_dir(mount_dir.join("stress")).unwrap();

    std::thread::scope(|scope| {
        for t in 0..8usize {
            let root = mount_dir.join("stress");
            scope.spawn(move || {
                for f in 0..10usize {
                    let p = root.join(format!("t{t}_f{f}.dat"));
                    let payload = format!("thread={t} file={f} payload-0123456789abcdef\n");
                    std::fs::write(&p, payload.as_bytes()).expect("concurrent write");
                    let got = std::fs::read_to_string(&p).expect("concurrent read");
                    assert_eq!(got, payload);
                }
            });
        }
    });

    let cm_c = Command::new(bin())
        .args(["commit", "-m", "concurrent stress"])
        .current_dir(&mount_dir)
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("commit feat-c");
    assert!(
        cm_c.status.success(),
        "commit feat-c failed: {}",
        String::from_utf8_lossy(&cm_c.stderr)
    );

    // 7. Clean 3-way merge (`feat-a` + `feat-c` -> `merged`) + publish to `main`.
    let merged = Command::new(bin())
        .args([
            "--store",
            repo_dir.to_str().unwrap(),
            "merge",
            "feat-a",
            "feat-c",
            "merged",
            "-m",
            "clean merge",
        ])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("clean merge");
    assert!(
        merged.status.success(),
        "clean merge failed: {}",
        String::from_utf8_lossy(&merged.stderr)
    );

    let pub_out = Command::new(bin())
        .args(["--store", repo_dir.to_str().unwrap(), "publish", "merged"])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("publish");
    assert!(
        pub_out.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&pub_out.stderr)
    );

    // 8. Clean unmount.
    let umount = Command::new(bin())
        .args(["umount", mount_dir.to_str().unwrap()])
        .env("LAYERFS_MOUNTS_DIR", &mounts_dir)
        .output()
        .expect("umount");
    assert!(
        umount.status.success(),
        "umount failed: {}",
        String::from_utf8_lossy(&umount.stderr)
    );
    let exit = guard.0.take().unwrap().wait().expect("wait mount child");
    assert!(exit.success());
}
