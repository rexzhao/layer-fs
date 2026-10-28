//! A mount over the **real** backend, with a probe that is allowed to fail.
//!
//! The previous attempt at this printed its findings and returned `Ok(())`; `mount_read_only` uses the probe's answer as its "does this look like a
//! mount?" test, so that probe measured nothing and its readings meant nothing (434/496). Everything below returns `Err` on the first sign of
//! trouble, so a mount that does not work fails the test instead of producing a tidy list of failures.

mod common;

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

/// A workspace on disk, kept alive for the test's duration.
struct Fixture {
    dir: PathBuf,
    _repo: Arc<Repo>,
    workspace: &'static fs_workspace::Workspace,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "fs-api-mounted-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Arc::new(Repo::open(dir.join("store"), true).expect("repo"));
        repo.init().expect("init");
        // `DynOps` carries no lifetime, so the backend must outlive the mount call; a probe may leak, and this file is a probe.
        let workspace: &'static fs_workspace::Workspace = Box::leak(Box::new(
            repo.workspace_create("w1", "main").expect("workspace"),
        ));
        Self {
            dir,
            _repo: repo,
            workspace,
        }
    }
}

#[test]
fn windows_can_write_and_list_through_a_real_backend_mount() {
    common::strict_gate();
    let library = default_library();
    if let Err(LoadError::NotFound { path }) = resolve_library(&library) {
        eprintln!("SKIP: no winfsp at {}", path.display());
        return;
    }
    let fixture = Fixture::new("root");
    let mount_point = mount_point();
    let backend: DynOps = Box::new(ApiBackend::new(fixture.workspace));

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
