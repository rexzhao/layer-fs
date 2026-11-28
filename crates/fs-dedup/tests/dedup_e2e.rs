//! End-to-end tests for `fs-dedup`:
//! 1. All 6 `fs_winfsp::scenarios` WinFsp backend contract tests.
//! 2. Parallel `import_dir` with cross-workspace dedup, Windows `nul` + >280-char long path,
//!    and automatic hot-DB overlay pinning (`ArtifactDB`, `wc.db`).
//! 3. Instant CoW `clone_workspace`, CoW write isolation, background cold-file `dedup_workspace`
//!    with busy-handle safety, and pack `gc` reclamation.
//! 4. Live Windows WinFsp mount, real OS file read/write, and named-pipe `umount`.

use fs_dedup::{Codec, DedupStore, DedupWorkspaceBackend, StoreConfig};
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::{Path as OpsPath, WorkspaceOps};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fs-dedup-test-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn dedup_backend_satisfies_all_winfsp_contract_scenarios() {
    let run_one = |label: &str, f: fn(&DedupWorkspaceBackend)| {
        let tmp = TempDir::new(label);
        let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
        store.create_workspace("ws").unwrap();
        let backend = DedupWorkspaceBackend::open(store, "ws").unwrap();
        f(&backend);
    };

    run_one("rename", fs_winfsp::scenarios::scenario_identity_across_rename);
    run_one(
        "supersede",
        fs_winfsp::scenarios::scenario_supersede_truncates_in_place,
    );
    run_one("removal", fs_winfsp::scenarios::scenario_removal_cases);
    run_one("readdir", fs_winfsp::scenarios::scenario_read_dir_by_id);
    run_one(
        "nonempty",
        fs_winfsp::scenarios::scenario_remove_non_empty_directory,
    );
    run_one("case", fs_winfsp::scenarios::scenario_case_insensitive_names);
}

