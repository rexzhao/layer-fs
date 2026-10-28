//! The two Win32 questions path discovery needs (ADR-010 §4.4): which volume is this directory on, and what is that volume's label?
//!
//! Isolated behind one function so [`crate::discovery::discover`] can be tested with fakes and this can be tested by being called: the rule "a mount counts
//! only if the volume is ours" lives in the discovery module, and the fact "the label is `layerfs`" comes from here.

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
    let mut root = vec![0u16; 260];
    // SAFETY: the input is NUL-terminated and the buffer is sized for a volume path.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }
    let root_length = root
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(root.len());

    let mut label = vec![0u16; 260];
    let mut serial: u32 = 0;
    let mut maximum: u32 = 0;
    let mut flags: u32 = 0;
    let mut filesystem = vec![0u16; 260];
    // SAFETY: every buffer is a valid, sized array; the root is NUL-terminated by the call above.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
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
        return None;
    }
    let label_length = label
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(label.len());
    Some((
        PathBuf::from(String::from_utf16_lossy(&root[..root_length])),
        String::from_utf16_lossy(&label[..label_length]),
    ))
}

/// Not Windows: there are no drive letters to ask about, so discovery has nothing to say.
#[cfg(not(windows))]
#[must_use]
pub fn volume_root(_path: &Path) -> Option<(PathBuf, String)> {
    None
}

#[cfg(test)]
mod tests {
    /// The current directory is on **a** volume, and asking twice gives the same answer: this is the smoke test that the pair of calls is wired the right
    /// way round (a swapped buffer would answer a label nobody has).
    #[cfg(windows)]
    #[test]
    fn the_current_directory_has_a_volume_and_a_label() {
        let current = std::env::current_dir().expect("a current directory");
        let (root, label) = super::volume_root(&current).expect("a volume root");
        assert!(
            root.exists(),
            "cell volume: the root must exist: {}",
            root.display()
        );
        let (again_root, again_label) = super::volume_root(&current).expect("a volume root");
        assert_eq!(
            (root, label),
            (again_root, again_label),
            "cell volume: stable"
        );
    }
}
