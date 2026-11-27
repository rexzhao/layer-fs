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
