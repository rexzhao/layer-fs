//! Verifies loose sparse-file overlay payload storage: SQLite stays tiny,
//! deleted files immediately free disk space, and multi-GiB sparse offsets
//! allocate minimal physical blocks on NTFS.
mod payload_support;
use layerfs_overlay::*;
use payload_support::*;

fn maintain(db: &Overlay) {
    let mut cursor = MaintenanceCursor::default();
    for _ in 0..4000 {
        let Some(step) = db.maintain(cursor).unwrap() else {
            return;
        };
        cursor = step.cursor;
    }
    panic!("maintenance deadline");
}

#[test]
fn large_payload_stays_out_of_sqlite_and_reclaims_loose_file_on_unlink() {
    let temp = Temp::new();
    let db_path = temp.0.join("overlay.sqlite");
    let db = Overlay::create(&db_path, ProfileConfig::default()).unwrap();
    db.open_workspace([77; 32]).unwrap();
    let source = db.acquire_base_source(1).unwrap();

    // Write 16 MiB into inode 9.
    let mut file = File::new(&db, source, 9, Vec::new());
    let total = 16 * 1024 * 1024usize;
    let chunk = pattern(WRITE_WINDOW, 19);
    let mut off = 0u64;
    while (off as usize) < total {
        file.write(off, &chunk);
        off += chunk.len() as u64;
    }

    let sqlite_bytes = std::fs::metadata(&db_path).unwrap().len();
    assert!(
        sqlite_bytes < 512 * 1024,
        "SQLite metadata file should remain small (<512 KiB) for 16 MiB payload, got {sqlite_bytes} bytes"
    );

    let blob_path = temp
        .0
        .join("overlay.sqlite.blobs")
        .join("09")
        .join("i9_g1.dat");
    assert_eq!(
        std::fs::metadata(&blob_path).unwrap().len(),
        total as u64,
        "loose payload file should hold the 16 MiB payload"
    );

    // Unlink and run maintenance: loose file must be unlinked immediately, returning 16 MiB to NTFS.
    let mut inode = file.inode(total as u64);
    inode.nlink = 0;
    let pub_id = db
        .apply(
            source,
            &Changes {
                inodes: vec![inode],
                ..Changes::default()
            },
        )
        .unwrap();
    db.reply_attempted(pub_id).unwrap();
    maintain(&db);
    assert!(
        !blob_path.exists(),
        "loose payload file must be deleted from disk once reclaimed"
    );

    db.release_base_source(source).unwrap();
    db.close().unwrap();
    db.close_database().unwrap();
    assert!(
        !temp.0.join("overlay.sqlite.blobs").exists(),
        "sidecar .blobs directory must be removed on close_database"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn sparse_write_at_five_gib_allocates_minimal_ntfs_blocks() {
    let temp = Temp::new();
    let db_path = temp.0.join("sparse.sqlite");
    let db = Overlay::create(&db_path, ProfileConfig::default()).unwrap();
    db.open_workspace([88; 32]).unwrap();
    let source = db.acquire_base_source(1).unwrap();

    let offset = 5u64 * 1024 * 1024 * 1024;
    let payload = pattern(4096, 5);
    let mut file = File::new(&db, source, 11, Vec::new());
    let pub_id = db
        .apply(
            source,
            &Changes {
                inodes: vec![file.inode(offset + 4096)],
                write: Some(PayloadWrite {
                    serial: 11,
                    offset,
                    data: payload.clone().into(),
                }),
                ..Changes::default()
            },
        )
        .unwrap();
    db.reply_attempted(pub_id).unwrap();
    let _ = &mut file;

    let read = db.source_read(source, 11, offset, 4096).unwrap().unwrap();
    assert_eq!(read.data, payload);

    let blob_path = temp
        .0
        .join("sparse.sqlite.blobs")
        .join("0b")
        .join("i11_g1.dat");
    let ident = layerfs_win32_host::path_identity(&blob_path).unwrap();
    assert!(ident.logical_bytes >= offset + 4096);
    assert!(
        ident.allocated_bytes < 1024 * 1024,
        "sparse 5 GiB write should allocate <1 MiB on NTFS, got {} bytes",
        ident.allocated_bytes
    );

    db.release_base_source(source).unwrap();
    db.close().unwrap();
    db.close_database().unwrap();
}
