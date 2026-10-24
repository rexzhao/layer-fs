//! Pack files: many object frames in one immutable file (ADR-008 §3).
//!
//! ```text
//! packs/<pack_id>.pack
//!   object frame × n      each frame is ADR-001's complete canonical frame
//!   tail: magic "WPK1" | u32 count | u64 frames_len | 32 B BLAKE3(all frame bytes)
//! ```
//!
//! Two properties drive the shape:
//!
//! - **one `fsync` per pack, not per object.** That is the whole point of packing: the
//!   P2a baseline spent one file fsync per 16 KiB CDC chunk (ADR-008 §1);
//! - **a single-object read never touches the tail.** The index records `(offset, length)`
//!   and reading seeks straight to that frame, so a pack costs the reader exactly one
//!   frame — no scan, no full-file read. The tail is for `verify` and for GC's repack.

use cas_core::ObjectId;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::durability::{rename_replace, sync_directory, sync_file};
use crate::error::{CorruptCause, StoreError, StoreResult};
use crate::layout::{temporary_name, Layout};

/// Diagnostic checkpoint (`staging-audit`): the pack must be on disk at `stage`, with `bytes`.
///
/// Prints the concrete `tmp/` and `packs/` paths (so a layout mistake cannot hide behind "the
/// code clearly only deletes tmp/"), the entry listing with each entry's own error, and the
/// evidence link's state.
#[cfg(feature = "staging-audit")]
pub(crate) fn audit(stage: &str, layout: &Layout, pack_id: &str, bytes: u64) {
    let path = layout.pack_path(pack_id);
    let Ok(meta) = std::fs::metadata(&path) else {
        let evidence = layout.packs().join(format!("{pack_id}.pack.evidence"));
        let mut listing = String::new();
        match std::fs::read_dir(layout.packs()) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => listing.push_str(&format!(
                            "{:?}(len={:?}) ",
                            entry.file_name(),
                            entry.metadata().map(|meta| meta.len())
                        )),
                        Err(entry_error) => {
                            listing.push_str(&format!("ENTRY-ERROR({entry_error}) "))
                        }
                    }
                }
            }
            Err(dir_error) => listing.push_str(&format!("READDIR-ERROR({dir_error})")),
        }
        // Path identity, both raw and canonicalised, plus a listing of the store root: if the
        // publisher and the verifier disagree about *where* `packs/` is (junction, 8.3 name,
        // relative vs absolute root), that shows up here and nowhere else.
        let mut root_listing = String::new();
        if let Ok(entries) = std::fs::read_dir(layout.root()) {
            for entry in entries.flatten() {
                root_listing.push_str(&format!("{:?} ", entry.file_name()));
            }
        }
        panic!(
            "staging-audit[{stage}]: {pack_id} missing; path={}; packs={}; packs_canon={:?};              tmp={}; root={}; root_canon={:?}; root_entries=[{root_listing}];              evidence_exists={}; entries=[{listing}]",
            path.display(),
            layout.packs().display(),
            layout.packs().canonicalize(),
            layout.temporaries().display(),
            layout.root().display(),
            layout.root().canonicalize(),
            evidence.exists(),
        );
    };
    assert_eq!(
        meta.len(),
        bytes,
        "staging-audit[{stage}]: {pack_id} is {} bytes, expected {bytes}",
        meta.len()
    );
}

/// The tail's magic, so a truncated or foreign file is never mistaken for a pack.
pub(crate) const TAIL_MAGIC: [u8; 4] = *b"WPK1";

/// Bytes of the fixed-size tail: magic, count, frames length, BLAKE3.
pub(crate) const TAIL_BYTES: u64 = 4 + 4 + 8 + 32;

/// The observable steps of writing one pack, in the order they happen.
///
/// This is the single description of the sequence: production passes
/// [`NoPackObservation`], the crash-injection tests pass an observer that aborts, and both run
/// the same code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackStep {
    /// The complete pack (frames + tail) has been written to `tmp/`, not synced.
    Staged,
    /// The staging file has been synced: its bytes are durable.
    StagedSynced,
    /// The staging file has been renamed into `packs/`.
    Renamed,
    /// `packs/` has been synced after the rename.
    DirectorySynced,
    /// The index rows (the `pack` row and its objects) have been committed.
    Committed,
}

