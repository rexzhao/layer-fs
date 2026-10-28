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