#[test]
fn import_cross_workspace_dedup_cow_clone_background_dedup_and_gc() {
    let tmp = TempDir::new("full_lifecycle");
    let src1 = tmp.0.join("src1");
    let src2 = tmp.0.join("src2");
    std::fs::create_dir_all(src1.join("Assets")).unwrap();
    std::fs::create_dir_all(src1.join("Library")).unwrap();
    std::fs::create_dir_all(src1.join(".svn")).unwrap();
    std::fs::create_dir_all(src2.join("Assets")).unwrap();
    std::fs::create_dir_all(src2.join("Library")).unwrap();

    // 1.5 MiB deterministic payload (spans multiple FastCDC-256K chunks).
    let mut big_v1 = vec![0u8; 1_500_000];
    let mut rng = 0xCAFE_BABE_1234_5678u64;
    for chunk in big_v1.chunks_mut(8) {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let b = rng.to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&b[..n]);
    }
    // v2 inserts 97 bytes at the beginning — FastCDC should still deduplicate >80% of the tail!
    let mut big_v2 = vec![0x5Au8; 97];
    big_v2.extend_from_slice(&big_v1);

    let yaml = b"UnityPrefab:\n  m_Name: HeroController\n  m_Speed: 12.5\n".repeat(3000);
    std::fs::write(src1.join("Assets").join("Big.bin"), &big_v1).unwrap();
    std::fs::write(src1.join("Assets").join("Hero.prefab"), &yaml).unwrap();
    std::fs::write(src1.join("Library").join("ArtifactDB"), b"lmdb-hot-db-v1").unwrap();
    std::fs::write(src1.join(".svn").join("wc.db"), b"sqlite-wc-db-v1").unwrap();

    std::fs::write(src2.join("Assets").join("Big.bin"), &big_v2).unwrap();
    std::fs::write(src2.join("Assets").join("Hero.prefab"), &yaml).unwrap();
    std::fs::write(src2.join("Assets").join("OnlyV2.txt"), b"unique-to-v2").unwrap();

    // Windows reserved device name (`nul`) + >280-char long path!
    #[cfg(windows)]
    let _nul_cleanup = {
        let canon1 = std::fs::canonicalize(&src1).unwrap();
        let verbatim_nul = canon1.join("nul");
        let seg = "x".repeat(75);
        let deep_dir = canon1.join(&seg).join(&seg).join(&seg).join(&seg);
        std::fs::create_dir_all(&deep_dir).unwrap();
        let deep_file = deep_dir.join("deep.txt");
        assert!(deep_file.as_os_str().len() > 280);
        std::fs::write(&verbatim_nul, b"literal-nul-data").unwrap();
        std::fs::write(&deep_file, b"deep-long-path-data").unwrap();
        struct Guard(PathBuf, PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        Guard(verbatim_nul, canon1.join(&seg))
    };

    let cfg = StoreConfig {
        codec: Codec::Zstd1,
        pack_target_bytes: 1024 * 1024, // 1 MiB packs so GC exercises multi-pack compaction!
        ..StoreConfig::default()
    };
    let store = DedupStore::init(tmp.0.join("store"), cfg).unwrap();

    let s1 = store.import_dir(&src1, "v1", 4, None).unwrap();
    assert_eq!(s1.errors, 0);
    assert_eq!(s1.overlay_pinned_files, 2, "ArtifactDB + wc.db must stay pinned in overlay");

    let s2 = store.import_dir(&src2, "v2", 4, None).unwrap();
    assert_eq!(s2.errors, 0);
    assert!(
        s2.new_unique_bytes < s2.logical_bytes / 2,
        "FastCDC + dedup should keep v2 incremental < 50% of logical (got {} / {})",
        s2.new_unique_bytes,
        s2.logical_bytes
    );

    // Verify exact readback of v1 (including nul + long path on Windows) and v2.
    let b1 = DedupWorkspaceBackend::open(store.clone(), "v1").unwrap();
    let f_big1 = b1
        .create_or_open(&OpsPath::new(b"/Assets/Big.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(b1.read(f_big1.file_id, 0, big_v1.len()).unwrap(), big_v1);
    let f_db = b1
        .create_or_open(&OpsPath::new(b"/Library/ArtifactDB"), Disposition::Open)
        .unwrap();
    assert_eq!(
        b1.read(f_db.file_id, 0, f_db.size as usize).unwrap(),
        b"lmdb-hot-db-v1"
    );
    #[cfg(windows)]
    {
        let f_nul = b1
            .create_or_open(&OpsPath::new(b"/nul"), Disposition::Open)
            .unwrap();
        assert_eq!(
            b1.read(f_nul.file_id, 0, f_nul.size as usize).unwrap(),
            b"literal-nul-data"
        );
    }

    let b2 = DedupWorkspaceBackend::open(store.clone(), "v2").unwrap();
    let f_big2 = b2
        .create_or_open(&OpsPath::new(b"/Assets/Big.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(b2.read(f_big2.file_id, 0, big_v2.len()).unwrap(), big_v2);

    // Instant CoW clone of v1 -> feature_branch.
    store.clone_workspace("v1", "feature").unwrap();
    let bf = DedupWorkspaceBackend::open(store.clone(), "feature").unwrap();

    // CoW write inside `feature` must NOT affect `v1`.
    let f_prefab = bf
        .create_or_open(&OpsPath::new(b"/Assets/Hero.prefab"), Disposition::Open)
        .unwrap();
    bf.write(f_prefab.file_id, 0, b"MODIFIED_HEADER").unwrap();
    let modified_read = bf.read(f_prefab.file_id, 0, 15).unwrap();
    assert_eq!(modified_read, b"MODIFIED_HEADER");

    let f_prefab_v1 = b1
        .create_or_open(&OpsPath::new(b"/Assets/Hero.prefab"), Disposition::Open)
        .unwrap();
    assert_eq!(
        b1.read(f_prefab_v1.file_id, 0, 12).unwrap(),
        b"UnityPrefab:"
    );

    // Also create a brand-new file in `feature` whose content matches `v1`'s Hero.prefab.
    let f_dup = bf
        .create_or_open(&OpsPath::new(b"/Assets/CloneCopy.prefab"), Disposition::Create)
        .unwrap();
    bf.write(f_dup.file_id, 0, &yaml).unwrap();

    // Flush buffered overlay metadata before running background dedup directly on `store`.
    bf.flush().unwrap();

    // Background dedup skips busy inodes when `is_busy` reports true!
    let busy_sum = store
        .dedup_workspace(Some("feature"), 0, |_, id| id == f_prefab.file_id)
        .unwrap();
    assert_eq!(busy_sum.skipped_busy, 1);
    assert_eq!(busy_sum.deduped_files, 1, "only non-busy CloneCopy.prefab should dedup");
    assert_eq!(
        busy_sum.new_unique_bytes, 0,
        "CloneCopy.prefab matches existing chunks so 0 new unique bytes!"
    );

    // Now release busy handle and dedup the modified Hero.prefab too; hot DBs must remain untouched.
    let cold_sum = store
        .dedup_workspace(Some("feature"), 0, |_, _| false)
        .unwrap();
    assert_eq!(cold_sum.deduped_files, 1);
    assert!(
        !store
            .overlay()
            .file_path(bf.ws_id(), f_prefab.file_id)
            .exists(),
        "loose overlay file must be deleted after dedup"
    );
    assert_eq!(
        bf.read(f_prefab.file_id, 0, 15).unwrap(),
        b"MODIFIED_HEADER",
        "deduped file reads back identically from pack store"
    );
    // Pinned ArtifactDB must still be in overlay!
    let f_db_feat = bf
        .create_or_open(&OpsPath::new(b"/Library/ArtifactDB"), Disposition::Open)
        .unwrap();
    assert!(
        store
            .overlay()
            .file_path(bf.ws_id(), f_db_feat.file_id)
            .exists(),
        "pinned ArtifactDB must remain a local loose overlay file"
    );

    // Drop `v2` and `feature` so dead bytes accumulate, then run pack GC and verify `v1` stays intact.
    drop(b2);
    drop(bf);
    store.remove_workspace("v2").unwrap();
    store.remove_workspace("feature").unwrap();
    let gc = store.gc(1).unwrap();
    assert!(
        gc.deleted_empty_packs + gc.compacted_packs >= 1,
        "expected GC to delete or compact packs, got {gc:?}"
    );
    assert_eq!(
        b1.read(f_big1.file_id, 0, big_v1.len()).unwrap(),
        big_v1,
        "v1 must remain 100% intact and BLAKE3-verified after GC"
    );
}

#[cfg(windows)]
#[test]
fn live_winfsp_mount_io_background_dedup_and_umount() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let tmp = TempDir::new("live_mount");
    let store_dir = tmp.0.join("store");
    let src_dir = tmp.0.join("src");
    let mount_point = tmp.0.join("mnt");
    std::fs::create_dir_all(src_dir.join("Assets")).unwrap();
    std::fs::create_dir_all(src_dir.join(".claude").join(".session-state")).unwrap();
    std::fs::write(src_dir.join("Assets").join("hello.txt"), b"hello unity dedupfs").unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.import_dir(&src_dir, "main_ws", 2, None).unwrap();
    drop(store);

    let bin = env!("CARGO_BIN_EXE_dedupfs");
    struct ChildGuard(Option<std::process::Child>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut c) = self.0.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
    let mut child_guard = ChildGuard(Some(
        Command::new(bin)
            .args([
                "--store",
                store_dir.to_str().unwrap(),
                "mount",
                "main_ws",
                mount_point.to_str().unwrap(),
                "--cache",
                "32M",
                "--dedup-interval",
                "1",
                "--dedup-idle",
                "0",
            ])
            .spawn()
            .expect("spawn dedupfs mount"),
    ));

    // Wait up to 10s for WinFsp mount point to become readable.
    let deadline = Instant::now() + Duration::from_secs(10);
    let hello_path = mount_point.join("Assets").join("hello.txt");
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(&hello_path) {
            assert_eq!(bytes, b"hello unity dedupfs");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read(&hello_path).expect("read imported file through WinFsp"),
        b"hello unity dedupfs"
    );

    // Verify empty subdirectory emits `.` and `..` so `git status` / `FindFirstFileW("*")`
    // never fails with ENOENT ("could not open directory ...: No such file or directory")!
    let empty_dir = mount_point.join(".claude").join(".session-state");
    assert_eq!(std::fs::read_dir(&empty_dir).unwrap().count(), 0);
    let git_init = Command::new("git")
        .current_dir(&mount_point)
        .args(["init", "-q"])
        .output();
    if let Ok(out) = git_init {
        if out.status.success() {
            let st = Command::new("git")
                .current_dir(&mount_point)
                .args(["status"])
                .output()
                .expect("run git status");
            let stderr = String::from_utf8_lossy(&st.stderr);
            assert!(
                !stderr.contains("could not open directory"),
                "git status must not fail on empty subdirectories, got stderr: {stderr}"
            );
        }
    }

    // Write a new file through Windows filesystem APIs (lands in sparse overlay).
    let created_path = mount_point.join("Assets").join("new_asset.txt");
    let payload = b"newly written asset through WinFsp!\n".repeat(200);
    std::fs::write(&created_path, &payload).expect("write through WinFsp");
    assert_eq!(std::fs::read(&created_path).unwrap(), payload);

    // Wait briefly so the 1s background dedup sweep moves `new_asset.txt` into the pack pool.
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(
        std::fs::read(&created_path).unwrap(),
        payload,
        "file must remain readable after background dedup moves it to pack CAS"
    );

    // Unmount cleanly via `dedupfs umount <mount_point>`.
    let umount_out = Command::new(bin)
        .args(["umount", mount_point.to_str().unwrap()])
        .output()
        .expect("run dedupfs umount");
    assert!(
        umount_out.status.success(),
        "umount failed: stdout={} stderr={}",
        String::from_utf8_lossy(&umount_out.stdout),
        String::from_utf8_lossy(&umount_out.stderr)
    );

    // Wait for mount process to exit cleanly.
    let mut child = child_guard.0.take().unwrap();
    let wait_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "mount process exited with {status:?}");
            break;
        }
        if Instant::now() >= wait_deadline {
            let _ = child.kill();
            panic!("dedupfs mount process did not exit after umount");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Reopen store offline and confirm `new_asset.txt` was deduplicated into pack CAS (0 overlay files left!).
    let reopened = DedupStore::open(&store_dir).unwrap();
    let stats = reopened.stats().unwrap();
    assert_eq!(
        stats.overlay_files, 0,
        "background dedup should have moved new_asset.txt out of overlay into packs"
    );
    assert!(stats.unique_chunks >= 2);
}

#[test]
fn import_update_skips_unchanged_updates_modified_adds_new_and_deletes_removed() {
    let tmp = TempDir::new("import_update");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(src.join("keep_dir")).unwrap();
    std::fs::create_dir_all(src.join("drop_dir")).unwrap();
    std::fs::write(src.join("keep_dir").join("same.txt"), b"unchanged content").unwrap();
    std::fs::write(src.join("keep_dir").join("edit.txt"), b"version 1").unwrap();
    std::fs::write(src.join("drop_dir").join("gone.txt"), b"will be deleted").unwrap();
    std::fs::write(src.join("Library_ArtifactDB.db"), b"hot-db-v1").unwrap();

    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();

    // First `--update` on a non-existent workspace creates it cleanly.
    let first = store.import_dir_update(&src, "proj", 2, None).unwrap();
    assert_eq!(first.files, 4);
    assert_eq!(first.unchanged_files, 0);

    // Second `--update` with zero changes skips all 4 files with zero new unique bytes!
    let noop = store.import_dir_update(&src, "proj", 2, None).unwrap();
    assert_eq!(noop.files, 4);
    assert_eq!(noop.unchanged_files, 4);
    assert_eq!(noop.updated_files, 0);
    assert_eq!(noop.removed_entries, 0);
    assert_eq!(noop.new_unique_bytes, 0);

    // Mutate host source: modify `edit.txt` & hot DB, add `new_dir/added.txt`, delete `drop_dir/`.
    std::fs::write(src.join("keep_dir").join("edit.txt"), b"version 2 - longer payload!").unwrap();
    std::fs::write(src.join("Library_ArtifactDB.db"), b"hot-db-v2-updated").unwrap();
    std::fs::create_dir_all(src.join("new_dir")).unwrap();
    std::fs::write(src.join("new_dir").join("added.txt"), b"brand new file").unwrap();
    std::fs::remove_dir_all(src.join("drop_dir")).unwrap();

    let upd = store.import_dir_update(&src, "proj", 2, None).unwrap();
    assert_eq!(upd.unchanged_files, 1, "only same.txt should be unchanged");
    assert_eq!(upd.updated_files, 2, "edit.txt + Library_ArtifactDB.db updated");
    assert_eq!(upd.files, 4, "same + edit + hot-db + added");
    assert_eq!(upd.removed_entries, 2, "drop_dir + drop_dir/gone.txt removed");

    // Verify exact contents in workspace after update.
    let backend = DedupWorkspaceBackend::open(store.clone(), "proj").unwrap();
    let f_same = backend
        .create_or_open(&OpsPath::new(b"/keep_dir/same.txt"), Disposition::Open)
        .unwrap();
    assert_eq!(
        backend.read(f_same.file_id, 0, f_same.size as usize).unwrap(),
        b"unchanged content"
    );
    let f_edit = backend
        .create_or_open(&OpsPath::new(b"/keep_dir/edit.txt"), Disposition::Open)
        .unwrap();
    assert_eq!(
        backend.read(f_edit.file_id, 0, f_edit.size as usize).unwrap(),
        b"version 2 - longer payload!"
    );
    let f_db = backend
        .create_or_open(&OpsPath::new(b"/Library_ArtifactDB.db"), Disposition::Open)
        .unwrap();
    assert_eq!(
        backend.read(f_db.file_id, 0, f_db.size as usize).unwrap(),
        b"hot-db-v2-updated"
    );
    let f_add = backend
        .create_or_open(&OpsPath::new(b"/new_dir/added.txt"), Disposition::Open)
        .unwrap();
    assert_eq!(
        backend.read(f_add.file_id, 0, f_add.size as usize).unwrap(),
        b"brand new file"
    );
    assert!(
        backend
            .create_or_open(&OpsPath::new(b"/drop_dir/gone.txt"), Disposition::Open)
            .is_err(),
        "deleted file must no longer exist in workspace"
    );
}

#[test]
fn cli_auto_discovers_store_from_cwd_and_env() {
    use std::process::Command;

    let tmp = TempDir::new("cli_discover");
    let store_dir = tmp.0.join("my_store");
    let src_dir = tmp.0.join("src");
    let other_dir = tmp.0.join("elsewhere");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&other_dir).unwrap();
    std::fs::write(src_dir.join("a.txt"), b"hello auto discovery").unwrap();

    let bin = env!("CARGO_BIN_EXE_dedupfs");
    let run_in = |cwd: &std::path::Path, args: &[&str], env_store: Option<&std::path::Path>| {
        let mut cmd = Command::new(bin);
        cmd.current_dir(cwd).args(args).env_remove("DEDUPFS_STORE");
        if let Some(s) = env_store {
            cmd.env("DEDUPFS_STORE", s);
        }
        let out = cmd.output().expect("run dedupfs");
        assert!(
            out.status.success(),
            "dedupfs {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Inside store dir: omit `<store>` everywhere!
    run_in(&store_dir, &["init"], None);
    let out = run_in(
        &store_dir,
        &["import", src_dir.to_str().unwrap(), "--name", "ws1", "--update"],
        None,
    );
    assert!(out.contains("updated `ws1`"));
    let list_out = run_in(&store_dir, &["list"], None);
    assert!(list_out.contains("ws1"));
    run_in(&store_dir, &["clone", "ws1", "ws2"], None);
    let stats_out = run_in(&store_dir, &["stats"], None);
    assert!(stats_out.contains("workspaces     : 2"));
    run_in(&store_dir, &["dedup"], None);
    run_in(&store_dir, &["gc"], None);
    run_in(&store_dir, &["rm", "ws2"], None);

    // Outside store dir: discovered via DEDUPFS_STORE and explicit `--store`!
    let env_stats = run_in(&other_dir, &["stats"], Some(&store_dir));
    assert!(env_stats.contains("workspaces     : 1"));
    let flag_stats = run_in(
        &other_dir,
        &["--store", store_dir.to_str().unwrap(), "stats"],
        None,
    );
    assert!(flag_stats.contains("workspaces     : 1"));
}

#[test]
fn directory_filters_exclude_on_import_and_skip_dedup_at_runtime() {
    let tmp = TempDir::new("dir_filters");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(src.join("Assets")).unwrap();
    std::fs::create_dir_all(src.join("Temp")).unwrap();
    std::fs::create_dir_all(src.join("Library").join("ShaderCache")).unwrap();
    std::fs::create_dir_all(src.join("Library").join("Artifacts")).unwrap();
    std::fs::write(src.join("Assets").join("hero.txt"), b"keep-hero").unwrap();
    std::fs::write(src.join("Temp").join("UnityLockfile"), b"ephemeral-temp").unwrap();
    std::fs::write(
        src.join("Library").join("ShaderCache").join("s.bin"),
        b"shader-cache",
    )
    .unwrap();
    std::fs::write(
        src.join("Library").join("Artifacts").join("a.bin"),
        b"artifact-data",
    )
    .unwrap();

    let cfg = StoreConfig {
        import_exclude: vec!["Temp".to_owned(), "Library/ShaderCache".to_owned()],
        no_dedup: vec!["Temp".to_owned(), "Logs".to_owned()],
        ..StoreConfig::default()
    };
    let store = DedupStore::init(&store_dir, cfg).unwrap();

    // 1. Import skips Temp/ and Library/ShaderCache/, keeps Assets/ and Library/Artifacts/.
    let sum = store.import_dir(&src, "unity", 2, None).unwrap();
    assert_eq!(sum.files, 2, "only hero.txt and Artifacts/a.bin imported");
    assert_eq!(sum.excluded_entries, 2, "Temp and Library/ShaderCache excluded");

    let backend = DedupWorkspaceBackend::open(store.clone(), "unity").unwrap();
    assert!(backend
        .open_dir(&OpsPath::new(b"/Temp"))
        .is_err());
    assert!(backend
        .open_dir(&OpsPath::new(b"/Library/ShaderCache"))
        .is_err());
    assert!(backend
        .create_or_open(&OpsPath::new(b"/Assets/hero.txt"), Disposition::Open)
        .is_ok());

    // 2. At runtime (e.g. Unity opens project), Unity creates `/Temp/runtime.tmp` and `/Assets/new.txt`.
    //    `dedup_workspace` must deduplicate `/Assets/new.txt` into packs, while keeping `/Temp/runtime.tmp`
    //    in the local loose overlay (skipped via `no_dedup`)!
    backend.mkdir(&OpsPath::new(b"/Temp")).unwrap();
    let f_temp = backend
        .create_or_open(&OpsPath::new(b"/Temp/runtime.tmp"), Disposition::Create)
        .unwrap();
    backend.write(f_temp.file_id, 0, b"unity-temp-scratch").unwrap();

    let f_new = backend
        .create_or_open(&OpsPath::new(b"/Assets/new.txt"), Disposition::Create)
        .unwrap();
    backend.write(f_new.file_id, 0, b"real-asset-change").unwrap();
    backend.flush().unwrap();

    let dsum = store
        .dedup_workspace(Some("unity"), 0, |_, _| false)
        .unwrap();
    assert_eq!(dsum.deduped_files, 1, "Assets/new.txt should be deduped");
    assert_eq!(dsum.skipped_filtered, 1, "Temp/runtime.tmp must skip dedup");
    assert!(
        store
            .overlay()
            .file_path(backend.ws_id(), f_temp.file_id)
            .exists(),
        "Temp/runtime.tmp stays as local loose overlay file"
    );
    assert!(
        !store
            .overlay()
            .file_path(backend.ws_id(), f_new.file_id)
            .exists(),
        "Assets/new.txt overlay file reclaimed after dedup"
    );

    // 3. `DedupStore::update_config` persists config updates cleanly to `store.json`.
    let updated = DedupStore::update_config(&store_dir, |c| {
        c.import_exclude.push("Logs".to_owned());
    })
    .unwrap();
    assert!(updated.import_exclude.iter().any(|p| p == "Logs"));
    let reopened = DedupStore::open(&store_dir).unwrap();
    assert!(reopened
        .config()
        .import_exclude
        .iter()
        .any(|p| p == "Logs"));
}

#[test]
fn read_dir_prefetches_metadata_without_manifest_and_read_lazy_loads_bytes() {
    let tmp = TempDir::new("lazy_manifest");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(src.join("dir")).unwrap();
    let payload_a = b"hello-from-pack-file-A".repeat(1000);
    let payload_b = b"hello-from-pack-file-B".repeat(500);
    std::fs::write(src.join("dir").join("a.bin"), &payload_a).unwrap();
    std::fs::write(src.join("dir").join("b.bin"), &payload_b).unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.import_dir(&src, "ws", 2, None).unwrap();

    // Fresh backend with empty inode/dentry caches (cold start).
    let backend = DedupWorkspaceBackend::open(store, "ws").unwrap();
    let dir_id = backend.open_dir(&OpsPath::new(b"/dir")).unwrap();
    let entries = backend.read_dir(dir_id).unwrap();
    assert_eq!(entries.len(), 2);

    // Metadata (`get_info`) is already hot from `read_dir` prefetch.
    let a_id = entries
        .iter()
        .find(|e| e.name.as_slice() == b"a.bin")
        .unwrap()
        .file_id;
    let b_id = entries
        .iter()
        .find(|e| e.name.as_slice() == b"b.bin")
        .unwrap()
        .file_id;
    assert_eq!(backend.get_info(a_id).unwrap().size, payload_a.len() as u64);
    assert_eq!(backend.get_info(b_id).unwrap().size, payload_b.len() as u64);

    // `read()` must transparently lazy-load the manifest BLOB from SQLite and return exact bytes.
    let got_a = backend.read(a_id, 0, payload_a.len()).unwrap();
    assert_eq!(got_a, payload_a);
    let got_b = backend.read(b_id, 0, payload_b.len()).unwrap();
    assert_eq!(got_b, payload_b);
}

#[test]
fn overlay_pooled_handles_support_concurrent_reads_and_clean_eviction_on_delete() {
    let tmp = TempDir::new("overlay_pool");
    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store.clone(), "ws").unwrap());

    // Create a hot overlay DB file (like Unity's ArtifactDB) and write 256 KiB of patterned bytes.
    let opened = backend
        .create_or_open(&OpsPath::new(b"/ArtifactDB"), Disposition::Create)
        .unwrap();
    let mut payload = vec![0u8; 256 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i.wrapping_mul(31) ^ (i >> 8)) & 0xff) as u8;
    }
    backend.write(opened.file_id, 0, &payload).unwrap();

    // 8 threads perform 200 concurrent random reads each against the pooled handle.
    std::thread::scope(|s| {
        for tid in 0..8usize {
            let b = backend.clone();
            let p = &payload;
            let fid = opened.file_id;
            s.spawn(move || {
                for iter in 0..200usize {
                    let off = ((tid * 13 + iter * 97) % 60) * 4096;
                    let got = b.read(fid, off as u64, 4096).unwrap();
                    assert_eq!(&got[..], &p[off..off + 4096]);
                }
            });
        }
    });

    // Truncating and deleting the pooled overlay file must succeed cleanly without Windows sharing errors.
    backend.set_size(opened.file_id, 4096).unwrap();
    assert_eq!(backend.get_info(opened.file_id).unwrap().size, 4096);
    backend
        .remove_at(opened.binding.0, &opened.binding.1, opened.file_id)
        .unwrap();
    assert!(
        !store
            .overlay()
            .file_path(backend.ws_id(), opened.file_id)
            .exists(),
        "overlay file must be deleted cleanly after pool eviction"
    );
}

