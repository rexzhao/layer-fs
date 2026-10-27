//! The **same** mount shape over the mock backend: write, list the root, create a subdirectory, write inside it, list it.
//!
//! Paired with `mounted_real_backend.rs`, which runs the identical sequence over `ApiBackend`: two readings of the same question, so "the mount works"
//! is not one assumption. Both return `Err` on the first failure — a probe that answers `Ok(())` unconditionally measures nothing (434/496).

use std::path::{Path, PathBuf};

use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::loader::{default_library, resolve_library, LoadError};
use fs_winfsp::mock::MockBackend;
use fs_winfsp::mounted::mount_read_only;
use fs_winfsp::ops::DynOps;

fn volume() -> VolumeInfo {
    VolumeInfo {
        total_size: 1 << 20,
        free_size: 1 << 19,
        volume_label_length: 4,
        volume_label: {
            let mut label = [0u16; 32];
            label[..4].copy_from_slice(&[b'w' as u16, b'r' as u16, b'i' as u16, b't' as u16]);
            label
        },
    }
}

fn mount_point() -> PathBuf {
    std::env::var_os("LAYER_FS_MNT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("V:"))
}

fn root_of(mount_point: &Path) -> PathBuf {
    if mount_point.to_string_lossy().ends_with(':') {
        PathBuf::from(format!("{}\\", mount_point.display()))
    } else {
        mount_point.to_path_buf()
    }
}

#[test]
fn windows_can_write_and_list_through_a_mock_mount() {
    let library = default_library();
    if let Err(LoadError::NotFound { path }) = resolve_library(&library) {
        eprintln!("SKIP: no winfsp at {}", path.display());
        return;
    }
    let mount_point = mount_point();
    let backend: DynOps = Box::new(MockBackend::new());

    let report = mount_read_only(&library, &mount_point, backend, volume(), |path: &Path| {
        let root = root_of(path);
        let file = root.join("written.txt");
        // 1. Write. This is what `mounted_write_chain` has always done successfully on the mock, so it is the step that must come first.
        std::fs::write(&file, b"hello layer-fs").map_err(|error| format!("write: {error}"))?;
        let read_back = std::fs::read(&file).map_err(|error| format!("read back: {error}"))?;
        if read_back != b"hello layer-fs" {
            return Err(format!(
                "read back: {:?}",
                String::from_utf8_lossy(&read_back)
            ));
        }
        // 2. List the root: the entry we just wrote must be visible.
        let names = std::fs::read_dir(&root)
            .map_err(|error| format!("read_dir(root): {error}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if !names.iter().any(|name| name == "written.txt") {
            return Err(format!(
                "read_dir(root) did not list written.txt: {names:?}"
            ));
        }
        // 3. A subdirectory: create it, write inside it, list it.
        let dir = root.join("sub");
        std::fs::create_dir(&dir).map_err(|error| format!("create_dir: {error}"))?;
        std::fs::write(dir.join("a.txt"), b"a")
            .map_err(|error| format!("write in sub: {error}"))?;
        let sub_names = std::fs::read_dir(&dir)
            .map_err(|error| format!("read_dir(sub): {error}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if !sub_names.iter().any(|name| name == "a.txt") {
            return Err(format!("read_dir(sub) did not list a.txt: {sub_names:?}"));
        }
        Ok(())
    })
    .expect("the mount path is available");
    println!(
        "report create={:#010X} mount={:#010X} start={:#010X} unmount={:#010X} dispatcher={:#010X}",
        report.create, report.mount, report.start, report.unmount, report.dispatcher_result
    );
}
