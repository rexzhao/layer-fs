//! **The user-facing text, checked against the binary** (README + help card).
//!
//! Prose rots silently: a removed command stays in `--help` for months and a quick start keeps a flag that
//! no longer exists, and nothing goes red. These criteria make the two documents answer to the code that is
//! actually shipped.

mod common;

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fs-cli"))
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the crate lives under the repository root")
        .to_path_buf()
}

fn help() -> String {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("--help runs");
    assert!(output.status.success(), "--help must succeed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The commands `--help` lists, in its own order: the first word of every indented line.
fn help_commands(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .filter(|word| !word.starts_with('-'))
        .collect()
}

/// **C1** — the help text no longer advertises the file commands ADR-010 removed.
///
/// They exit 2 now, so a help text that lists them is worse than no help: it teaches a command that cannot
/// work. The check is on **words**, not substrings: `status` contains `stat` and is a real command.
#[test]
fn the_help_text_does_not_advertise_removed_file_commands() {
    common::strict_gate();
    let text = help();
    let removed = ["put", "get", "ls", "stat", "mkdir", "rm", "mv", "cat"];
    let advertised = help_commands(&text);
    for command in removed {
        assert!(
            !advertised.iter().any(|listed| listed == command),
            "cell C1: `--help` still lists the removed command {command:?}: {advertised:?}"
        );
        // And not as a bare word anywhere in the text either (a sentence is enough to teach it).
        let as_word = text
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word == command);
        assert!(
            !as_word,
            "cell C1: the help text mentions {command:?} as a word"
        );
    }
}

/// **C2** — every command line in the README's quick start is one the binary knows.
///
/// The README's fenced blocks are read, the `layerfs …` lines are taken, and each command word is looked up in
/// `--help`. A typo in the documentation is then a test failure rather than a support question — and the same
/// check catches a command that was renamed without the guide.
#[test]
fn every_command_in_the_readme_exists_in_the_help_text() {
    common::strict_gate();
    let readme =
        std::fs::read_to_string(repository_root().join("README.md")).expect("README.md exists");
    let advertised = help_commands(&help());
    let mut checked = 0;
    for line in readme.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("layerfs ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        // Skip global options **and their values**: `--store F:\repo mount Z:` starts with two words that are not the command.
        let mut command = "";
        while let Some(word) = words.next() {
            if word.starts_with("--") {
                if !word.contains('=') {
                    let _ = words.next();
                }
                continue;
            }
            command = word;
            break;
        }
        if command.is_empty() || command.starts_with('-') {
            continue;
        }
        assert!(
            advertised.iter().any(|listed| listed == command),
            "cell C2: the README uses the command {command:?}, which `--help` does not list: {advertised:?}"
        );
        checked += 1;
    }
    println!(
        "C2 reading: {checked} README command lines checked against {} help entries",
        advertised.len()
    );
    assert!(
        checked >= 8,
        "cell C2: the README's quick start must have been read, only {checked} lines found"
    );
}

/// **C3** — no file command appears in the README as a `layerfs` invocation either.
///
/// The guide must not teach what the binary refuses; C1 covers the help text, this covers the prose.
#[test]
fn the_readme_does_not_teach_a_file_command() {
    common::strict_gate();
    let readme =
        std::fs::read_to_string(repository_root().join("README.md")).expect("README.md exists");
    for command in ["put", "get", "ls", "stat", "mkdir", "rm", "mv", "cat"] {
        let taught = readme
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("layerfs "))
            .any(|line| {
                line.split_whitespace()
                    .skip_while(|word| {
                        word.starts_with('-') || word.contains(":\\") || word.contains("repo")
                    })
                    .any(|word| word == command)
            });
        assert!(
            !taught,
            "cell C3: the README shows a `layerfs {command} …` line, but that command is gone"
        );
    }
}

/// **C4** — every command `--help` lists has its own `help <command>` text, and `help` never invents one.
///
/// `command_help` is a second list of names next to the dispatch table, and two lists drift. This walks the advertised commands and asks the binary for each.
#[test]
fn every_command_has_a_help_entry() {
    common::strict_gate();
    for command in help_commands(&help()) {
        let output = Command::new(binary())
            .arg("help")
            .arg(&command)
            .output()
            .expect("help runs");
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "cell C4: `help {command}` must succeed, got {:?} / {:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains(&command),
            "cell C4: `help {command}` must describe it, got {text:?}"
        );
    }
    // And an unknown name is a usage error, not a silent success.
    let output = Command::new(binary())
        .arg("help")
        .arg("nonsense")
        .output()
        .expect("help runs");
    assert_eq!(
        output.status.code(),
        Some(2),
        "cell C4: `help nonsense` must be a usage error"
    );
}