/// Receives each [`PackStep`] as it happens.
pub trait PackObserver {
    /// Called immediately after `step` has completed.
    fn step(&mut self, step: PackStep);
}

/// The production observer: records nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPackObservation;

impl PackObserver for NoPackObservation {
    fn step(&mut self, _step: PackStep) {}
}

impl<F: FnMut(PackStep)> PackObserver for F {
    fn step(&mut self, step: PackStep) {
        self(step);
    }
}

/// One frame destined for a pack, with its index coordinates filled in by [`encode`].
pub(crate) struct PackedFrame {
    pub(crate) oid: ObjectId,
    pub(crate) role: cas_core::Role,
    pub(crate) frame: Vec<u8>,
    /// Byte offset of the frame inside the pack body (before the tail).
    pub(crate) offset: u64,
}

/// A sealed pack body plus the frame boundaries its index rows need.
pub(crate) struct EncodedPack {
    pub(crate) bytes: Vec<u8>,
}

/// Seals `frames` into a pack body: the frames verbatim, then the tail.
pub(crate) fn encode(frames: &mut [PackedFrame]) -> EncodedPack {
    let mut bytes = Vec::with_capacity(
        frames.iter().map(|frame| frame.frame.len()).sum::<usize>() + TAIL_BYTES as usize,
    );
    for frame in frames.iter_mut() {
        frame.offset = bytes.len() as u64;
        bytes.extend_from_slice(&frame.frame);
    }
    let frames_len = bytes.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    bytes.extend_from_slice(&TAIL_MAGIC);
    bytes.extend_from_slice(
        &u32::try_from(frames.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&frames_len.to_le_bytes());
    bytes.extend_from_slice(hasher.finalize().as_bytes());
    EncodedPack { bytes }
}

/// Writes a sealed pack: `tmp/` → fsync → `packs/` → fsync directory.
///
/// The index commit that publishes it is the caller's business: this function only makes
/// the bytes durable, which is why a crash between it and the commit leaves an unreferenced
/// pack file rather than a dangling index row (ADR-008 §5.1, rows 2 and 3).
pub(crate) fn write(
    layout: &Layout,
    pack_id: &str,
    bytes: &[u8],
    observer: &mut dyn PackObserver,
) -> StoreResult<crate::durability::Pin> {
    // The staging file can legitimately disappear underneath us: `tmp/` is a *shared*
    // directory and every open of the Store sweeps it (ADR-002 §7). A second process opening
    // the same Store while this pack is being staged therefore deletes our staging file
    // between `create` and `rename`. ADR-002 §7.0 decides the shape of the answer, and this is
    // its (d): retry the staging segment under three conditions —
    //
    // 1. each attempt uses a **new** staging name and rewrites the pack from the start (never
    //    appends to a file another process may have truncated);
    // 2. the retry wraps **only** staging + rename: no database transaction, nothing on the
    //    read path, is ever replayed;
    // 3. only `NotFound`/`PermissionDenied` are retried, and when the attempts are exhausted
    //    the **original** error is reported, not a later symptom.
    //
    // P2b-6 replaces this with "ordinary opens do not sweep; the offline GC reclaims tmp/".
    const ATTEMPTS: u32 = 20;
    let mut first_failure: Option<StoreError> = None;
    for attempt in 1..=ATTEMPTS {
        #[cfg(feature = "sync-probe")]
        crate::probe::begin_attempt();
        let staged = layout.temporaries().join(temporary_name());
        let staged_result = (|| -> StoreResult<()> {
            {
                let mut file = File::create(&staged)?;
                file.write_all(bytes)?;
                observer.step(PackStep::Staged);
                #[cfg(feature = "sync-probe")]
                crate::probe::record(crate::probe::Event::PackWrite);
                sync_file(&file)?;
                observer.step(PackStep::StagedSynced);
            }
            // The staging file is closed here, which is the window a concurrent opener's
            // `tmp/` sweep actually interrupts (on Windows a sweep while the handle is open
            // fails instead). The hook runs at that exact point.
            #[cfg(feature = "crash-points")]
            crate::testing::run_staging_sweep(&layout.temporaries());
            rename_replace(&staged, &layout.pack_path(pack_id)).map_err(StoreError::Io)
        })();
        match staged_result {
            Ok(()) => break,
            Err(StoreError::Io(error))
                if attempt < ATTEMPTS
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                if first_failure.is_none() {
                    first_failure = Some(StoreError::Io(error));
                }
                #[cfg(feature = "crash-points")]
                crate::testing::note_staging_retry();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(other) => {
                // A different failure, or the last attempt: report the first error we saw if
                // this is the same *kind* of transient failure, otherwise this one.
                return Err(match (&first_failure, &other) {
                    (Some(first), StoreError::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                        ) =>
                    {
                        clone_store_error(first)
                    }
                    _ => other,
                });
            }
        }
    }
    #[cfg(feature = "staging-audit")]
    {
        audit(
            "after-rename-pre-evidence",
            layout,
            pack_id,
            bytes.len() as u64,
        );
        // Evidence link: if the file later "disappears", this tells us whether the *name* was
        // removed (link survives) or the whole directory entry went with it.
        let evidence = layout.packs().join(format!("{pack_id}.pack.evidence"));
        match std::fs::hard_link(layout.pack_path(pack_id), &evidence) {
            Ok(()) => eprintln!("staging-audit: evidence link OK for {pack_id}"),
            Err(error) => {
                eprintln!("staging-audit: evidence link FAILED for {pack_id}: {error}")
            }
        }
        audit("after-evidence", layout, pack_id, bytes.len() as u64);
    }
    observer.step(PackStep::Renamed);
    #[cfg(feature = "staging-audit")]
    audit("after-rename", layout, pack_id, bytes.len() as u64);
    sync_directory(&layout.packs())?;
    observer.step(PackStep::DirectorySynced);
    #[cfg(feature = "staging-audit")]
    audit("after-sync-dir", layout, pack_id, bytes.len() as u64);
    // ADR-008 §5.2: deny deletion from here until the index row exists. A failure to hold the
    // handle means the file is already gone, which is exactly `MissingPack`; nothing has been
    // committed at this point, so the batch can be abandoned cleanly (no retry: a visibility
    // re-check would not close the window, and retrying would turn data loss into latency).
    crate::durability::Pin::hold(&layout.pack_path(pack_id)).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::MissingPack {
            pack_id: pack_id.to_owned(),
        },
        _ => StoreError::Io(error),
    })
}