#[test]
fn parallel_import_preserves_exact_refcounts_and_fsck_repairs_drift() {
    let tmp = TempDir::new("p0_refcounts_fsck");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Create 60 duplicate files sharing the exact same payload across 8 parallel import workers
    // (including a larger file that shares chunks with smaller files to stress out-of-order commits).
    let shared_block = b"DEDUP-SHARED-CHUNK-DATA-0123456789ABCDEF".repeat(8000); // ~320 KiB
    let mut big_with_shared = shared_block.clone();
    big_with_shared.extend_from_slice(&vec![0xABu8; 2 * 1024 * 1024]);
    std::fs::write(src.join("00_big.bin"), &big_with_shared).unwrap();
    for i in 1..=60 {
        std::fs::write(src.join(format!("dup_{i:02}.bin")), &shared_block).unwrap();
    }

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    let sum = store.import_dir(&src, "ws", 8, None).unwrap();
    assert_eq!(sum.files, 61);

    // `fsck(false)` must report zero missing chunks, zero mismatched refcounts, and zero orphans!
    let check = store.fsck(false).unwrap();
    assert_eq!(check.files_scanned, 61);
    assert_eq!(check.missing_chunks, 0);
    assert_eq!(check.mismatched_refcounts, 0);
    assert_eq!(check.orphan_chunks, 0);
    assert_eq!(check.pack_accounting_fixed, 0);

    // Deliberately corrupt a refcount in SQLite and verify `fsck(true)` detects and repairs it!
    {
        let conn = store.open_conn().unwrap();
        conn.execute("UPDATE chunks SET refcount = 1", []).unwrap();
    }
    let broken = store.fsck(false).unwrap();
    assert!(broken.mismatched_refcounts > 0);

    let fixed = store.fsck(true).unwrap();
    assert!(fixed.mismatched_refcounts > 0);
    let after = store.fsck(false).unwrap();
    assert_eq!(after.mismatched_refcounts, 0);
    assert_eq!(after.missing_chunks, 0);
    assert_eq!(after.pack_accounting_fixed, 0);
}

