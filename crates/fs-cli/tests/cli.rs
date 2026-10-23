//! The command surface, end to end (ADR-004).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "fs-cli-{label}-{}-{}-{sequence}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("sandbox");
        Self { root }
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fs-cli"));
        command.arg("--store").arg(self.store());
        command.args(arguments);
        command.output().expect("run fs-cli")
    }

    fn ok(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);
        assert!(
            output.status.success(),
            "command {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf-8 output")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn init_creates_the_store() {
    let sandbox = Sandbox::new("init");
    let output = sandbox.ok(&["init"]);
    assert!(output.contains("initialised"), "{output}");
    assert!(sandbox.store().join("store.sqlite").exists());
}

#[test]
fn put_then_get_round_trips_through_host_files() {
    let sandbox = Sandbox::new("put-get");
    sandbox.ok(&["init"]);

    let source = sandbox.root.join("source.bin");
    let destination = sandbox.root.join("destination.bin");
    let payload = b"the quick brown fox jumps over the lazy dog";
    std::fs::write(&source, payload).expect("write source");

    sandbox.ok(&["put", "/doc.txt", source.to_str().expect("utf8")]);
    sandbox.ok(&["get", "/doc.txt", destination.to_str().expect("utf8")]);

    let read = std::fs::read(&destination).expect("read destination");
    assert_eq!(read, payload);
}

#[test]
fn get_without_a_target_writes_to_stdout() {
    let sandbox = Sandbox::new("get-stdout");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"stdout payload").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    let output = sandbox.run(&["get", "/f"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"stdout payload");
}

#[test]
fn ls_lists_a_directory() {
    let sandbox = Sandbox::new("ls");
    sandbox.ok(&["init"]);
    sandbox.ok(&["mkdir", "/dir"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"x").expect("write");
    sandbox.ok(&["put", "/dir/file.txt", source.to_str().expect("utf8")]);

    let output = sandbox.ok(&["ls", "/dir"]);
    assert!(output.contains("file.txt"), "{output}");
}

#[test]
fn stat_reports_size_and_kind() {
    let sandbox = Sandbox::new("stat");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"12345").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    let output = sandbox.ok(&["stat", "/f"]);
    assert!(output.contains("size:      5"), "{output}");
    assert!(output.contains("File"), "{output}");
}

#[test]
fn mkdir_rm_and_mv_work_together() {
    let sandbox = Sandbox::new("tree");
    sandbox.ok(&["init"]);
    sandbox.ok(&["mkdir", "/a"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"move me").expect("write");
    sandbox.ok(&["put", "/a/f", source.to_str().expect("utf8")]);

    sandbox.ok(&["mv", "/a/f", "/a/g"]);
    let listing = sandbox.ok(&["ls", "/a"]);
    assert!(listing.contains('g') && !listing.contains('f'), "{listing}");

    sandbox.ok(&["rm", "/a/g"]);
    assert!(sandbox.ok(&["ls", "/a"]).trim().is_empty());

    sandbox.ok(&["rm", "/a"]);
    assert!(!sandbox.ok(&["ls", "/"]).contains('a'));
}

#[test]
fn verify_reports_a_healthy_store() {
    let sandbox = Sandbox::new("verify");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"payload").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    let output = sandbox.ok(&["verify"]);
    assert!(output.contains("ok"), "{output}");
}

#[test]
fn stats_reports_counts() {
    let sandbox = Sandbox::new("stats");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"payload").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    let output = sandbox.ok(&["stats"]);
    assert!(output.contains("objects:"), "{output}");
    assert!(output.contains("root records:"), "{output}");
}

#[test]
fn a_missing_store_is_an_error_not_a_new_one() {
    let sandbox = Sandbox::new("missing-store");
    // No `init`: the other commands must not silently create a store.
    let output = sandbox.run(&["ls", "/"]);
    assert!(!output.status.success(), "expected a failure");
    assert!(
        !sandbox.store().join("store.sqlite").exists(),
        "a typo must not materialise a store"
    );
}

