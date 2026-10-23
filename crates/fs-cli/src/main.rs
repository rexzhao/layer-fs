//! Command line interface for the Workspace CAS filesystem (ADR-004).
//!
//! A thin command surface: argument parsing and output only. Every operation
//! goes through [`fs_api::Fs`], the same core the SDK uses, so the CLI cannot
//! drift from the library's semantics.

use std::io::{Read, Write};
use std::process::ExitCode;

use cas_store::OpenMode;
use fs_api::{Fs, FsError};

const USAGE: &str = "\
fs-cli --store <dir> [--namespace <name>] <command> [args]

  init                        create the Store and namespace
  put   <path> <host-file>    write a host file's bytes to <path>
  get   <path> [host-file]    write <path>'s bytes to a host file (or stdout)
  ls    <path>                list a directory
  stat  <path>                show metadata
  mkdir <path>                create a directory
  rm    <path>                remove a file or an empty directory
  mv    <from> <to>           rename
  verify                      check objects and namespace references
  stats                       show object and publication counts";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fs-cli: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!("{USAGE}");
        return Ok(());
    }

    let store =
        take_option(&mut arguments, "--store").ok_or("missing --store <dir>; see --help")?;
    let namespace = take_option(&mut arguments, "--namespace").unwrap_or_else(|| "default".into());
    if arguments.is_empty() {
        return Err("missing command; see --help".into());
    }
    let command = arguments.remove(0);

    // `init` creates; every other command requires an existing Store, so a typo
    // in the path is reported instead of materialised.
    let mode = if command == "init" {
        OpenMode::Create
    } else {
        OpenMode::ReadWrite
    };
    let fs = Fs::open(&store, &namespace, mode)?;

    match command.as_str() {
        "init" => {
            let stats = fs.stats()?;
            println!("initialised {store} ({} objects)", stats.objects);
            Ok(())
        }
        "put" => {
            let (path, from) = two_args(&arguments, "put <path> <host-file>")?;
            let mut bytes = Vec::new();
            std::fs::File::open(&from)?.read_to_end(&mut bytes)?;
            if fs.stat(&path).is_ok() {
                fs.write_at(&path, 0, &bytes)?;
                fs.truncate(&path, bytes.len() as u64)?;
            } else {
                fs.create(&path)?;
                fs.write_at(&path, 0, &bytes)?;
            }
            println!("{path} <- {from} ({} bytes)", bytes.len());
            Ok(())
        }
        "get" => {
            let (path, to) = one_or_two_args(&arguments, "get <path> [host-file]")?;
            let size = fs.stat(&path)?.size;
            let mut bytes = vec![0u8; size as usize];
            let read = fs.read_at(&path, 0, &mut bytes)?;
            bytes.truncate(read);
            match to {
                Some(target) => {
                    std::fs::write(&target, &bytes)?;
                    println!("{path} ({read} bytes) -> {target}");
                }
                None => std::io::stdout().write_all(&bytes)?,
            }
            Ok(())
        }
        "ls" => {
            let (path, _) = one_or_two_args(&arguments, "ls <path>")?;
            for entry in fs.read_dir(&path)? {
                println!("{:?}\t{}", entry.kind, entry.name);
            }
            Ok(())
        }
        "stat" => {
            let (path, _) = one_or_two_args(&arguments, "stat <path>")?;
            let stat = fs.stat(&path)?;
            println!("path:      {path}");
            println!("kind:      {:?}", stat.kind);
            println!("id:        {}", stat.id.serial());
            println!("size:      {}", stat.size);
            println!("created:   {}", stat.created_ns);
            println!("modified:  {}", stat.modified_ns);
            Ok(())
        }
        "mkdir" => {
            let (path, _) = one_or_two_args(&arguments, "mkdir <path>")?;
            fs.mkdir(&path)?;
            Ok(())
        }
        "rm" => {
            let (path, _) = one_or_two_args(&arguments, "rm <path>")?;
            fs.remove(&path)?;
            Ok(())
        }
        "mv" => {
            let (from, to) = two_args(&arguments, "mv <from> <to>")?;
            fs.rename(&from, &to)?;
            Ok(())
        }
        "verify" => {
            let report = fs.verify()?;
            println!("references: {}", report.references);
            if report.is_clean() {
                println!("ok");
                Ok(())
            } else {
                for dangling in &report.dangling {
                    println!("dangling: {} ({})", dangling.object, dangling.reason);
                }
                Err(FsError::Corrupt {
                    detail: format!("{} dangling reference(s)", report.dangling.len()),
                }
                .into())
            }
        }
        "stats" => {
            let stats = fs.stats()?;
            let reads = fs.read_stats();
            println!("objects:        {}", stats.objects);
            println!("root records:   {}", stats.root_records);
            println!("object fetches: {}", reads.object_gets);
            Ok(())
        }
        other => Err(format!("unknown command {other}; see --help").into()),
    }
}

/// Removes `--name value` (or `--name=value`) from `arguments`.
fn take_option(arguments: &mut Vec<String>, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    if let Some(index) = arguments
        .iter()
        .position(|argument| argument.starts_with(&prefix))
    {
        let value = arguments[index][prefix.len()..].to_owned();
        arguments.remove(index);
        return Some(value);
    }
    let index = arguments.iter().position(|argument| argument == name)?;
    arguments.remove(index);
    if index < arguments.len() {
        Some(arguments.remove(index))
    } else {
        None
    }
}

fn two_args(
    arguments: &[String],
    usage: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    match arguments {
        [first, second] => Ok((first.clone(), second.clone())),
        _ => Err(format!("expected {usage}").into()),
    }
}

fn one_or_two_args(
    arguments: &[String],
    usage: &str,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    match arguments {
        [first] => Ok((first.clone(), None)),
        [first, second] => Ok((first.clone(), Some(second.clone()))),
        _ => Err(format!("expected {usage}").into()),
    }
}
