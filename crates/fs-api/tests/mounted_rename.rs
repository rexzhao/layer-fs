//! Class 07: a rename through a mounted volume moves the **name**, leaving nothing behind (521).
//!
//! The defect this pins: WinFsp dispatches the rename with the **upcased source name** (`\L2-FILE` for an entry stored as `l2-file`), and the core
//! unbound the requested spelling — which removed nothing. The rename answered success and **both** names stayed bound.
//!
//! (A file test earlier reported this as "old name remains" while its own subsequent `exists()` calls contradicted the file-system log; that probe was
//! wrong about the mechanism, this one is what makes the absence of the fix fail.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_api::winfsp_backend::ApiBackend;
use fs_winfsp::abi::VolumeInfo;
use fs_winfsp::loader::{default_library, resolve_library, LoadError};
use fs_winfsp::mounted::mount_read_only;
use fs_winfsp::ops::DynOps;
use fs_workspace::Repo;

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

#[test]
fn a_rename_through_a_mounted_volume_moves_the_name() {
    let library = default_library();
    if let Err(LoadError::NotFound { path }) = resolve_library(&library) {
        eprintln!("SKIP: no winfsp at {}", path.display());
        return;
    }
    let dir = std::env::temp_dir().join(format!("fs-api-07-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp");
    let repo = Arc::new(Repo::open(dir.join("store"), true).expect("repo"));
    repo.init().expect("init");
    let workspace: &'static fs_workspace::Workspace =
        Box::leak(Box::new(repo.workspace_create("w1", "main").expect("ws")));
    let mount_point = std::env::var_os("LAYER_FS_MNT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("V:"));
    let backend: DynOps = Box::new(ApiBackend::new(workspace));
    let _ = mount_read_only(&library, &mount_point, backend, volume(), |path: &Path| {
        let root = if path.to_string_lossy().ends_with(':') {
            PathBuf::from(format!("{}\\", path.display()))
        } else {
            path.to_path_buf()
        };
        std::fs::write(root.join("l2-file"), b"x").expect("write");
        std::fs::rename(root.join("l2-file"), root.join("l2-renamed"))
            .map_err(|e| format!("rename: {e}"))?;
        if std::fs::symlink_metadata(root.join("l2-file")).is_ok() {
            return Err("the old name is still bound after a successful rename".to_string());
        }
        if std::fs::symlink_metadata(root.join("l2-renamed")).is_err() {
            return Err("the new name is not bound after a successful rename".to_string());
        }
        let names: Vec<String> = std::fs::read_dir(&root)
            .map_err(|e| format!("list: {e}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if names != vec!["l2-renamed".to_string()] {
            return Err(format!(
                "the listing is {names:?}, not exactly the new name"
            ));
        }
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&dir);
}