#[test]
fn unknown_command_and_missing_options_fail() {
    let sandbox = Sandbox::new("usage");
    sandbox.ok(&["init"]);
    let output = sandbox.run(&["frobnicate"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown command"));

    let output = Command::new(env!("CARGO_BIN_EXE_fs-cli"))
        .arg("ls")
        .arg("/")
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--store"));
}

#[test]
fn help_is_printed() {
    let output = Command::new(env!("CARGO_BIN_EXE_fs-cli"))
        .arg("--help")
        .output()
        .expect("run");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in [
        "init", "put", "get", "ls", "stat", "mkdir", "rm", "mv", "verify", "stats",
    ] {
        assert!(text.contains(command), "help omits {command}");
    }
}

#[test]
fn put_overwrites_an_existing_file() {
    let sandbox = Sandbox::new("overwrite");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"first version").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    std::fs::write(&source, b"second").expect("write");
    sandbox.ok(&["put", "/f", source.to_str().expect("utf8")]);

    let output = sandbox.run(&["get", "/f"]);
    assert_eq!(output.stdout, b"second");
}

#[test]
fn a_name_with_spaces_round_trips() {
    let sandbox = Sandbox::new("spaces");
    sandbox.ok(&["init"]);
    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"spaced").expect("write");
    sandbox.ok(&[
        "put",
        "/a file with spaces.txt",
        source.to_str().expect("utf8"),
    ]);

    let output = sandbox.run(&["get", "/a file with spaces.txt"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"spaced");
}

#[test]
fn a_namespace_option_selects_an_isolated_tree() {
    let sandbox = Sandbox::new("namespace");
    sandbox.ok(&["init"]);
    let output = Command::new(env!("CARGO_BIN_EXE_fs-cli"))
        .arg("--store")
        .arg(sandbox.store())
        .arg("--namespace")
        .arg("other")
        .arg("init")
        .output()
        .expect("run");
    assert!(output.status.success());

    let source = sandbox.root.join("s.bin");
    std::fs::write(&source, b"payload").expect("write");
    sandbox.ok(&["put", "/only-in-default", source.to_str().expect("utf8")]);

    let listing = sandbox.ok(&["ls", "/"]);
    assert!(listing.contains("only-in-default"));

    let other = Command::new(env!("CARGO_BIN_EXE_fs-cli"))
        .arg("--store")
        .arg(sandbox.store())
        .arg("--namespace")
        .arg("other")
        .arg("ls")
        .arg("/")
        .output()
        .expect("run");
    assert!(other.status.success());
    assert!(
        String::from_utf8_lossy(&other.stdout).trim().is_empty(),
        "the other namespace is empty"
    );
}

#[test]
fn a_large_file_round_trips_through_the_cli() {
    let sandbox = Sandbox::new("large");
    sandbox.ok(&["init"]);
    let payload: Vec<u8> = (0..(fs_api::BLOCK_BYTES as usize * 3 + 123))
        .map(|index| (index % 251) as u8)
        .collect();
    let source = sandbox.root.join("big.bin");
    std::fs::write(&source, &payload).expect("write");

    sandbox.ok(&["put", "/big", source.to_str().expect("utf8")]);
    let destination = sandbox.root.join("back.bin");
    sandbox.ok(&["get", "/big", destination.to_str().expect("utf8")]);

    let read = std::fs::read(&destination).expect("read back");
    assert_eq!(read, payload);
}

/// Guard against the binary being replaced by another placeholder.
#[test]
fn the_binary_reports_its_own_errors_with_a_nonzero_exit() {
    let sandbox = Sandbox::new("errors");
    sandbox.ok(&["init"]);
    let output = sandbox.run(&["stat", "/nope"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("fs-cli:"), "{text}");
    assert!(text.contains("/nope"), "{text}");
    let _ = Path::new("");
}