#[test]
fn batched_write_prefix_cow_read_into_and_dir_negative_cache() {
    let tmp = TempDir::new("p1_p2_cow_neg_cache");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();

    // Create a multi-chunk (~2 MiB) pseudo-random file so it spans many chunks in packs.
    let mut big = vec![0u8; 2 * 1024 * 1024];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for b in &mut big {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    std::fs::write(src.join("sub").join("multi.bin"), &big).unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.import_dir(&src, "ws", 4, None).unwrap();

    let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();

    // 1. Directory `complete` negative cache: read_dir loads /sub completely.
    let sub_id = backend.open_dir(&OpsPath::new(b"/sub")).unwrap();
    let entries = backend.read_dir_with_info(sub_id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1.size, big.len() as u64);

    // Missing lookup must hit negative cache and return NotFound.
    assert!(backend
        .create_or_open(&OpsPath::new(b"/sub/missing.meta"), Disposition::Open)
        .is_err());

    // Creating that file must invalidate/update the negative cache so Open finds it.
    let created = backend
        .create_or_open(&OpsPath::new(b"/sub/missing.meta"), Disposition::Create)
        .unwrap();
    backend.write(created.file_id, 0, b"hello-meta").unwrap();
    assert_eq!(backend.get_info(created.file_id).unwrap().size, 10);

    // `read_dir_with_info` must reflect buffered dirty write size immediately even before flush!
    let entries2 = backend.read_dir_with_info(sub_id).unwrap();
    let meta_entry = entries2
        .iter()
        .find(|(name, _)| name.as_slice() == b"missing.meta")
        .unwrap();
    assert_eq!(meta_entry.1.size, 10);

    // Per-file flush persists metadata + syncs data cleanly.
    backend.flush_file(created.file_id).unwrap();

    // Removing the file must return NotFound on subsequent open (negative cache stays consistent).
    backend
        .remove_at(created.binding.0, &created.binding.1, created.file_id)
        .unwrap();
    assert!(backend
        .create_or_open(&OpsPath::new(b"/sub/missing.meta"), Disposition::Open)
        .is_err());

    // 2. Zero-copy `read_into` across multi-chunk boundaries using binary search over chunk_offsets.
    let multi = backend
        .create_or_open(&OpsPath::new(b"/sub/multi.bin"), Disposition::Open)
        .unwrap();
    let mut slice_buf = vec![0u8; 300_000];
    let n = backend
        .read_into(multi.file_id, 900_000, &mut slice_buf)
        .unwrap();
    assert_eq!(n, 300_000);
    assert_eq!(&slice_buf[..], &big[900_000..1_200_000]);

    // 3. Prefix-only CoW truncation on pack-backed multi-chunk file (`set_size`).
    let prefix_len = 500_000usize;
    backend.set_size(multi.file_id, prefix_len as u64).unwrap();
    assert_eq!(
        backend.get_info(multi.file_id).unwrap().size,
        prefix_len as u64
    );
    let mut got_prefix = vec![0u8; prefix_len];
    let n2 = backend.read_into(multi.file_id, 0, &mut got_prefix).unwrap();
    assert_eq!(n2, prefix_len);
    assert_eq!(&got_prefix[..], &big[..prefix_len]);

    // Refcounts must remain exact after prefix CoW released all pack chunks!
    let check = store.fsck(false).unwrap();
    assert_eq!(check.missing_chunks, 0);
    assert_eq!(check.mismatched_refcounts, 0);
    assert_eq!(check.orphan_chunks, 0);
}

#[test]
fn r2_1_concurrent_overwrite_and_write_has_no_abba_deadlock() {
    let tmp = TempDir::new("r2_1_no_deadlock");
    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store, "ws").unwrap());

    let f = backend
        .create_or_open(&OpsPath::new(b"/hot.bin"), Disposition::Create)
        .unwrap();
    backend.write(f.file_id, 0, &[0x42u8; 4096]).unwrap();

    // Hammer concurrent Overwrite/Supersede (which used to lock write_conn -> inode) against
    // concurrent write/set_size/flush_file (which lock inode -> write_conn).
    std::thread::scope(|s| {
        for tid in 0..4usize {
            let b = backend.clone();
            s.spawn(move || {
                let path = OpsPath::new(b"/hot.bin");
                for i in 0..100usize {
                    let disp = if (tid + i) % 2 == 0 {
                        Disposition::Overwrite
                    } else {
                        Disposition::Supersede
                    };
                    let out = b.create_or_open(&path, disp).unwrap();
                    assert_eq!(out.file_id, f.file_id);
                }
            });
        }
        for tid in 0..4usize {
            let b = backend.clone();
            s.spawn(move || {
                let payload = vec![(tid as u8) + 1; 2048];
                for i in 0..100usize {
                    b.write(f.file_id, (i as u64) * 16, &payload).unwrap();
                    if i % 10 == 0 {
                        b.set_size(f.file_id, 512).unwrap();
                    }
                    if i % 15 == 0 {
                        b.flush_file(f.file_id).unwrap();
                    }
                }
            });
        }
    });
}

