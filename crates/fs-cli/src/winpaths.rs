//! The two Win32 questions path discovery needs (ADR-010 §4.4): which volume is this directory on, and what is that volume's label?
//!
//! Isolated behind one function so [`crate::discovery::discover`] can be tested with fakes and this can be tested by being called: the rule "a mount counts
//! only if the volume is ours" lives in the discovery module, and the fact "the label is `layerfs`" comes from here.
//!
//! # The order of the two calls is not obvious, and it was measured
//!
//! `GetVolumePathNameW("J:\\")` **fails** on a volume served by WinFsp with `ERROR_UNRECOGNIZED_VOLUME (1005)`: Windows will happily answer questions *about*
//! the volume (`vol J:`, and `GetVolumeInformationW("J:\\")`) but does not accept it as a mount point for path decomposition. So the label is asked **first,
//! of the path itself**, and the root is derived only for reporting — that is the difference between "discovery never works on our own volumes" and "it works".

use std::path::{Path, PathBuf};

/// The root and label of the volume containing `path`, or `None` when Windows cannot tell us.
#[cfg(windows)]
#[must_use]
pub fn volume_root(path: &Path) -> Option<(PathBuf, String)> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // **The root first, by two routes** (measured): `GetVolumePathNameW` works for ordinary volumes and fails with `ERROR_UNRECOGNIZED_VOLUME (1005)` on a
    // volume served by WinFsp — there the drive prefix *is* the root. `GetVolumeInformationW` then needs a **root**: handing it a directory path fails with
    // `ERROR_INVALID_NAME (123)`, which is why the label is asked last, of whichever root we managed to name.
    let mut root_buffer = vec![0u16; 260];
    // SAFETY: the input is NUL-terminated and the buffer is sized for a volume path.
    let decomposed = unsafe {
        GetVolumePathNameW(
            wide.as_ptr(),
            root_buffer.as_mut_ptr(),
            root_buffer.len() as u32,
        )
    } != 0;
    let root = if decomposed {
        let length = root_buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(root_buffer.len());
        PathBuf::from(String::from_utf16_lossy(&root_buffer[..length]))
    } else {
        drive_prefix(path)?
    };

    let root_wide: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut label = vec![0u16; 260];
    let mut serial: u32 = 0;
    let mut maximum: u32 = 0;
    let mut flags: u32 = 0;
    let mut filesystem = vec![0u16; 260];
    // SAFETY: every buffer is a valid, sized array and `root_wide` is NUL-terminated.
    let ok = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut maximum,
            &mut flags,
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if ok == 0 {
        // Only when asked: a path that is not on a volume we can name is a normal answer ("this is not a mount of ours"), not a diagnostic to print at every
        // command run from a plain directory. `LAYERFS_DEBUG_DISCOVERY=1` is the switch that makes it talk.
        if std::env::var_os("LAYERFS_DEBUG_DISCOVERY").is_some() {
            eprintln!(
                "winpaths: GetVolumeInformationW({}) failed, last error {}",
                root.display(),
                unsafe { windows_sys::Win32::Foundation::GetLastError() }
            );
        }
        return None;
    }
    let label_length = label
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(label.len());

    Some((root, String::from_utf16_lossy(&label[..label_length])))
}
#[cfg(windows)]
fn drive_prefix(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    let bytes: Vec<char> = text.chars().take(2).collect();
    if bytes.len() == 2 && bytes[1] == ':' && bytes[0].is_ascii_alphabetic() {
        Some(PathBuf::from(format!("{}:\\", bytes[0])))
    } else {
        None
    }
}

/// Not Windows: there are no drive letters to ask about, so discovery has nothing to say.
#[cfg(not(windows))]
#[must_use]
pub fn volume_root(_path: &Path) -> Option<(PathBuf, String)> {
    None
}

#[cfg(test)]
mod tests {
    /// The current directory is on **a** volume with a label, and asking twice gives the same answer — the smoke test that the pair of calls is wired the
    /// right way round (a swapped buffer would answer a label nobody has).
    #[cfg(windows)]
    #[test]
    fn the_current_directory_has_a_volume_and_a_label() {
        let current = std::env::current_dir().expect("a current directory");
        let first = super::volume_root(&current).expect("a volume root");
        assert!(
            first.0.exists() || first.0.to_string_lossy().ends_with('\\'),
            "cell volume: the root must be a real path: {}",
            first.0.display()
        );
        // An unlabelled volume answers an **empty** label; that is a real answer and discovery simply will not match it.
        let _ = &first.1;
        assert_eq!(
            first,
            super::volume_root(&current).expect("a volume root"),
            "cell volume: stable"
        );
    }

    /// `drive_prefix` is the fallback that keeps discovery working on volumes Windows will not decompose.
    #[cfg(windows)]
    #[test]
    fn a_drive_prefix_is_derived_from_the_path() {
        use std::path::Path;
        assert_eq!(
            super::drive_prefix(Path::new("J:\\sub\\file")).map(|root| root.display().to_string()),
            Some("J:\\".to_owned())
        );
        assert!(super::drive_prefix(Path::new("\\\\server\\share")).is_none());
    }
}
