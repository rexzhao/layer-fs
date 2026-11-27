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
                "mount",
                store_dir.to_str().unwrap(),
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