#[test]
fn r2_2_concurrent_read_dir_and_create_remove_never_hides_or_resurrects_files() {
    let tmp = TempDir::new("r2_2_neg_cache_race");
    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store, "ws").unwrap());

    let dir_id = backend.mkdir(&OpsPath::new(b"/race")).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|s| {
        let mut readers = Vec::new();
        for _ in 0..4 {
            let b = backend.clone();
            let st = stop.clone();
            readers.push(s.spawn(move || {
                while !st.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = b.read_dir_with_info(dir_id).unwrap();
                }
            }));
        }
        let mut writers = Vec::new();
        for tid in 0..2usize {
            let b = backend.clone();
            writers.push(s.spawn(move || {
                for i in 0..80usize {
                    let keep_name = format!("/race/live_{tid}_{i}.txt");
                    let created = b
                        .create_or_open(&OpsPath::new(keep_name.as_bytes()), Disposition::Create)
                        .unwrap();
                    b.write(created.file_id, 0, b"live").unwrap();

                    let gone_name = format!("/race/gone_{tid}_{i}.txt");
                    let gone = b
                        .create_or_open(&OpsPath::new(gone_name.as_bytes()), Disposition::Create)
                        .unwrap();
                    b.remove_at(gone.binding.0, &gone.binding.1, gone.file_id)
                        .unwrap();
                }
            }));
        }
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
    });

    // EVERY created `live_*` file must be visible via Open (not hidden by stale negative cache),
    // and EVERY deleted `gone_*` file must return NotFound (not resurrected by stale snapshot).
    for tid in 0..2usize {
        for i in 0..80usize {
            let keep_name = format!("/race/live_{tid}_{i}.txt");
            assert!(
                backend
                    .create_or_open(&OpsPath::new(keep_name.as_bytes()), Disposition::Open)
                    .is_ok(),
                "created file {keep_name} must never be hidden by negative cache!"
            );
            let gone_name = format!("/race/gone_{tid}_{i}.txt");
            assert!(
                backend
                    .create_or_open(&OpsPath::new(gone_name.as_bytes()), Disposition::Open)
                    .is_err(),
                "deleted file {gone_name} must never be resurrected by read_dir cache!"
            );
        }
    }
}