/// Reads exactly one frame at `(offset, length)`.
///
/// Reads `length` bytes and nothing else: no tail read, no scan (ADR-008 §3). The caller
/// decodes and verifies the frame as usual, so a wrong offset is caught by identity, not by
/// trusting the index.
pub(crate) fn read_frame(
    layout: &Layout,
    pack_id: &str,
    offset: u64,
    length: u64,
) -> StoreResult<Vec<u8>> {
    let path = layout.pack_path(pack_id);
    #[cfg(not(feature = "staging-audit"))]
    let mut file = open_pack(&path, pack_id)?;
    #[cfg(feature = "staging-audit")]
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            // One short retry, then the full picture: the exact path, every directory entry
            // with *its own* error (Windows can fail a per-entry read while the directory is
            // being modified — `.flatten()` would silently hide that), and the canonical root.
            let mut line = format!(
                "staging-audit: pack frame read failed for {pack_id}: {error}; path={}; root={:?}",
                path.display(),
                layout.root().canonicalize()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            match File::open(&path) {
                Ok(file) => {
                    eprintln!("{line}; RETRY SUCCEEDED (visibility delay)");
                    file
                }
                Err(second) => {
                    line.push_str(&format!("; retry failed: {second}; entries="));
                    match std::fs::read_dir(layout.packs()) {
                        Ok(entries) => {
                            let mut listed = Vec::new();
                            for entry in entries {
                                match entry {
                                    Ok(entry) => listed.push(format!(
                                        "{:?}(len={:?})",
                                        entry.file_name(),
                                        entry.metadata().map(|meta| meta.len())
                                    )),
                                    Err(entry_error) => {
                                        listed.push(format!("ENTRY-ERROR({entry_error})"))
                                    }
                                }
                            }
                            line.push_str(&format!("{listed:?}"));
                        }
                        Err(dir_error) => line.push_str(&format!("READDIR-ERROR({dir_error})")),
                    }
                    panic!("{line}");
                }
            }
        }
    };

    let mut bytes = vec![0u8; usize::try_from(length).unwrap_or(0)];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "pack {pack_id}: frame at {offset}+{length} runs past the end of {}",
                    path.display()
                ),
            )),
            _ => StoreError::Io(error),
        })?;
    #[cfg(feature = "crash-points")]
    crate::testing::note_pack_read(length);
    Ok(bytes)
}

