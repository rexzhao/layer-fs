//! End-to-end tests for `fs-layerfs`: upstream LayerFS storage + history + WinFspBackend.

use std::path::PathBuf;

use fs_layerfs::{PersistenceProfile, Repo, DEFAULT_BRANCH, DEFAULT_VOLUME_SIZE_BYTES};
use fs_semantics::disposition::Disposition;
use fs_winfsp::ops::{Attributes, Kind, Path, WorkspaceOps};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "fs-layerfs-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn full_lifecycle_create_write_case_fold_rename_commit_fork_merge_publish_diff() {
    let tmp = TempDir::new("lifecycle");
    let repo_dir = tmp.0.join("repo");

    let repo = Repo::init(
        &repo_dir,
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("init repo");

    // Open default writable branch `work`.
    let ws = repo.open_workspace(DEFAULT_BRANCH).expect("open work");
    let ops = ws.backend();

    // Create subdirectory and file (case-preserving).
    let dir_id = ops.mkdir(&Path::new(b"/SubDir")).expect("mkdir /SubDir");
    assert_eq!(ops.open_dir(&Path::new(b"/subdir")).unwrap(), dir_id);

    let created = ops
        .create_or_open(&Path::new(b"/SubDir/ReadMe.txt"), Disposition::Create)
        .expect("create file");
    assert_eq!(created.kind, Kind::File);

    // Case-insensitive collision on Create must fail with NameCollision!
    let err = ops
        .create_or_open(&Path::new(b"/subdir/readme.txt"), Disposition::Create)
        .unwrap_err();
    assert_eq!(err, fs_winfsp::OpsError::NameCollision);

    // Write and read back through FileId.
    let payload = b"hello from upstream layerfs on windows!";
    assert_eq!(ops.write(created.file_id, 0, payload).unwrap(), payload.len());
    assert_eq!(ops.read(created.file_id, 0, payload.len()).unwrap(), payload);

    // ReadOnly attribute maps to POSIX write bit.
    ops.set_attributes(created.file_id, Attributes(0x01)).unwrap();
    let info = ops.get_info(created.file_id).unwrap();
    assert_ne!(info.attributes.0 & 0x01, 0);
    ops.set_attributes(created.file_id, Attributes(0x00)).unwrap();

    // Case-only rename in same directory works!
    let renamed = ops
        .rename(&Path::new(b"/subdir/readme.txt"), &Path::new(b"/SubDir/README.md"), false)
        .expect("case rename");
    assert_eq!(renamed.file_id, created.file_id);
    let entries = ops.read_dir(dir_id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, b"README.md");

    // Commit changes to `work`.
    let committed = ws.commit(Some("initial commit"), false).expect("commit");
    assert_eq!(committed.kind, "Committed");
    assert!(committed.commit.is_some());
    ws.close().expect("close work");

    // Fork two branches from `work`, edit different files, merge, diff, publish to `main`.
    repo.fork("feat-a", Some("work")).expect("fork feat-a");
    repo.fork("feat-b", Some("work")).expect("fork feat-b");

    {
        let wa = repo.open_workspace("feat-a").unwrap();
        let a_ops = wa.backend();
        let f = a_ops
            .create_or_open(&Path::new(b"/a.txt"), Disposition::Create)
            .unwrap();
        a_ops.write(f.file_id, 0, b"branch a").unwrap();
        wa.commit(Some("add a"), false).unwrap();
        wa.close().unwrap();
    }
    {
        let wb = repo.open_workspace("feat-b").unwrap();
        let b_ops = wb.backend();
        let f = b_ops
            .create_or_open(&Path::new(b"/b.txt"), Disposition::Create)
            .unwrap();
        b_ops.write(f.file_id, 0, b"branch b").unwrap();
        wb.commit(Some("add b"), false).unwrap();
        wb.close().unwrap();
    }

    let merged = repo
        .merge("feat-a", "feat-b", "merged", Some("merge a and b"))
        .expect("3-way merge");
    assert_eq!(merged.kind, "Merged");
    assert!(merged.conflicts.is_empty());

    let diffs = repo.diff_branches("work", "merged").expect("diff");
    let diff_paths: Vec<&str> = diffs.iter().map(|d| d.path.as_str()).collect();
    assert!(diff_paths.contains(&"/a.txt"));
    assert!(diff_paths.contains(&"/b.txt"));

    let published = repo.publish("merged", None).expect("publish to main");
    assert_eq!(published.kind, "Added");

    // Reopen repo from disk and verify merged files are readable!
    drop(repo);
    let reopened = Repo::open(&repo_dir).expect("reopen repo");
    let wm = reopened.open_workspace("merged").unwrap();
    let m_ops = wm.backend();
    let a = m_ops
        .create_or_open(&Path::new(b"/A.TXT"), Disposition::Open)
        .unwrap();
    assert_eq!(m_ops.read(a.file_id, 0, 8).unwrap(), b"branch a");
    let b = m_ops
        .create_or_open(&Path::new(b"/B.TXT"), Disposition::Open)
        .unwrap();
    assert_eq!(m_ops.read(b.file_id, 0, 8).unwrap(), b"branch b");
    wm.close().unwrap();
}