#[test]
fn r2_3_r2_4_r2_5_dirty_authority_dedup_guard_cleanup_flush_and_mount_reconcile() {
    let tmp = TempDir::new("r2_3_4_5_dirty_authority");
    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store.clone(), "ws").unwrap());

    let f = backend
        .create_or_open(&OpsPath::new(b"/grow.bin"), Disposition::Create)
        .unwrap();
    // Write 10 KiB (buffered in dirty_inodes, DB still has size=0).
    let initial = vec![0xAAu8; 10_240];
    backend.write(f.file_id, 0, &initial).unwrap();
    assert!(backend.is_file_dirty(f.file_id));

    // 1. R2-3: Even if inode_cache loses the entry, `get_info`, `read_into`, and slow-path
    //    `write(offset=0, small)` MUST consult `dirty_inodes` and NEVER regress size!
    store.invalidate_inode(backend.ws_id(), f.file_id);
    assert_eq!(backend.get_info(f.file_id).unwrap().size, 10_240);

    store.invalidate_inode(backend.ws_id(), f.file_id);
    let mut rbuf = vec![0u8; 10_240];
    assert_eq!(backend.read_into(f.file_id, 0, &mut rbuf).unwrap(), 10_240);
    assert_eq!(rbuf, initial);

    // Slow-path overwrite of first 16 bytes when cache is cold must preserve full 10_240 size!
    store.invalidate_inode(backend.ws_id(), f.file_id);
    backend.write(f.file_id, 0, b"0123456789ABCDEF").unwrap();
    assert_eq!(backend.get_info(f.file_id).unwrap().size, 10_240);

    // Concurrent appending writes + `read_dir_with_info` must never cause size regression.
    let root_id = backend.open_dir(&OpsPath::new(b"/")).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|s| {
        let b1 = backend.clone();
        let st = stop.clone();
        let reader = s.spawn(move || {
            while !st.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = b1.read_dir_with_info(root_id).unwrap();
            }
        });
        for i in 0..50u64 {
            let off = 10_240 + i * 512;
            backend.write(f.file_id, off, &[0xBBu8; 512]).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
    });
    let expected_size = 10_240 + 50 * 512;
    assert_eq!(backend.get_info(f.file_id).unwrap().size, expected_size);

    // 2. R2-4: Same-size dirty overwrite after flush must be skipped by dedup when `is_busy`
    //    checks `is_file_dirty`, and `on_cleanup` must persist it so subsequent dedup stores NEW data!
    backend.flush().unwrap();
    assert!(!backend.is_file_dirty(f.file_id));
    backend.write(f.file_id, 0, b"NEW_HEADER_16B!!").unwrap();
    assert!(backend.is_file_dirty(f.file_id));

    let skipped = store
        .dedup_workspace(Some("ws"), 0, |_, id| backend.is_file_dirty(id))
        .unwrap();
    assert_eq!(skipped.skipped_busy, 1);
    assert_eq!(skipped.deduped_files, 0);

    // Simulating WinFsp Cleanup flushes the dirty inode immediately.
    backend.on_cleanup(f.file_id).unwrap();
    assert!(!backend.is_file_dirty(f.file_id));

    let deduped = store
        .dedup_workspace(Some("ws"), 0, |_, id| backend.is_file_dirty(id))
        .unwrap();
    assert_eq!(deduped.deduped_files, 1);
    assert_eq!(
        backend.read(f.file_id, 0, 16).unwrap(),
        b"NEW_HEADER_16B!!"
    );

    // 3. R2-9: `flush_file` succeeds even when overlay handle was evicted from the pool.
    let f2 = backend
        .create_or_open(&OpsPath::new(b"/sync_evicted.bin"), Disposition::Create)
        .unwrap();
    backend.write(f2.file_id, 0, b"durable-after-evict").unwrap();
    store.overlay().evict_handle(backend.ws_id(), f2.file_id);
    backend.flush_file(f2.file_id).unwrap();
    assert!(!backend.is_file_dirty(f2.file_id));

    // 4. R2-5: Mount-time overlay reconcile recovers file size if SQLite lagged behind NTFS after a crash.
    {
        let conn = store.open_conn().unwrap();
        conn.execute(
            "UPDATE inodes SET size = 0 WHERE ws_id = ?1 AND inode_id = ?2",
            rusqlite::params![backend.ws_id() as i64, f2.file_id as i64],
        )
        .unwrap();
    }
    store.invalidate_inode(backend.ws_id(), f2.file_id);
    let reopened = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
    assert_eq!(
        reopened.get_info(f2.file_id).unwrap().size,
        b"durable-after-evict".len() as u64
    );
}

#[test]
fn r3_parallel_overlapped_pack_reads_and_single_flight_decompress() {
    let tmp = TempDir::new("r3_parallel_overlapped");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // 2 MiB pseudo-random multi-chunk file stored in packs.
    let mut big = vec![0u8; 2 * 1024 * 1024];
    let mut x: u64 = 0x517c_c1b7_2722_0a95;
    for b in &mut big {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
    std::fs::write(src.join("blob.bin"), &big).unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.import_dir(&src, "ws", 4, None).unwrap();
    drop(store);

    // Reopen fresh store so chunk cache is 100% cold and triggers single-flight decompression +
    // concurrent Windows FILE_FLAG_OVERLAPPED reads on the same .pack handle.
    let cold_store = DedupStore::open(&store_dir).unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(cold_store, "ws").unwrap());
    let opened = backend
        .create_or_open(&OpsPath::new(b"/blob.bin"), Disposition::Open)
        .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    std::thread::scope(|s| {
        for tid in 0..16usize {
            let b = backend.clone();
            let bar = barrier.clone();
            let data = &big;
            let fid = opened.file_id;
            s.spawn(move || {
                bar.wait();
                // First wave: all 16 threads hammer the exact same cold chunks simultaneously (single-flight).
                let mut same_buf = vec![0u8; 128 * 1024];
                let n = b.read_into(fid, 0, &mut same_buf).unwrap();
                assert_eq!(n, same_buf.len());
                assert_eq!(&same_buf[..], &data[..same_buf.len()]);

                // Second wave: parallel scattered overlapped reads across the full 2 MiB pack-backed file.
                for iter in 0..40usize {
                    let off = ((tid * 37 + iter * 73) % 450) * 4096;
                    let len = 8192usize.min(data.len() - off);
                    let mut buf = vec![0u8; len];
                    let got = b.read_into(fid, off as u64, &mut buf).unwrap();
                    assert_eq!(got, len);
                    assert_eq!(&buf[..], &data[off..off + len]);
                }
            });
        }
    });
}

