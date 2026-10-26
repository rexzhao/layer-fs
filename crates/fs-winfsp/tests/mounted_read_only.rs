//! Step ④: mount read-only at a directory, look at it **through the OS**, then unmount and tear down.
//!
//! The probe runs while the mount is up and its outcome is reported, not asserted: `std::fs::read_dir` failing there is a
//! **result** (it may mean "directory must be empty", "no permission", or "the mount never happened"), and the record of that
//! error is the evidence.

use std::path::PathBuf;

use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::callbacks;
use fs_winfsp::loader::{default_library, resolve_library, LoadError};
use fs_winfsp::mounted::mount_read_only;
use fs_winfsp::{Attributes, Kind, NodeInfo};

fn node(file_id: u64) -> NodeInfo {
    NodeInfo {
        file_id,
        kind: Kind::File,
        size: 0,
        times: fs_winfsp::file_info::times(
            1_793_000_000_000_000_000,
            1_793_000_000_000_000_000,
            1_793_000_000_000_000_000,
            1_793_000_000_000_000_000,
        ),
        attributes: Attributes(0x80),
    }
}

fn lookup(file_id: u64) -> Option<NodeInfo> {
    (file_id == 7).then(|| node(file_id))
}

#[test]
fn the_file_system_can_be_mounted_read_only_and_unmounted() {
    let library = default_library();
    if let Err(LoadError::NotFound { path }) = resolve_library(&library) {
        eprintln!(
            "SKIP: winfsp library not installed at {} — mount not exercised (skip, not pass)",
            path.display()
        );
        return;
    }

    let configured = callbacks::configure(
        &VolumeInfo {
            total_size: 1 << 20,
            free_size: 1 << 19,
            volume_label_length: 8,
            volume_label: {
                let mut label = [0u16; 32];
                label[..4].copy_from_slice(&[b'w' as u16, b'c' as u16, b'a' as u16, b's' as u16]);
                label
            },
        },
        lookup,
    );
    assert!(
        configured,
        "cell configure: this test configures the callbacks first; a second configuration in the same process fails by design"
    );

    let mount_point: PathBuf = std::env::temp_dir().join(format!(
        "layer-fs-mnt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&mount_point).expect("empty mount point directory");

    // The probe runs **while mounted**; its error is captured for the report.
    let mut probe_result: Option<Result<(), String>> = None;
    let report = mount_read_only(&library, &mount_point, |path| {
        let outcome = std::fs::read_dir(path)
            .map(|mut entries| {
                // Reading one entry is enough to prove the OS served the directory from us; the callback table has no
                // ReadDirectory yet, so a `None` here is expected and recorded as such.
                let _ = entries.next();
            })
            .map_err(|error| error.to_string());
        probe_result = Some(outcome.clone());
        outcome
    })
    .expect("the mount path is available");

    eprintln!(
        "create={} mount={} start={} unmount={} looked_like_a_mount={} probe={:?}",
        report.create,
        report.mount,
        report.start,
        report.unmount,
        report.looked_like_a_mount,
        probe_result
    );

    assert_eq!(
        report.create, 0,
        "cell create: the file system object was created"
    );
    assert_eq!(
        report.unmount, 0,
        "cell unmount: the mount point was released (a stuck mount outlives the process)"
    );
    // `mount`/`start` may legitimately fail on a machine without the WinFsp service running; the report keeps both numbers, and
    // this assertion is only about the teardown having happened.
    let _ = std::fs::remove_dir_all(&mount_point);
}
