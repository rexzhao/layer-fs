//! The CLI's **vocabulary**, as a library so the criteria can reference the same constants the binary prints (ADR-010 §8.2).
//!
//! A test that restates a message in its own words stops testing the message the moment one of the two changes; asserting the constant makes the criterion and
//! the binary fail together, which is the point.

pub mod discovery;
pub mod pipe;
#[cfg(windows)]
pub mod pipe_win;
pub mod winpaths;

/// Printed when a command needs a workspace and none was given.
///
/// ADR-010 §8.2 fixes this text **byte for byte** (A0 asserts it). The second half — `, or cd into a mount point` — arrived with path discovery: before that, a
/// way out that did not work would have been a promise the CLI could not keep.
pub const NO_WORKSPACE: &str =
    "no workspace specified; use --workspace <name>, or cd into a mount point";

/// The commands that were **removed** in ADR-010 §5: file operations happen through the mounted file system, not through the CLI.
pub const FILE_COMMANDS: &[&str] = &["put", "get", "ls", "stat", "mkdir", "rm", "mv", "cat"];

/// `FILE_COMMAND_REMOVED` (ADR-010 §8.2): the message a removed file command answers, naming the way it is done now.
#[must_use]
pub fn file_command_removed(command: &str) -> String {
    format!(
        "{command}: file operations go through the mounted file system; fs-cli mount <mount_point> --workspace <name>"
    )
}

/// Printed when a workspace cannot be opened because **this machine is serving it right now** (ADR-010 §8.2).
///
/// The mount point and pid come from the mount records: a message that only says "in use" leaves the user to guess which window to close.
#[must_use]
pub fn mounted_refusal(command: &str, workspace: &str, mount_point: &str, pid: u32) -> String {
    format!(
        "{command}: workspace {workspace} is mounted at {mount_point} (pid {pid}); run `layerfs umount {mount_point}` first, or use the mount"
    )
}

/// Printed when a workspace is held by a process that has **no mount record** — another `fs-cli` invocation, most likely.
pub const IN_USE_ELSEWHERE: &str =
    "workspace is open in another process (another fs-cli?); close it and try again";

/// A failure that knows which **exit code** it deserves (ADR-010 §8.3).
///
/// The binary used to answer 1 for everything, which makes a script unable to tell "retry when the other process finishes" from "your command is wrong".
/// Codes: 2 usage, 3 not found, 4 busy/in use, 5 conflict; everything else stays 1.
#[derive(Debug)]
pub struct Failure {
    /// What to print.
    pub message: String,
    /// What to exit with.
    pub code: u8,
}

