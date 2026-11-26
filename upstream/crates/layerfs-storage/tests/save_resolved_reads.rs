//! #364: a Save keeps the canonical bytes it resolved, within
//! `SAVE_RESOLVED_CACHE_BYTES`. A merge reads the same pages of its three
//! trees many times (48,897 reads of 2,444 objects in range 15's slow merge);
//! each read used to walk the object's chain, decompress it and hash it again.
#[path = "support/memory_metadata.rs"]
mod metadata;
use layerfs_content::{FinalizedObject, ObjectRole};
use layerfs_storage::Storage;
use std::sync::Arc;

/// `count` incompressible objects of 32 KiB each.
fn objects(count: usize) -> Vec<FinalizedObject> {
    let mut random = 0x3640_aacc_4821_9625u64;
    (0..count)
        .map(|_| {
            let bytes = (0..4096)
                .flat_map(|_| {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    random.to_le_bytes()
                })
                .collect::<Vec<_>>();
            FinalizedObject::new(
                ObjectRole::FileState,
                layerfs_content::object::codec::encode_bytes_object(&bytes).unwrap(),
            )
            .unwrap()
        })
        .collect()
}

/// Stores `objects`, then reads them twice in one Save, in order. Returns
/// the packs each pass acquired.
fn read_twice(objects: &[FinalizedObject]) -> (usize, usize) {
    let metadata = Arc::new(metadata::MemoryMetadata::default());
    let store = Storage::new(metadata.clone()).unwrap();
    let save = store.begin_save().unwrap();
    for object in objects {
        save.accept(object.clone()).unwrap();
    }
    save.finish().unwrap();
    let reading = Storage::new(metadata.clone()).unwrap();
    let save = reading.begin_save().unwrap();
    let mut passes = [0; 2];
    for pass in &mut passes {
        metadata.acquired_pack_ids.lock().unwrap().clear();
        for object in objects {
            assert_eq!(
                save.read_objects(&[object.id()]).unwrap(),
                vec![object.canonical().to_vec()]
            );
        }
        *pass = metadata.acquired_pack_ids.lock().unwrap().len();
    }
    (passes[0], passes[1])
}

#[test]
fn a_save_reads_an_object_it_resolved_without_acquiring_its_pack_again() {
    // 3 MiB: more than the 2 MiB pack cache holds, less than the resolved
    // cache.
    let (first, again) = read_twice(&objects(96));
    assert!(first > 0, "the first pass reads the packs");
    assert_eq!(
        again, 0,
        "second pass acquired {again} packs (first {first})"
    );
}

#[test]
fn past_its_bound_the_resolved_cache_releases_the_least_recently_read() {
    // 12 MiB read twice in order: three times SAVE_RESOLVED_CACHE_BYTES. Each
    // object is released before it is read again, so the second pass reads
    // the packs again, and every read still returns the object's bytes.
    let objects = objects(384);
    assert!(
        objects.iter().map(|o| o.canonical().len()).sum::<usize>()
            > 3 * layerfs_storage::policy::SAVE_RESOLVED_CACHE_BYTES
    );
    let (first, again) = read_twice(&objects);
    assert!(first > 0 && again > 0, "first {first}, again {again}");
}