#[test]
fn r3_atomic_concurrent_write_append_has_no_lost_records() {
    let tmp = TempDir::new("r3_atomic_append");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let seed = b"SEED_HEADER_16B!";
    std::fs::write(src.join("packed.log"), seed).unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.import_dir(&src, "ws", 2, None).unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store, "ws").unwrap());

    // Test both a fresh empty overlay file AND a pack-backed file (CoW on first append).
    let fresh = backend
        .create_or_open(&OpsPath::new(b"/fresh.log"), Disposition::Create)
        .unwrap();
    let packed = backend
        .create_or_open(&OpsPath::new(b"/packed.log"), Disposition::Open)
        .unwrap();

    for (fid, prefix_len) in [(fresh.file_id, 0usize), (packed.file_id, seed.len())] {
        let threads = 8usize;
        let per_thread = 50usize;
        let rec_len = 16usize;
        let offsets = std::sync::Mutex::new(Vec::<u64>::new());

        std::thread::scope(|s| {
            for tid in 0..threads {
                let b = backend.clone();
                let offs = &offsets;
                s.spawn(move || {
                    for iter in 0..per_thread {
                        let mut rec = [0u8; 16];
                        rec[0] = tid as u8;
                        rec[1] = iter as u8;
                        for (k, slot) in rec[2..].iter_mut().enumerate() {
                            *slot = ((tid * 17 + iter * 31 + k) & 0xff) as u8;
                        }
                        let (at, written) = b.write_append(fid, &rec).unwrap();
                        assert_eq!(written, rec_len);
                        offs.lock().unwrap().push(at);
                    }
                });
            }
        });

        let expected_size = (prefix_len + threads * per_thread * rec_len) as u64;
        assert_eq!(backend.get_info(fid).unwrap().size, expected_size);

        // Every returned append offset must be unique and tightly packed at 16-byte stride.
        let mut offs = offsets.into_inner().unwrap();
        offs.sort_unstable();
        for (idx, off) in offs.iter().enumerate() {
            assert_eq!(*off, (prefix_len + idx * rec_len) as u64);
        }

        // Read back the entire file and verify every (tid, iter) record is intact and present once.
        let all = backend.read(fid, 0, expected_size as usize).unwrap();
        assert_eq!(all.len(), expected_size as usize);
        if prefix_len > 0 {
            assert_eq!(&all[..prefix_len], seed);
        }
        let mut seen = vec![vec![false; per_thread]; threads];
        for chunk in all[prefix_len..].chunks_exact(rec_len) {
            let tid = chunk[0] as usize;
            let iter = chunk[1] as usize;
            assert!(!seen[tid][iter], "duplicate record ({tid}, {iter})");
            for (k, &byte) in chunk[2..].iter().enumerate() {
                assert_eq!(byte, ((tid * 17 + iter * 31 + k) & 0xff) as u8);
            }
            seen[tid][iter] = true;
        }
        assert!(seen.into_iter().flatten().all(|v| v));
    }
}

#[test]
fn r3_clean_marker_cached_read_dir_noop_basic_info_and_wal_checkpoint() {
    let tmp = TempDir::new("r3_clean_marker_and_caches");
    let store_dir = tmp.0.join("store");
    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();

    let ws_id = {
        let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
        let ws_id = backend.ws_id();
        // Clean marker must NOT exist while workspace is mounted/open.
        assert!(!store.overlay().clean_marker_path(ws_id).exists());

        let dir_id = backend.mkdir(&OpsPath::new(b"/dir")).unwrap();
        for i in 0..5 {
            let p = format!("/dir/f{i}.txt");
            let f = backend
                .create_or_open(&OpsPath::new(p.as_bytes()), Disposition::Create)
                .unwrap();
            backend.write(f.file_id, 0, format!("payload-{i}").as_bytes()).unwrap();
            backend.flush_file(f.file_id).unwrap();

            // No-op set_times and set_attributes short-circuit should succeed and preserve metadata.
            let info = backend.get_info(f.file_id).unwrap();
            backend
                .set_times(
                    f.file_id,
                    Some(info.times.accessed),
                    Some(info.times.written),
                    Some(info.times.changed),
                    Some(info.times.created),
                )
                .unwrap();
            backend
                .set_attributes(f.file_id, fs_winfsp::Attributes(info.attributes.0 & 0x27))
                .unwrap();
            let after = backend.get_info(f.file_id).unwrap();
            assert_eq!(after.times, info.times);
            assert_eq!(after.attributes, info.attributes);
        }

        // First read_dir warms the dir cache; second hits the zero-SQLite cache path.
        let first = backend.read_dir_with_info(dir_id).unwrap();
        let second = backend.read_dir_with_info(dir_id).unwrap();
        assert_eq!(first.len(), 5);
        assert_eq!(first.len(), second.len());
        for ((n1, i1), (n2, i2)) in first.iter().zip(second.iter()) {
            assert_eq!(n1, n2);
            assert_eq!(i1.size, i2.size);
        }

        // Explicit WAL checkpoint works cleanly.
        backend.checkpoint_wal().unwrap();
        ws_id
    };

    // Dropping the backend cleanly must write the `.clean` marker.
    assert!(store.overlay().clean_marker_path(ws_id).exists());

    // Reopening consumes/removes the `.clean` marker and skips reconcile cleanly.
    let reopened = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
    assert!(!store.overlay().clean_marker_path(ws_id).exists());
    let dir_id = reopened.open_dir(&OpsPath::new(b"/dir")).unwrap();
    assert_eq!(reopened.read_dir_with_info(dir_id).unwrap().len(), 5);
    drop(reopened);

    // Durable dedup + gc path (Safety A: sync_data + synchronous=FULL) keeps fsck clean.
    let dsum = store.dedup_workspace(Some("ws"), 0, |_, _| false).unwrap();
    assert_eq!(dsum.deduped_files, 5);
    let gsum = store.gc(50).unwrap();
    assert_eq!(gsum.deleted_empty_packs, 0);
    let check = store.fsck(false).unwrap();
    assert_eq!(check.missing_chunks, 0);
    assert_eq!(check.mismatched_refcounts, 0);
    assert_eq!(check.orphan_chunks, 0);
}

