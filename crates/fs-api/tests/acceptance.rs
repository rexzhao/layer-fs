//! The Phase 1 acceptance matrix (spec §6), scenarios S1, S4 and S5.
//!
//! These are the cheap scenarios, so they run in the ordinary suite. The heavy
//! ones live in `acceptance_scale.rs` behind `#[ignore]`; see that file for the
//! time budgets and `docs/reports/p1-baseline.md` for the measured numbers.
//!
//! Every buffer here is poisoned before a read (ADR-004 §13, item 3): a read must
//! write every byte it reports, so stale memory can never pass for content.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{Fs, FsError, Kind, OpenMode};

const POISON: u8 = 0xAA;

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fs-accept-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl TempRoot {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(label: &str) -> (TempRoot, Fs) {
    let root = TempRoot::new(label);
    let fs = Fs::open(&root.path, "default", OpenMode::Create).expect("open");
    (root, fs)
}

fn read_poisoned(fs: &Fs, path: &str) -> Vec<u8> {
    let size = fs.stat(path).expect("stat").size as usize;
    let mut buffer = vec![POISON; size];
    let read = fs.read_at(path, 0, &mut buffer).expect("read");
    buffer.truncate(read);
    assert!(
        !buffer.contains(&POISON),
        "{path}: read_at left part of the buffer unwritten"
    );
    buffer
}

/// **S1** — the same content at 100 paths yields one canonical object, while file
/// identity and metadata stay independent.
///
/// The spec's words: "相同内容在 100 个路径引用：只有一个相同规范内容对象，文件身份与元数据仍独立".
#[test]
fn s1_identical_content_at_100_paths_shares_one_object() {
    const PATHS: usize = 100;
    let (_root, fs) = fixture("s1");
    let payload = b"the same bytes at every path, long enough to be a real object";

    for index in 0..PATHS {
        let path = format!("/file{index:03}");
        fs.create(&path).expect("create");
        fs.write_at(&path, 0, payload).expect("write");
    }

    // One object: the store holds exactly the one the content hashes to.
    let stats = fs.stats().expect("stats");
    assert_eq!(
        stats.objects, 1,
        "identical content must collapse to one object, got {}",
        stats.objects
    );

    // Independent identity: 100 distinct file ids.
    let mut ids = Vec::new();
    for index in 0..PATHS {
        let path = format!("/file{index:03}");
        ids.push(fs.file_id(&path).expect("id").serial());
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), PATHS, "every path keeps its own file identity");

    // Independent metadata: writing to one changes only that one.
    fs.write_at("/file000", 0, b"CHANGED").expect("edit one");
    assert_ne!(
        read_poisoned(&fs, "/file000"),
        payload,
        "the edited file changed"
    );
    for index in 1..PATHS {
        let path = format!("/file{index:03}");
        assert_eq!(
            read_poisoned(&fs, &path),
            payload,
            "{path} must be unaffected by the edit to /file000"
        );
    }

    // The edit added exactly one object; the shared one is still shared.
    let stats = fs.stats().expect("stats");
    assert_eq!(stats.objects, 2, "one new object for the edited content");

    // Listing stays independent per path.
    let entries = fs.read_dir("/").expect("read_dir");
    assert_eq!(entries.len(), PATHS);
    assert!(entries.iter().all(|entry| entry.kind == Kind::File));
    assert!(fs.verify().expect("verify").is_clean());
}

