//! Command line interface for the Workspace CAS filesystem.
//!
//! **Nothing here is implemented yet.** Phase 1 task T4 adds `init`, `put`,
//! `get`, `ls`, `stat`, `mkdir`, `rm`, `mv`, `verify` and `stats`, all over the
//! same core the SDK uses. Until then the binary reports that it is a
//! placeholder and exits non-zero, so no script can mistake it for a working
//! tool.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "fs-cli {}: not implemented; Phase 1 task T4 adds the command surface. \
         No command was run and no Store was touched.",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::from(2)
}