#[test]
fn durable_profile_init_and_reopen() {
    let tmp = TempDir::new("durable");
    let repo_dir = tmp.0.join("repo");
    let repo = Repo::init(
        &repo_dir,
        PersistenceProfile::Durable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("init durable");
    assert_eq!(repo.profile(), PersistenceProfile::Durable);
    let ws = repo.open_workspace(DEFAULT_BRANCH).unwrap();
    let ops = ws.backend();
    let f = ops
        .create_or_open(&Path::new(b"/durable.bin"), Disposition::Create)
        .unwrap();
    ops.write(f.file_id, 0, b"durable payload").unwrap();
    ws.commit(Some("durable"), false).unwrap();
    ws.close().unwrap();
    drop(repo);

    let reopened = Repo::open(&repo_dir).unwrap();
    assert_eq!(reopened.profile(), PersistenceProfile::Durable);
    let ws2 = reopened.open_workspace(DEFAULT_BRANCH).unwrap();
    let f2 = ws2
        .backend()
        .create_or_open(&Path::new(b"/durable.bin"), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws2.backend().read(f2.file_id, 0, 15).unwrap(),
        b"durable payload"
    );
    ws2.close().unwrap();
}

#[test]
fn remove_pagination_resize_times_and_windowed_io() {
    let tmp = TempDir::new("edgecases");
    let repo_dir = tmp.0.join("repo");
    let repo = Repo::init(
        &repo_dir,
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("init repo");
    let ws = repo.open_workspace(DEFAULT_BRANCH).unwrap();
    let ops = ws.backend();

    // 1. Pagination beyond PAGE_ROWS (64): create 100 files in one directory.
    let big_dir = ops.mkdir(&Path::new(b"/many")).unwrap();
    for i in 0..100 {
        let p = format!("/many/file-{i:03}.txt");
        ops.create_or_open(&Path::new(p.as_bytes()), Disposition::Create)
            .unwrap();
    }
    let entries = ops.read_dir(big_dir).unwrap();
    assert_eq!(entries.len(), 100, "read_dir must paginate past 64 rows");

    // 2. Non-empty directory cannot be removed.
    let binding = ops.dir_binding(&Path::new(b"/many")).unwrap();
    assert_eq!(
        ops.remove_at(binding.binding.0, &binding.binding.1, big_dir),
        Err(fs_winfsp::OpsError::NotEmpty)
    );

    // 3. Remove one file with guard check (wrong expected_id fails, right succeeds).
    let target = ops
        .create_or_open(&Path::new(b"/many/FILE-050.TXT"), Disposition::Open)
        .unwrap();
    assert!(matches!(
        ops.remove_at(target.binding.0, &target.binding.1, target.file_id + 999),
        Err(fs_winfsp::OpsError::InvalidRecord(_))
    ));
    ops.remove_at(target.binding.0, &target.binding.1, target.file_id)
        .unwrap();
    assert_eq!(ops.read_dir(big_dir).unwrap().len(), 99);

    // 4. Empty directory can be removed.
    let empty_dir = ops.mkdir(&Path::new(b"/EmptyDir")).unwrap();
    let empty_bind = ops.dir_binding(&Path::new(b"/emptydir")).unwrap();
    ops.remove_at(empty_bind.binding.0, &empty_bind.binding.1, empty_dir)
        .unwrap();
    assert_eq!(
        ops.open_dir(&Path::new(b"/EmptyDir")),
        Err(fs_winfsp::OpsError::NotFound)
    );

    // 5. set_size shrink + zero-extend + large multi-window I/O (1.5 MiB).
    let big = ops
        .create_or_open(&Path::new(b"/big.bin"), Disposition::Create)
        .unwrap();
    let payload: Vec<u8> = (0..1_500_000usize).map(|i| (i % 251) as u8).collect();
    assert_eq!(ops.write(big.file_id, 0, &payload).unwrap(), payload.len());
    assert_eq!(ops.read(big.file_id, 0, payload.len()).unwrap(), payload);

    ops.set_size(big.file_id, 10).unwrap();
    assert_eq!(ops.get_info(big.file_id).unwrap().size, 10);
    assert_eq!(ops.read(big.file_id, 0, 10).unwrap(), &payload[..10]);

    ops.set_size(big.file_id, 20).unwrap();
    let extended = ops.read(big.file_id, 0, 20).unwrap();
    assert_eq!(&extended[..10], &payload[..10]);
    assert_eq!(&extended[10..20], &[0u8; 10]);

    // 6. set_times persists mtime.
    let ts_ns: i64 = 1_700_000_000_123_456_789;
    ops.set_times(big.file_id, None, Some(ts_ns), None, None)
        .unwrap();
    let info = ops.get_info(big.file_id).unwrap();
    assert_eq!(info.times.written, ts_ns);

    ws.commit(Some("edgecases"), false).unwrap();
    ws.close().unwrap();
}

#[test]
fn init_from_host_directory_and_reclaim_space() {
    let tmp = TempDir::new("import");
    let src = tmp.0.join("src_tree");
    std::fs::create_dir_all(src.join("Nested").join("EmptyDir")).unwrap();
    std::fs::write(src.join("Root.txt"), b"root content").unwrap();
    let big: Vec<u8> = (0..300_000usize).map(|i| (i % 241) as u8).collect();
    std::fs::write(src.join("Nested").join("Data.bin"), &big).unwrap();

    // Create a read-only file and verify READONLY attribute maps cleanly through POSIX mode 0444.
    let ro_path = src.join("ReadOnly.txt");
    std::fs::write(&ro_path, b"readonly").unwrap();
    let mut perms = std::fs::metadata(&ro_path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&ro_path, perms).unwrap();

    // Refuses creating repo inside source directory!
    let inside = src.join("bad-repo");
    let err = Repo::init_from(
        &inside,
        &src,
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .unwrap_err();
    assert!(
        matches!(err, fs_layerfs::Error::InvalidInput(_)),
        "expected InvalidInput when repo lies inside source, got {err:?}"
    );
    assert!(!inside.join("store.sqlite").exists());

    // Real import into sibling repo directory.
    let repo_dir = tmp.0.join("repo");
    let (repo, imported) = Repo::init_from(
        &repo_dir,
        &src,
        PersistenceProfile::Durable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("init_from");
    assert_eq!(imported.entries, 6, "root + Nested + EmptyDir + 3 files");
    assert_eq!(imported.branches.len(), 1);
    assert_eq!(imported.branches[0].branch, "src_tree");

    let ws = repo.open_workspace("src_tree").unwrap();
    let ops = ws.backend();

    let f_root = ops
        .create_or_open(&Path::new(b"/root.txt"), Disposition::Open)
        .expect("case-insensitive lookup of imported Root.txt");
    assert_eq!(ops.read(f_root.file_id, 0, 12).unwrap(), b"root content");

    let f_big = ops
        .create_or_open(&Path::new(b"/nested/data.bin"), Disposition::Open)
        .expect("open imported Nested/Data.bin");
    assert_eq!(ops.read(f_big.file_id, 0, big.len()).unwrap(), big);

    let f_ro = ops
        .create_or_open(&Path::new(b"/ReadOnly.txt"), Disposition::Open)
        .expect("open imported ReadOnly.txt");
    assert_ne!(
        ops.get_info(f_ro.file_id).unwrap().attributes.0 & 0x01,
        0,
        "readonly source file must import with READONLY bit set"
    );

    let empty_id = ops
        .open_dir(&Path::new(b"/Nested/EmptyDir"))
        .expect("imported empty dir");
    assert!(ops.read_dir(empty_id).unwrap().is_empty());

    ws.close().unwrap();
    drop(repo);

    // Clear readonly before TempDir Drop removes `src`.
    let mut perms = std::fs::metadata(&ro_path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&ro_path, perms).unwrap();

    // Incremental SQLite space reclamation succeeds cleanly.
    let reclaimed = Repo::reclaim_space(&repo_dir, Some(64)).expect("reclaim_space");
    assert!(reclaimed.remaining_free_pages <= reclaimed.free_pages_before);
}

#[test]
fn init_from_multiple_directories_branches_diff_merge_and_zero_side_effects() {
    let tmp = TempDir::new("import-multi");
    let v1_app = tmp.0.join("v1").join("app");
    let v2_app = tmp.0.join("v2").join("app");
    let docs = tmp.0.join("docs");
    std::fs::create_dir_all(&v1_app).unwrap();
    std::fs::create_dir_all(&v2_app).unwrap();
    std::fs::create_dir_all(&docs).unwrap();

    std::fs::write(v1_app.join("only_a.txt"), b"from v1\n").unwrap();
    std::fs::write(v1_app.join("shared.bin"), vec![42u8; 64 * 1024]).unwrap();
    std::fs::write(v2_app.join("only_b.txt"), b"from v2\n").unwrap();
    std::fs::write(v2_app.join("shared.bin"), vec![42u8; 64 * 1024]).unwrap();
    std::fs::write(docs.join("readme.md"), b"# docs\n").unwrap();

    // 1. Duplicate inferred branch name (`app` vs `app`) fails with Conflict and ZERO side effects.
    let dup_repo = tmp.0.join("dup-repo");
    let dup_err = Repo::init_from_many(
        &dup_repo,
        &[
            fs_layerfs::ImportSource {
                path: v1_app.clone(),
                branch: None,
            },
            fs_layerfs::ImportSource {
                path: v2_app.clone(),
                branch: None,
            },
        ],
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .unwrap_err();
    assert!(
        matches!(dup_err, fs_layerfs::Error::Conflict(_)),
        "expected Conflict for duplicate branch name, got {dup_err:?}"
    );
    assert!(
        !dup_repo.exists(),
        "duplicate branch failure must not create repo directory"
    );

    // 2. Explicit duplicate branch name (`same` vs `same`) fails with ZERO side effects.
    let dup_explicit_repo = tmp.0.join("dup-explicit-repo");
    let dup_explicit_err = Repo::init_from_many(
        &dup_explicit_repo,
        &[
            fs_layerfs::ImportSource {
                path: v1_app.clone(),
                branch: Some("same".to_owned()),
            },
            fs_layerfs::ImportSource {
                path: docs.clone(),
                branch: Some("same".to_owned()),
            },
        ],
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .unwrap_err();
    assert!(matches!(dup_explicit_err, fs_layerfs::Error::Conflict(_)));
    assert!(!dup_explicit_repo.exists());

    // 3. Multi-directory import with explicit `:branch` disambiguation + auto-inferred `docs`.
    let repo_dir = tmp.0.join("multi-repo");
    let (repo, summary) = Repo::init_from_many(
        &repo_dir,
        &[
            fs_layerfs::ImportSource {
                path: v1_app.clone(),
                branch: Some("app-v1".to_owned()),
            },
            fs_layerfs::ImportSource {
                path: v2_app.clone(),
                branch: Some("app-v2".to_owned()),
            },
            fs_layerfs::ImportSource {
                path: docs.clone(),
                branch: None,
            },
        ],
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("init_from_many");
    let names: Vec<&str> = summary.branches.iter().map(|b| b.branch.as_str()).collect();
    assert_eq!(names, vec!["app-v1", "app-v2", "docs"]);

    // Verify each branch has its own files.
    let ws1 = repo.open_workspace("app-v1").unwrap();
    let f1 = ws1
        .backend()
        .create_or_open(&Path::new(b"/only_a.txt"), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws1.backend().read(f1.file_id, 0, f1.size as usize).unwrap(),
        b"from v1\n"
    );
    ws1.close().unwrap();

    let ws_docs = repo.open_workspace("docs").unwrap();
    let fd = ws_docs
        .backend()
        .create_or_open(&Path::new(b"/readme.md"), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws_docs
            .backend()
            .read(fd.file_id, 0, fd.size as usize)
            .unwrap(),
        b"# docs\n"
    );
    ws_docs.close().unwrap();

    // Verify `diff` and 3-way `merge` work across independently imported branches (shared genesis root_serial)!
    let diffs = repo.diff_branches("app-v1", "docs").unwrap();
    assert!(!diffs.is_empty(), "expected diff between app-v1 and docs");

    let merged = repo
        .merge("app-v1", "docs", "combined", Some("merge imported branches"))
        .expect("merge imported branches");
    assert!(
        merged.conflicts.is_empty(),
        "non-overlapping imported branches should merge cleanly: {:?}",
        merged.conflicts
    );
    let ws_m = repo.open_workspace("combined").unwrap();
    let ma = ws_m
        .backend()
        .create_or_open(&Path::new(b"/only_a.txt"), Disposition::Open)
        .unwrap();
    let md = ws_m
        .backend()
        .create_or_open(&Path::new(b"/readme.md"), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws_m.backend()
            .read(ma.file_id, 0, ma.size as usize)
            .unwrap(),
        b"from v1\n"
    );
    assert_eq!(
        ws_m.backend()
            .read(md.file_id, 0, md.size as usize)
            .unwrap(),
        b"# docs\n"
    );
    ws_m.close().unwrap();
}

#[cfg(windows)]
#[test]
fn init_from_imports_windows_reserved_device_names_and_long_paths() {
    let dir = TempDir::new("reserved_and_long");
    let src = dir.0.join("src");
    std::fs::create_dir_all(&src).unwrap();

    let canonical_src = std::fs::canonicalize(&src).unwrap();
    let verbatim_nul = canonical_src.join("nul");
    let seg_a = "a".repeat(80);
    let seg_b = "b".repeat(80);
    let seg_c = "c".repeat(80);
    let seg_d = "d".repeat(60);
    let deep_dir = canonical_src
        .join(&seg_a)
        .join(&seg_b)
        .join(&seg_c)
        .join(&seg_d);
    std::fs::create_dir_all(&deep_dir).unwrap();
    let deep_file = deep_dir.join("payload.txt");
    assert!(deep_file.as_os_str().len() > 280);
    std::fs::write(&verbatim_nul, b"reserved-nul-payload").unwrap();
    std::fs::write(&deep_file, b"deep-long-path-payload").unwrap();

    struct Cleanup(std::path::PathBuf, std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let _cleanup = Cleanup(verbatim_nul.clone(), canonical_src.join(&seg_a));

    let store_dir = dir.0.join("store");
    let (repo, summary) = Repo::init_from(
        &store_dir,
        &src,
        PersistenceProfile::Disposable,
        DEFAULT_VOLUME_SIZE_BYTES,
    )
    .expect("Repo::init_from must import literal nul + >260-char path");

    let ws = repo.open_workspace(&summary.branches[0].branch).unwrap();
    let nul_handle = ws
        .backend()
        .create_or_open(&Path::new(b"/nul"), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws.backend()
            .read(nul_handle.file_id, 0, nul_handle.size as usize)
            .unwrap(),
        b"reserved-nul-payload"
    );

    let deep_vfs_path = format!("/{seg_a}/{seg_b}/{seg_c}/{seg_d}/payload.txt");
    let deep_handle = ws
        .backend()
        .create_or_open(&Path::new(deep_vfs_path.as_bytes()), Disposition::Open)
        .unwrap();
    assert_eq!(
        ws.backend()
            .read(deep_handle.file_id, 0, deep_handle.size as usize)
            .unwrap(),
        b"deep-long-path-payload"
    );
    ws.close().unwrap();
}