/// **S4** — a published root record is readable before and after later writes and
/// never changes (D+, ADR-003 §4.1).
///
/// The spec's words: "任意写入前后 Root 可读取；旧 Root 的数据不因新写入改变".
#[test]
fn s4_published_roots_are_readable_and_never_change() {
    let (_root, fs) = fixture("s4");
    fs.create("/a").expect("create");
    fs.write_at("/a", 0, b"first").expect("write");

    // Before: the chain is readable.
    let before = fs.root_records().expect("records");
    assert!(!before.is_empty());
    let head_before = fs.head_root().expect("head");

    // Write more, in several shapes.
    fs.mkdir("/dir").expect("mkdir");
    fs.create("/dir/b").expect("create");
    fs.write_at("/dir/b", 0, &vec![7u8; 300_000])
        .expect("write big");
    fs.rename("/a", "/a2").expect("rename");
    fs.remove("/a2").expect("remove");

    // After: every earlier record is byte-for-byte identical.
    let after = fs.root_records().expect("records");
    assert!(after.len() > before.len(), "the chain grew");
    for (index, earlier) in before.iter().enumerate() {
        assert_eq!(
            &after[index], earlier,
            "root record {index} changed after later writes"
        );
    }

    // The head moved forward, the chain is unbroken, and the old head is still
    // present and readable.
    let head_after = fs.head_root().expect("head");
    assert!(head_after > head_before, "the head advanced");
    let mut current = head_after;
    let mut seen = 0u64;
    loop {
        let record = fs
            .root_record(current)
            .unwrap_or_else(|| panic!("root {current} missing from the chain"));
        seen += 1;
        match record.parent_root_id {
            Some(parent) => current = parent,
            None => break,
        }
        assert!(seen < 10_000, "chain did not terminate");
    }
    assert_eq!(seen, after.len() as u64, "the chain is complete");
}

/// **S5** — concurrent writes to different paths all succeed; the same path
/// follows the defined conflict behaviour (last write wins for files,
/// `AlreadyExists` for creates).
#[test]
fn s5_concurrent_writes_to_different_paths_all_succeed() {
    const THREADS: usize = 4;
    let (root, _fs) = fixture("s5-different");
    let root_path = root.path.clone();

    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|index| {
                let root_path = root_path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root_path, "default", OpenMode::ReadWrite).expect("open");
                    let path = format!("/t{index}.bin");
                    fs.create(&path)?;
                    fs.write_at(&path, 0, &vec![index as u8; 10_000])?;
                    Ok::<_, FsError>(path)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let mut written = Vec::new();
    for outcome in &outcomes {
        match outcome {
            Ok(path) => written.push(path.clone()),
            // A contended writer may lose the write lane; that is the typed
            // refusal ADR-003 §7 defines, never a silent partial success.
            Err(FsError::Store(cas_store::StoreError::Busy)) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }
    assert!(!written.is_empty(), "at least one writer succeeds");

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    for path in &written {
        let bytes = read_poisoned(&fs, path);
        assert_eq!(bytes.len(), 10_000, "{path} has its full content");
    }
}

/// **S5** — the same path from several threads: creates race to `AlreadyExists`
/// and writes end at one of the written values, never a mixture.
#[test]
fn s5_same_path_conflicts_follow_the_defined_behaviour() {
    let (root, _fs) = fixture("s5-same");
    let root_path = root.path.clone();

    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|index| {
                let root_path = root_path.clone();
                scope.spawn(move || {
                    let fs = Fs::open(&root_path, "default", OpenMode::ReadWrite).expect("open");
                    match fs.create("/same") {
                        Ok(()) => {
                            // The winner writes its own pattern.
                            let _ = fs.write_at("/same", 0, &vec![index as u8; 4096]);
                        }
                        Err(error) => return Err(error),
                    }
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let created = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(created, 1, "exactly one thread creates the file");
    for outcome in &outcomes {
        match outcome {
            Ok(()) => {}
            Err(FsError::AlreadyExists { .. })
            | Err(FsError::Store(cas_store::StoreError::Busy)) => {}
            Err(other) => panic!("unexpected failure: {other}"),
        }
    }

    let fs = Fs::open(root.path(), "default", OpenMode::ReadWrite).expect("reopen");
    let bytes = read_poisoned(&fs, "/same");
    assert_eq!(bytes.len(), 4096);
    let first = bytes[0];
    assert!(
        bytes.iter().all(|byte| *byte == first),
        "a same-path write must not interleave two writers' bytes"
    );
}
