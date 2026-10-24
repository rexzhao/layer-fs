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

/// The tail's magic, so a truncated or foreign file is never mistaken for a pack.
pub(crate) const TAIL_MAGIC: [u8; 4] = *b"WPK1";

/// Bytes of the fixed-size tail: magic, count, frames length, BLAKE3.
pub(crate) const TAIL_BYTES: u64 = 4 + 4 + 8 + 32;

/// The observable steps of writing one pack, in the order they happen.
///
/// Like [`PublishStep`](crate::PublishStep), this is the single description of the
/// sequence: production passes [`NoPackObservation`], the crash-injection tests pass an
/// observer that aborts, and both run the same code.
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
) -> StoreResult<()> {
    // The staging file can legitimately disappear underneath us: `tmp/` is a *shared*
    // directory and every open of the Store sweeps it (ADR-002 §7). A second process opening
    // the same Store while this pack is being staged therefore deletes our staging file
    // between `create` and `rename`, which is the same race the retired loose path handled by
    // adopting the target. Nothing has been published at that point, so staging again is not
    // a replay of an effect — it is the same, still-unpublished write. Bounded, because a
    // store whose `tmp/` is being cleared in a loop is a different problem.
    const ATTEMPTS: u32 = 20;
    let mut attempt = 0;
    loop {
        attempt += 1;
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
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(other) => return Err(other),
        }
    }
    observer.step(PackStep::Renamed);
    sync_directory(&layout.packs())?;
    observer.step(PackStep::DirectorySynced);
    Ok(())
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
    let mut file = File::open(&path)?;
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

/// What checking one pack found.
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
    let bytes = std::fs::read(&path)?;
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
    digest.as_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