impl Failure {
    /// A usage error — the command line itself is wrong.
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    /// Something named in the command does not exist.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 3,
        }
    }

    /// Another process is using what this command needs.
    #[must_use]
    pub fn in_use(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 4,
        }
    }

    /// The request would lose something the user has not asked to lose (a dirty workspace, a branch that moved).
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 5,
        }
    }

    /// A failure the **mount** reported over the pipe, turned into the same code the local path would have produced.
    ///
    /// This is the whole reason the protocol carries a `kind`: the mount classifies its failure where the error type still exists, and the CLI maps that
    /// classification to an exit code in one place. Falling back to a message means a script cannot tell "discard your changes" (5) from "that commit does not
    /// exist" (3) — the distinction the codes were introduced for.
    #[must_use]
    pub fn from_pipe_kind(kind: &str, detail: String) -> Self {
        match kind {
            "not-found" => Self::not_found(detail),
            "workspace-dirty" => Self::conflict(detail),
            "in-use" => Self::in_use(detail),
            _ => Self {
                message: format!("the mount refused the command ({kind}): {detail}"),
                code: 1,
            },
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// Where mount records live: `%LOCALAPPDATA%\layerfs\mounts`, overridable with `LAYERFS_MOUNTS_DIR` (tests only).
///
/// In the library, not the binary, because the **criteria** have to find the same directory the binary writes to: a test that recomputes the path has its own copy
/// of the rule, and the two drift silently. The old `%TEMP%` directory is deliberately **not** searched or cleaned (A7b) — an older binary may still be running
/// there, and deleting its records would strand its mounts.
#[must_use]
pub fn mounts_dir() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("LAYERFS_MOUNTS_DIR") {
        return std::path::PathBuf::from(path);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("layerfs").join("mounts")
}

/// The record name for a mount point: `V:` becomes `V`, `C:\mnt\x` becomes `C_mnt_x`.
#[must_use]
pub fn record_name(mount_point: &std::path::Path) -> String {
    let text = mount_point.to_string_lossy();
    let trimmed = text.trim_end_matches([':', '/', '\\']);
    // Runs of separators collapse: `C:\mnt` is one boundary, not two, and a double underscore in a file name helps nobody.
    let mut flattened = String::new();
    for character in trimmed.chars() {
        let replacement = if matches!(character, ':' | '/' | '\\') {
            '_'
        } else {
            character
        };
        if replacement == '_' && flattened.ends_with('_') {
            continue;
        }
        flattened.push(replacement);
    }
    if flattened.is_empty() {
        "root".to_string()
    } else {
        flattened.trim_matches('_').to_string()
    }
}

#[must_use]
pub fn restore_refusal(files: usize, dirs: usize) -> String {
    let files = if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{files} files")
    };
    if dirs == 0 {
        format!("restore: {files} still open; close them and try again — nothing was changed")
    } else {
        format!(
            "restore: {files} still open ({dirs} directory handle(s) do not block); close the files and try again — nothing was changed"
        )
    }
}

/// Printed after a `restore` / `switch` that left directory handles pointing at the old view.
#[must_use]
pub fn dirs_invalidated(dirs: usize) -> String {
    let handles = if dirs == 1 {
        "1 directory handle".to_owned()
    } else {
        format!("{dirs} directory handles")
    };
    format!("{handles} (a shell's cwd or an Explorer window) now point at the old view; run `cd .` in them to refresh")
}
/// Holds the calling thread at a **named** point for a moment, when a criterion asks for it (`WCAS_CLI_PAUSE_AT=<point>=<ms>`).
///
/// Not a crash point (that is `WCAS_WORKSPACE_CRASH_AT`, which tests recovery): a pause tests **ordering** — that something which arrives while the mount is
/// half-way through a change waits for the change rather than racing it. Off unless the variable names this exact point, so production never sleeps.
///
/// The variable form is `<point>=<milliseconds>`, and several points can be listed with commas; a bare `<point>` means 1000 ms.
pub fn pause_point(point: &str) {
    let Some(spec) = std::env::var_os("WCAS_CLI_PAUSE_AT") else {
        return;
    };
    let spec = spec.to_string_lossy().into_owned();
    for entry in spec.split(',') {
        let (name, milliseconds) = match entry.split_once('=') {
            Some((name, value)) => (name.trim(), value.trim().parse::<u64>().unwrap_or(1000)),
            None => (entry.trim(), 1000),
        };
        if name == point {
            std::thread::sleep(std::time::Duration::from_millis(milliseconds));
            return;
        }
    }
}
/// Rewrites a **held** mount record in place, for the `switch` that changes which workspace a mount serves (S3 card §5.2, step 6a).
///
/// The record's handle is held by the mount for its whole life, and that is the point: readers poll it (W1 does, every 10 ms) and must never see it `Gone` or
/// `Stale` while a switch is happening. So the file is **truncated and rewritten through the same handle**, never deleted and recreated.
///
/// Two consequences, both deliberate:
///
/// - a reader can land in the middle and see an empty or half-written record, so [`record_is_live`] treats "cannot parse" as **retry once** rather than as
///   something else (the liveness semantics themselves do not change);
/// - the write is `SetEndOfFile(0)` followed by one `WriteFile` (what `set_len(0)` + `write_all` does here), because a shorter record would otherwise leave the
///   tail of the old one behind and produce a line that parses but is wrong.
///
/// # Errors
///
/// Whatever the file system says. A failure here leaves the record as it was (truncate-then-write can leave it empty; the caller rewrites it), which is why this
/// is called **before** the success is reported, not after.
pub fn rewrite_record(record: &mut std::fs::File, contents: &str) -> std::io::Result<()> {
    use std::io::{Seek as _, SeekFrom, Write as _};
    record.set_len(0)?;
    record.seek(SeekFrom::Start(0))?;
    record.write_all(contents.as_bytes())?;
    record.flush()?;
    Ok(())
}
pub fn hold_record_for_test(
    path: &std::path::Path,
    contents: &str,
) -> std::io::Result<std::fs::File> {
    use std::io::Write as _;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_DELETE: u32 = 0x4;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        Ok(file)
    }
    #[cfg(not(windows))]
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        Ok(file)
    }
}
#[cfg(test)]
mod tests {
    use super::{
        file_command_removed, mounted_refusal, record_name, Failure, FILE_COMMANDS,
        IN_USE_ELSEWHERE, NO_WORKSPACE,
    };
    use std::path::Path;

    #[test]
    fn the_removed_commands_are_exactly_the_eight() {
        assert_eq!(
            FILE_COMMANDS,
            &["put", "get", "ls", "stat", "mkdir", "rm", "mv", "cat"]
        );
        // The message has to name the replacement, not only the removal (ADR-010 §8.1).
        assert!(file_command_removed("put").contains("mount"));
    }

    #[test]
    fn the_no_workspace_text_is_the_one_the_adr_fixes() {
        assert_eq!(
            NO_WORKSPACE,
            "no workspace specified; use --workspace <name>, or cd into a mount point"
        );
        assert_eq!(Failure::usage(NO_WORKSPACE).code, 2);
    }

    /// **A3's message**: the refusal names the mount **and** the process, because those are the two facts that make it actionable.
    #[test]
    fn a_mounted_refusal_names_the_mount_and_the_pid() {
        let message = mounted_refusal("commit", "dev", "U:", 4242);
        assert!(message.contains("commit"));
        assert!(message.contains("dev"));
        assert!(message.contains("U:"));
        assert!(message.contains("4242"));
        assert!(message.contains("umount"));
        // The other case is deliberately different: no mount, so nothing to unmount.
        assert!(!IN_USE_ELSEWHERE.contains("umount"));
    }

    #[test]
    fn a_mount_point_becomes_a_record_name() {
        assert_eq!(record_name(Path::new("V:")), "V");
        assert_eq!(record_name(Path::new("C:\\mnt\\x")), "C_mnt_x");
    }
}