/// Opens a pack, reporting a missing file as [`StoreError::MissingPack`].
///
/// A published pack that cannot be opened is storage damage (see [`StoreError::MissingPack`]),
/// never `Io`: the index said it was there, so the caller needs the damage, not an errno.
pub(crate) fn open_pack(path: &std::path::Path, pack_id: &str) -> StoreResult<File> {
    File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::MissingPack {
            pack_id: pack_id.to_owned(),
        },
        _ => StoreError::Io(error),
    })
}

/// What checking one pack found.
///
/// `count` is the tail's own record of how many frames the body holds. It is used as a
/// **necessary** condition only: concurrent publishers leave dead frames behind (ADR-008 §5),
/// so the physical frame count may exceed the index's row count, but it can never be smaller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackTail {
    /// Frames the tail claims.
    pub(crate) count: u32,
    /// Body length the tail claims.
    pub(crate) frames_len: u64,
}

/// Verifies a pack's tail and that the file is exactly one pack (ADR-008 §7).
///
/// Checks the magic, the recorded body length against the file length, and the BLAKE3 of
/// the body. A file shorter than the tail, a foreign file, or a corrupted body all report
/// [`CorruptCause`] with the pack named, so `verify` can say which pack is broken.
pub(crate) fn verify(layout: &Layout, pack_id: &str) -> StoreResult<PackTail> {
    let path = layout.pack_path(pack_id);
    let bytes = std::fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::MissingPack {
            pack_id: pack_id.to_owned(),
        },
        _ => StoreError::Io(error),
    })?;
    let length = bytes.len() as u64;
    if length < TAIL_BYTES {
        return Err(corrupt_pack(pack_id, "file is shorter than a pack tail"));
    }
    let at = (length - TAIL_BYTES) as usize;
    if bytes[at..at + 4] != TAIL_MAGIC {
        return Err(corrupt_pack(pack_id, "tail magic is not WPK1"));
    }
    let count = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap_or([0; 4]));
    let frames_len = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap_or([0; 8]));
    if frames_len + TAIL_BYTES != length {
        return Err(corrupt_pack(
            pack_id,
            &format!("tail declares {frames_len} frame bytes but the file holds {length}"),
        ));
    }
    let recorded = &bytes[at + 16..at + 48];
    let actual = blake3::hash(&bytes[..at]);
    if recorded != actual.as_bytes() {
        return Err(corrupt_pack(
            pack_id,
            "tail BLAKE3 does not match the frames",
        ));
    }
    Ok(PackTail { count, frames_len })
}

/// Rebuilds the original error for the caller (the retry needs it after the fact).
fn clone_store_error(error: &StoreError) -> StoreError {
    match error {
        StoreError::Io(inner) => {
            StoreError::Io(std::io::Error::new(inner.kind(), inner.to_string()))
        }
        other => StoreError::Io(std::io::Error::other(other.to_string())),
    }
}

fn corrupt_pack(pack_id: &str, what: &str) -> StoreError {
    StoreError::CorruptPack {
        pack_id: pack_id.to_owned(),
        cause: CorruptCause::PackTail(what.to_owned()),
    }
}

/// A pack id: 128 random-looking bits in hex, from the clock, the process and a counter.
///
/// The name carries no content identity (ADR-008 §3), so this needs uniqueness within a
/// store, not unpredictability: a collision would mean two writers picked the same name and
/// the second rename would replace the first, which is why the counter is process-wide.
pub(crate) fn new_pack_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&sequence.to_le_bytes());
    let digest = hasher.finalize();
    #[cfg(feature = "staging-audit")]
    eprintln!(
        "staging-audit: pack_id pid={} nanos={} counter={} id={}",
        std::process::id(),
        nanos,
        sequence,
        digest.as_bytes()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    digest.as_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
