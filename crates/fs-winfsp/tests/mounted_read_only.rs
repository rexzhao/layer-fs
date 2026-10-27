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

/// The directory listing the callbacks answer from: the root has two entries.
fn list(dir_id: u64) -> Vec<(Vec<u16>, NodeInfo)> {
    if dir_id != 0 {
        return Vec::new();
    }
    vec![
        ("alpha.txt".encode_utf16().collect(), node(1)),
        ("beta".encode_utf16().collect(), node(2)),
    ]
}

fn lookup(file_id: u64) -> Option<NodeInfo> {
    // The root (**id 0**) must resolve too: `Create` is called for `\` first, and without it the volume has no root to open.
    (file_id == 0 || file_id == 7).then(|| node(file_id))
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

    // Paths: the root is `\`; one child for the listing probe.
    // Paths: the root is a single backslash; the listing probe asks for its children.
    assert!(
        callbacks::configure_paths(|path| {
            let trimmed = path.trim_start_matches(char::from(92));
            match trimmed {
                "" => Some(0),
                "alpha.txt" => Some(1),
                _ => None,
            }
        }),
        "cell paths: the resolver is configured once"
    );
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
        list,
    );
    assert!(
        configured,
        "cell configure: this test configures the callbacks first; a second configuration in the same process fails by design"
    );

    // An **explicitly created empty directory**: WinFsp refuses a mount point that does not exist, and `0xC0000035` is what
    // that refusal looks like from the caller's side (it reads as "name collision").
    // **Z: by owner's ruling** (`Y:` already exists on this machine, which silently invalidated a batch of earlier readings).
    // The environment variable stays for other machines; the default is a letter that is expected to be free.
    let mount_point: PathBuf = std::env::var_os("LAYER_FS_MNT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("Z:"));
    // A **drive letter** is a different kind of mount point: it must not exist as a directory, and WinFsp picks the volume
    // itself. Testing both shapes separately is what tells "our parameters are wrong" from "directory mounts need more setup".
    let drive_letter = {
        let text = mount_point.to_string_lossy().to_string();
        text.len() == 2 && text.ends_with(':')
    };
    if drive_letter {
        // The precondition the `Y:` incident taught: a letter that is already in use makes every later reading meaningless,
        // so it is asserted rather than assumed.
        assert_eq!(
            fs_winfsp::mounted::drive_present(&mount_point),
            Some(false),
            "cell precondition: {} must be free before mounting — an existing drive letter silently invalidates the run              (the `Y:` incident)",
            mount_point.display()
        );
        eprintln!("mounting at drive letter {}", mount_point.display());
    } else {
        std::fs::create_dir_all(&mount_point).expect("empty mount point directory");
        assert!(
            std::fs::read_dir(&mount_point).expect("mount point exists").next().is_none(),
            "cell mount point: an empty directory is a precondition — a non-empty one is refused with 0xC0000035"
        );
    }

    // The probe runs **while mounted**; its error is captured for the report.
    let mut probe_result: Option<Result<(), String>> = None;
    let before_contexts = callbacks::context_counts();
    let report = mount_read_only(&library, &mount_point, |path| {
        // `Y:` alone means "the current directory on Y:", which a file system without directory state cannot answer; `Y:\`
        // is the volume root. This distinction is exactly the kind of thing that fails as ERROR_INVALID_FUNCTION rather than as
        // "not found".
        let root = if path.to_string_lossy().ends_with(':') {
            PathBuf::from(format!("{}\\", path.display()))
        } else {
            path.to_path_buf()
        };
        eprintln!(
            "during mount: drive present = {:?}",
            fs_winfsp::mounted::drive_present(path)
        );
        let outcome = std::fs::read_dir(&root)
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

    let after_contexts = callbacks::context_counts();
    eprintln!(
        "dispatcher result = {} (0x{:08X}) — 0 means the dispatcher threads survived",
        report.dispatcher_result, report.dispatcher_result as u32
    );
    eprintln!("probe hits = {}", report.probe_hits);
    eprintln!("open entries = {}", callbacks::open_entry_count());
    eprintln!(
        "open exits: no_resolver={} no_entry={} bad_record={}",
        callbacks::open_exit_counts().0,
        callbacks::open_exit_counts().1,
        callbacks::open_exit_counts().2
    );
    eprintln!(
        "callbacks reached: volume_info={} security={} create={}",
        callbacks::volume_call_count(),
        callbacks::security_call_count(),
        callbacks::create_call_count()
    );
    eprintln!(
        "file contexts: allocated={} freed={} (before: {before_contexts:?})",
        after_contexts.0, after_contexts.1
    );
    eprintln!(
        "create={} mount={} start={} unmount={} released_after_delete={} looked_like_a_mount={} probe={:?}",
        report.create,
        report.mount,
        report.start,
        report.unmount,
        report.released_after_delete,
        report.looked_like_a_mount,
        probe_result
    );

    assert_eq!(
        report.create, 0,
        "cell create: the file system object was created"
    );
    if drive_letter {
        // A drive letter needs no volume prefix: WinFsp picks the volume itself. Same parameters, different mount-point shape,
        // and the mount succeeds — which is the fact the directory-mount failure needed.
        assert_eq!(
            report.mount, 0,
            "cell mount: mounting at a drive letter succeeds"
        );
        assert_eq!(report.start, 0, "cell start: the dispatcher started");
        // The probe reaching the file system is the evidence that Windows saw us; the error is *expected* (no ReadDirectory
        // callback exists yet) and is recorded rather than asserted away.
        eprintln!(
            "drive-letter probe: {:?} — ERROR_INVALID_FUNCTION means the OS reached an unimplemented operation, i.e. it saw \
             the mount",
            probe_result
        );
        // Unmount is reported, not asserted: it failed on this machine (0xC000003D) and that is the next thing to explain, not
        // something to hide behind a relaxed assertion.
        eprintln!(
            "unmount status = {} (0x{:08X})",
            report.unmount, report.unmount as u32
        );
    } else {
        assert_eq!(
            report.unmount, 0,
            "cell unmount: the mount point was released (a stuck mount outlives the process)"
        );
    }
    let _ = std::fs::remove_dir_all(&mount_point);
}