#[test]
fn r4_single_write_conn_concurrent_foreground_and_dedup_plus_single_pass_open_entry() {
    use fs_winfsp::ops::{Kind, OpenTarget, OpsError};

    let tmp = TempDir::new("r4_single_conn_open_entry");
    let store = DedupStore::init(tmp.0.join("store"), StoreConfig::default()).unwrap();
    store.create_workspace("ws").unwrap();
    let backend = std::sync::Arc::new(DedupWorkspaceBackend::open(store.clone(), "ws").unwrap());

    // 1. Single-pass `open_entry` and `lookup_info` for root, dirs, files, and OpenTarget rules.
    let root_info = backend.lookup_info(&OpsPath::new(b"/")).unwrap();
    assert_eq!(root_info.kind, Kind::Directory);
    let root_open = backend
        .open_entry(&OpsPath::new(b"/"), Disposition::Open, OpenTarget::Any)
        .unwrap();
    assert_eq!(root_open.kind, Kind::Directory);
    assert!(root_open.binding.1.is_empty());

    let dir_out = backend
        .open_entry(
            &OpsPath::new(b"/Assets"),
            Disposition::Create,
            OpenTarget::DirOnly,
        )
        .unwrap();
    assert_eq!(dir_out.kind, Kind::Directory);
    assert_eq!(dir_out.binding.1, b"Assets");

    // Re-opening existing dir with OpenIf + DirOnly returns same id; FileOnly must return IsADirectory.
    let dir_again = backend
        .open_entry(
            &OpsPath::new(b"/assets"),
            Disposition::OpenIf,
            OpenTarget::DirOnly,
        )
        .unwrap();
    assert_eq!(dir_again.file_id, dir_out.file_id);
    assert_eq!(
        backend
            .open_entry(
                &OpsPath::new(b"/assets"),
                Disposition::OpenIf,
                OpenTarget::FileOnly,
            )
            .unwrap_err(),
        OpsError::IsADirectory
    );

    let file_out = backend
        .open_entry(
            &OpsPath::new(b"/Assets/Scene.unity"),
            Disposition::Create,
            OpenTarget::FileOnly,
        )
        .unwrap();
    assert_eq!(file_out.kind, Kind::File);
    assert_eq!(
        backend
            .open_entry(
                &OpsPath::new(b"/Assets/Scene.unity"),
                Disposition::Open,
                OpenTarget::DirOnly,
            )
            .unwrap_err(),
        OpsError::NotADirectory
    );
    assert_eq!(
        backend
            .lookup_info(&OpsPath::new(b"/assets/scene.unity"))
            .unwrap()
            .file_id,
        file_out.file_id
    );

    // 2. Concurrent foreground writes + flushes + checkpoint_wal + background dedup sharing the
    //    single unified `DedupStore::meta` write connection (zero SQLITE_BUSY, zero deadlock).
    let stop2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|s| {
        let mut writers = Vec::new();
        for tid in 0..4usize {
            let b = backend.clone();
            writers.push(s.spawn(move || {
                let payload = vec![(tid as u8).wrapping_add(0x60); 8192];
                for i in 0..30usize {
                    let p = format!("/Assets/g_{tid}_{i}.bin");
                    let out = b
                        .open_entry(
                            &OpsPath::new(p.as_bytes()),
                            Disposition::Create,
                            OpenTarget::FileOnly,
                        )
                        .unwrap();
                    b.write(out.file_id, 0, &payload).unwrap();
                    b.on_cleanup(out.file_id).unwrap();
                }
            }));
        }
        let bg_stop = stop2.clone();
        let bg_store = store.clone();
        let bg_backend = backend.clone();
        let bg = s.spawn(move || {
            while !bg_stop.load(std::sync::atomic::Ordering::Relaxed) {
                bg_backend.flush().unwrap();
                bg_backend.checkpoint_wal().unwrap();
                bg_store
                    .dedup_workspace(Some("ws"), 0, |_, id| bg_backend.is_file_dirty(id))
                    .unwrap();
            }
        });
        for w in writers {
            w.join().unwrap();
        }
        stop2.store(true, std::sync::atomic::Ordering::Relaxed);
        bg.join().unwrap();
    });

    backend.flush().unwrap();
    let check = store.fsck(false).unwrap();
    assert_eq!(check.missing_chunks, 0);
    assert_eq!(check.mismatched_refcounts, 0);
    assert_eq!(check.orphan_chunks, 0);
}

#[test]
fn r4_n3_n12_cow_overlay_sync_and_durable_import() {
    let tmp = TempDir::new("r4_n3_n12_durability");
    let store_dir = tmp.0.join("store");
    let src = tmp.0.join("src");
    std::fs::create_dir_all(&src).unwrap();

    // Include both pack-backed files AND a pinned overlay hot DB file (`ArtifactDB`).
    let pack_payload = b"DURABLE-IMPORT-CHUNK-0123456789ABCDEF".repeat(2000);
    std::fs::write(src.join("asset.bin"), &pack_payload).unwrap();
    std::fs::write(src.join("trunc.bin"), &pack_payload).unwrap();
    std::fs::write(src.join("ArtifactDB"), b"sqlite-hot-db-payload").unwrap();

    let store = DedupStore::init(&store_dir, StoreConfig::default()).unwrap();
    let sum = store.import_dir(&src, "ws", 4, None).unwrap();
    assert_eq!(sum.files, 3);
    assert_eq!(sum.overlay_pinned_files, 1);

    // N12: `PRAGMA synchronous` must be restored to NORMAL (1) after the final FULL commit.
    {
        let conn = store.open_conn().unwrap();
        let sync_mode: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!(sync_mode, 1);
    }

    // N3: CoW write & CoW set_size sync overlay before releasing old chunks.
    {
        let backend = DedupWorkspaceBackend::open(store.clone(), "ws").unwrap();
        let f1 = backend
            .create_or_open(&OpsPath::new(b"/asset.bin"), Disposition::Open)
            .unwrap();
        backend.write(f1.file_id, 0, b"PATCHED!").unwrap();

        let f2 = backend
            .create_or_open(&OpsPath::new(b"/trunc.bin"), Disposition::Open)
            .unwrap();
        backend.set_size(f2.file_id, 4096).unwrap();
    }

    // GC must reclaim dead pack space without losing either CoW overlay file.
    let _ = store.gc(0).unwrap();
    drop(store);

    let reopened = DedupStore::open(&store_dir).unwrap();
    let check = reopened.fsck(false).unwrap();
    assert_eq!(check.missing_chunks, 0);
    assert_eq!(check.mismatched_refcounts, 0);
    assert_eq!(check.orphan_chunks, 0);

    let backend2 = DedupWorkspaceBackend::open(reopened, "ws").unwrap();
    let f1 = backend2
        .create_or_open(&OpsPath::new(b"/asset.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(backend2.read(f1.file_id, 0, 8).unwrap(), b"PATCHED!");
    assert_eq!(
        backend2.read(f1.file_id, 8, 32).unwrap(),
        &pack_payload[8..40]
    );
    let f2 = backend2
        .create_or_open(&OpsPath::new(b"/trunc.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(backend2.get_info(f2.file_id).unwrap().size, 4096);
    assert_eq!(backend2.read(f2.file_id, 0, 4096).unwrap(), &pack_payload[..4096]);
    let db = backend2
        .create_or_open(&OpsPath::new(b"/ArtifactDB"), Disposition::Open)
        .unwrap();
    assert_eq!(backend2.read(db.file_id, 0, 21).unwrap(), b"sqlite-hot-db-payload");
}
