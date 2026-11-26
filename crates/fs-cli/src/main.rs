//! Command line interface for LayerFS on Windows.
//!
//! Powered by the upstream LayerFS core (`fs-layerfs`) with native Windows WinFsp mounting.

mod mountcmd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fs_layerfs::{Error as LayerError, PersistenceProfile, Repo, DEFAULT_BRANCH};

const USAGE: &str = "\
layerfs [--store <dir>] [--branch <name>] <command> [args]

  init <path> [--from <dir>] [--durable|--disposable] [--size <size>]
                                  create a repository (optionally importing an existing directory)
  branch [<name>] [--from <src>]  list branches, or fork a new branch from <src> (default: main)
  fork <name> [--from <src>]      alias for `branch <name> [--from <src>]`
  checkout <branch>               verify a branch; from inside a mount, hot-switch that mount to <branch>
  commit [-m <message>] [--force] commit uncommitted overlay changes on the mounted branch
  merge <left> <right> <into> [-m <message>]
                                  3-way merge <left> and <right> into new branch <into>
  publish <branch> [<commit>]     publish a branch's head (or commit) onto `main` as a new Layer
  diff <left> <right>             list changed paths between two branches (or `main`)
  log [<branch>] [<limit>]        list commits on a branch
  status                          current branch, base layer, head commit, root, dirty counts & open handles
  stats                           repository and storage statistics
  compact [--pages <n>]           reclaim free SQLite pages in store.sqlite (upstream incremental vacuum)

  mount <mount_point> [--branch <name>] [--no-version-gate]
                                  mount a writable branch over WinFsp until `umount` stops it
  umount <mount_point> [--force]  orderly unmount via the mount's control pipe (--force removes stale record)
  list                            list mounts recorded under %LOCALAPPDATA%\\layerfs\\mounts
  version                         WinFsp DLL version and ABI compatibility check
  help [<command>]                show general usage or help for one command

Files are read and written through the mounted filesystem, not through CLI file verbs.
Consistent with upstream LayerFS, uncommitted edits live in the mount's ephemeral overlay until `layerfs commit`.
Global option `--workspace <name>` is accepted as an alias for `--branch <name>`.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("layerfs: {error}");
            let code = exit_code(&*error);
            ExitCode::from(code)
        }
    }
}

/// Exit codes:
/// - 2: usage / invalid input
/// - 3: not found
/// - 4: busy / in use
/// - 5: conflict (nothing to commit, already exists, merge conflict)
/// - 1: other error
fn exit_code(error: &(dyn std::error::Error + 'static)) -> u8 {
    if let Some(failure) = error.downcast_ref::<fs_cli::Failure>() {
        return failure.code;
    }
    if let Some(err) = error.downcast_ref::<LayerError>() {
        return match err {
            LayerError::InvalidInput(_) => 2,
            LayerError::NotFound(_) => 3,
            LayerError::Busy(_) => 4,
            LayerError::Conflict(_) => 5,
            LayerError::Unsupported(_) | LayerError::Other(_) => 1,
        };
    }
    1
}

const DEFAULT_VOLUME_SIZE_BYTES: u64 = fs_cli::DEFAULT_VOLUME_SIZE_BYTES;

fn parse_size(text: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let trimmed = text.trim();
    let (number, unit) = trimmed.split_at(
        trimmed
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let value: u64 = number
        .parse()
        .map_err(|_| fs_cli::Failure::usage(format!("not a size: {text:?}")))?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(fs_cli::Failure::usage(format!(
                "not a size unit: {other:?} (use K, M, G, T, or a byte count)"
            ))
            .into())
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| fs_cli::Failure::usage(format!("size too large: {text:?}")).into())
}

fn command_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "init" => "init <path> [--from <dir>] [--durable|--disposable] [--size <size>]\n  Create an upstream-compatible LayerFS repository with `main` layer stack and default writable\n  branch `work`. Pass `--from <dir>` to import an existing directory via upstream `layerfs-project`.\n  Defaults to `--durable` (SQLite WAL/FULL) and 128 GiB advertised size; `--disposable` uses WAL/OFF.",
        "branch" => "branch [<name>] [--from <src>]\n  With no name: list `main` and all branches. With a name: fork a new branch from `<src>`\n  (default `main`).",
        "fork" => "fork <name> [--from <src>]\n  Fork a new writable branch `<name>` from `<src>` (default `main`).",
        "checkout" => "checkout <branch>\n  Verify `<branch>` exists; when run from inside a live mount, hot-switch that mount to `<branch>`.",
        "commit" => "commit [-m <message>] [--force]\n  Commit uncommitted ephemeral-overlay changes on the currently mounted branch (over the control pipe).\n  `--force` overwrites if the branch head moved concurrently.",
        "merge" => "merge <left> <right> <into> [-m <message>]\n  Perform a 3-way merge of branches `<left>` and `<right>` into a new branch `<into>`.",
        "publish" => "publish <branch> [<commit>]\n  Publish `<branch>`'s head (or a specific commit id) onto `main` as the next immutable Layer.",
        "diff" => "diff <left> <right>\n  Show changed paths between `<left>` and `<right>` (branches or `main`).",
        "log" => "log [<branch>] [<limit>]\n  List commits on `<branch>` (default: mounted branch or `work`), newest first.",
        "status" => "status\n  Show branch, base layer, head commit, effective root, dirty counts, and open handles.\n  Inside a mount, queries the live mount process over the control pipe.",
        "stats" => "stats\n  Show repository profile, store file size, branch count, and logical/canonical/stored bytes.",
        "compact" => "compact [--pages <n>]\n  Reclaim free SQLite pages in store.sqlite using upstream bounded incremental vacuum.\n  Logical objects and history are unchanged.",
        "mount" => "mount <mount_point> [--branch <name>] [--no-version-gate]\n  Mount a writable branch (default: `work`) via WinFsp at `<mount_point>` (drive letter like `Z:`\n  or directory path). Press Ctrl-C or run `layerfs umount <mount_point>` for orderly unmount.",
        "umount" => "umount <mount_point> [--force]\n  Ask a running mount to unmount cleanly (uncommitted ephemeral-overlay edits are discarded unless\n  committed first, matching upstream LayerFS). Refuses (exit 4) while user handles are open.\n  `--force` removes a stale mount record whose owner process is gone.",
        "list" => "list\n  List mounts recorded under %LOCALAPPDATA%\\layerfs\\mounts with live/stale status.",
        "version" => "version\n  Print WinFsp DLL version, calibrated ABI layout, and LayerFS core compatibility.",
        "help" => "help [<command>]\n  Show the top-level usage summary or detailed help for one command.",
        _ => return None,
    })
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    if arguments.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }

    let explicit_store = take_option(&mut arguments, "--store").map(PathBuf::from);
    let explicit_store_was_given = explicit_store.is_some();
    let branch_opt = take_option(&mut arguments, "--branch")
        .or_else(|| take_option(&mut arguments, "--workspace"));

    if arguments.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let command = arguments.remove(0);

    // `layerfs <cmd> --help` or `layerfs help <cmd>`
    if command == "help" {
        match arguments.first().map(String::as_str) {
            None => println!("{USAGE}"),
            Some(name) => match command_help(name) {
                Some(text) => println!("{text}"),
                None => {
                    return Err(fs_cli::Failure::usage(format!(
                        "no help for {name:?}; `layerfs help` lists the commands"
                    ))
                    .into())
                }
            },
        }
        return Ok(());
    }
    if arguments.iter().any(|a| a == "--help" || a == "-h") {
        if let Some(text) = command_help(&command) {
            println!("{text}");
            return Ok(());
        }
        println!("{USAGE}");
        return Ok(());
    }

    // Refuse legacy file verbs clearly before touching any store.
    if let Some(removed) = fs_cli::FILE_COMMANDS
        .iter()
        .find(|name| **name == command.as_str())
    {
        return Err(fs_cli::Failure::usage(fs_cli::file_command_removed(removed)).into());
    }

    // Standalone commands that don't require an existing repo via discovery:
    match command.as_str() {
        "init" => return cmd_init(explicit_store.as_deref(), &arguments),
        "umount" => return mountcmd::umount(&arguments),
        "list" => return mountcmd::list(),
        "version" => return mountcmd::version(),
        _ => {}
    }

    // Discover active mount from CWD when `--store` / `--branch` are omitted.
    let discovered = std::env::current_dir().ok().and_then(|cwd| {
        fs_cli::discovery::discover(
            &cwd,
            fs_cli::winpaths::volume_root,
            fs_cli::winpaths::is_volume_mount_point,
            mountcmd::live_records,
        )
    });
    let target = fs_cli::discovery::resolve(
        explicit_store.as_deref(),
        branch_opt.as_deref(),
        discovered.as_ref(),
    );
    let store = target.store;
    let active_branch = target.workspace;

    if command == "mount" {
        if !explicit_store_was_given && discovered.is_none() && store.as_os_str() == "." {
            // Allow positional `mount <store> <mount_point>` fallback inside mountcmd, or error clearly.
            if arguments.len() < 2 {
                return Err(fs_cli::Failure::usage(
                    "mount needs a store: add `--store <dir>` (the repository to serve)",
                )
                .into());
            }
        }
        return mountcmd::mount(&store, active_branch.as_deref(), &arguments);
    }

    // Inside a live mount, route `status`, `commit`, and `checkout` directly to the mount's control pipe.
    if let Some(here) = discovered.as_ref() {
        if !here.pipe.is_empty() && !explicit_store_was_given {
            match command.as_str() {
                "status" => return pipe_status(here),
                "commit" => return pipe_commit(here, &arguments),
                "checkout" => {
                    let target_branch = arguments
                        .first()
                        .ok_or_else(|| fs_cli::Failure::usage("expected `checkout <branch>`"))?;
                    if here.workspace != *target_branch {
                        return switch_mounted(here, target_branch);
                    }
                    println!(
                        "already serving {} at {}",
                        here.workspace,
                        here.mount_point.display()
                    );
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    if !explicit_store_was_given && discovered.is_none() {
        return Err(fs_cli::Failure::usage(
            "no store specified; pass `--store <dir>` or run from inside a mounted volume",
        )
        .into());
    }

    if command == "compact" {
        return cmd_compact(&store, &arguments);
    }

    let repo = Repo::open(&store)?;
    match command.as_str() {
        "branch" => cmd_branch(&repo, &arguments),
        "fork" => cmd_fork(&repo, &arguments),
        "checkout" => cmd_checkout(&repo, &arguments),
        "commit" => {
            let branch = active_branch.as_deref().unwrap_or(DEFAULT_BRANCH);
            Err(fs_cli::Failure::usage(format!(
                "commit must be run from inside a mounted volume (branch {branch}) so uncommitted overlay changes are captured"
            ))
            .into())
        }
        "merge" => cmd_merge(&repo, &arguments),
        "publish" => cmd_publish(&repo, &arguments),
        "diff" => cmd_diff(&repo, &arguments),
        "log" => cmd_log(&repo, active_branch.as_deref(), &arguments),
        "status" => cmd_offline_status(&repo, active_branch.as_deref()),
        "stats" => cmd_stats(&repo),
        other => Err(fs_cli::Failure::usage(format!("unknown command {other:?}; see `layerfs help`")).into()),
    }
}

// ------------------------------------------------------------------ commands

fn cmd_init(
    explicit_store: Option<&Path>,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut profile = PersistenceProfile::Durable;
    let mut volume_size = DEFAULT_VOLUME_SIZE_BYTES;
    let mut from_dir: Option<PathBuf> = None;
    let mut positional: Option<PathBuf> = None;
    let mut i = 0;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "--durable" => {
                profile = PersistenceProfile::Durable;
                i += 1;
            }
            "--disposable" => {
                profile = PersistenceProfile::Disposable;
                i += 1;
            }
            "--from" => {
                let raw = arguments
                    .get(i + 1)
                    .ok_or_else(|| fs_cli::Failure::usage("--from requires a directory path"))?;
                from_dir = Some(PathBuf::from(raw));
                i += 2;
            }
            other if other.starts_with("--from=") => {
                from_dir = Some(PathBuf::from(&other["--from=".len()..]));
                i += 1;
            }
            "--size" => {
                let raw = arguments
                    .get(i + 1)
                    .ok_or_else(|| fs_cli::Failure::usage("--size requires a value"))?;
                volume_size = parse_size(raw)?;
                i += 2;
            }
            other if other.starts_with("--size=") => {
                volume_size = parse_size(&other["--size=".len()..])?;
                i += 1;
            }
            other if other.starts_with('-') => {
                return Err(fs_cli::Failure::usage(format!("unknown init option {other:?}")).into());
            }
            other => {
                positional = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    let path = positional
        .or_else(|| explicit_store.map(Path::to_path_buf))
        .ok_or_else(|| {
            fs_cli::Failure::usage(
                "usage: layerfs init <path> [--from <dir>] [--durable|--disposable] [--size <size>]",
            )
        })?;

    if let Some(source) = from_dir {
        let (repo, summary) = Repo::init_from(&path, &source, profile, volume_size)?;
        let stats = repo.stats()?;
        println!(
            "initialized {} from {} (entries={}, profile={}, default branch={}, main={})",
            path.display(),
            source.display(),
            summary.entries,
            repo.profile().as_str(),
            DEFAULT_BRANCH,
            short(&stats.head_layer),
        );
    } else {
        let repo = Repo::init(&path, profile, volume_size)?;
        let stats = repo.stats()?;
        println!(
            "initialized {} (profile={}, default branch={}, main={})",
            path.display(),
            repo.profile().as_str(),
            DEFAULT_BRANCH,
            short(&stats.head_layer),
        );
    }
    Ok(())
}

fn cmd_compact(store: &Path, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut max_pages: Option<u32> = None;
    let mut i = 0;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "--pages" => {
                let raw = arguments
                    .get(i + 1)
                    .ok_or_else(|| fs_cli::Failure::usage("--pages requires a positive integer"))?;
                let parsed: u32 = raw
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| fs_cli::Failure::usage(format!("invalid --pages value: {raw:?}")))?;
                max_pages = Some(parsed);
                i += 2;
            }
            other if other.starts_with("--pages=") => {
                let raw = &other["--pages=".len()..];
                let parsed: u32 = raw
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| fs_cli::Failure::usage(format!("invalid --pages value: {raw:?}")))?;
                max_pages = Some(parsed);
                i += 1;
            }
            other => {
                return Err(
                    fs_cli::Failure::usage(format!("unknown compact option {other:?}")).into(),
                );
            }
        }
    }
    let summary = Repo::reclaim_space(store, max_pages)?;
    println!(
        "compacted {} (free_before={}, reclaimed={}, remaining_free={})",
        store.display(),
        summary.free_pages_before,
        summary.reclaimed_pages,
        summary.remaining_free_pages
    );
    Ok(())
}

fn cmd_branch(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.is_empty() {
        for b in repo.branches()? {
            let head = b.head.as_deref().map_or("-", short);
            println!("{:<20} head={:<14} base={}", b.name, head, short(&b.base));
        }
        return Ok(());
    }
    cmd_fork(repo, arguments)
}

fn cmd_fork(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut name: Option<&str> = None;
    let mut from: Option<&str> = None;
    let mut i = 0;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "--from" => {
                from = Some(
                    arguments
                        .get(i + 1)
                        .ok_or_else(|| fs_cli::Failure::usage("--from requires a branch name"))?
                        .as_str(),
                );
                i += 2;
            }
            other if other.starts_with("--from=") => {
                from = Some(&other["--from=".len()..]);
                i += 1;
            }
            other if other.starts_with('-') => {
                return Err(fs_cli::Failure::usage(format!("unknown option {other:?}")).into());
            }
            other => {
                name = Some(other);
                i += 1;
            }
        }
    }
    let name = name.ok_or_else(|| fs_cli::Failure::usage("expected `<name> [--from <src>]`"))?;
    let info = repo.fork(name, from)?;
    println!(
        "forked {} (base={}, head={})",
        info.name,
        short(&info.base),
        info.head.as_deref().map_or("-", short)
    );
    Ok(())
}

fn cmd_checkout(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let branch = arguments
        .first()
        .ok_or_else(|| fs_cli::Failure::usage("expected `checkout <branch>`"))?;
    if !repo.branches()?.iter().any(|b| b.name == *branch) {
        return Err(fs_cli::Failure::not_found(format!(
            "branch {branch:?} not found; create it with `layerfs branch {branch}`"
        ))
        .into());
    }
    match mountcmd::live_mount_of(branch) {
        Some((mount_point, pid)) => {
            println!("branch {branch} is mounted at {mount_point} (pid {pid})");
        }
        None => {
            println!(
                "branch {branch} is ready; mount it with `layerfs --store {} mount <point> --branch {branch}`",
                repo.dir().display()
            );
        }
    }
    Ok(())
}

fn cmd_merge(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut message: Option<&str> = None;
    let mut i = 0;
    while i < arguments.len() {
        match arguments[i].as_str() {
            "-m" | "--message" => {
                message = Some(
                    arguments
                        .get(i + 1)
                        .ok_or_else(|| fs_cli::Failure::usage("-m requires a message"))?
                        .as_str(),
                );
                i += 2;
            }
            other if other.starts_with('-') => {
                return Err(fs_cli::Failure::usage(format!("unknown option {other:?}")).into());
            }
            other => {
                positionals.push(other);
                i += 1;
            }
        }
    }
    if positionals.len() != 3 {
        return Err(
            fs_cli::Failure::usage("usage: layerfs merge <left> <right> <into> [-m <message>]")
                .into(),
        );
    }
    let outcome = repo.merge(positionals[0], positionals[1], positionals[2], message)?;
    if !outcome.conflicts.is_empty() {
        for path in &outcome.conflicts {
            println!("CONFLICT: {path}");
        }
        return Err(fs_cli::Failure::conflict(format!(
            "merge into {} has {} conflict(s)",
            outcome.branch,
            outcome.conflicts.len()
        ))
        .into());
    }
    println!(
        "{} {} (fast_forward={}, head={}, root={})",
        outcome.kind,
        outcome.branch,
        outcome.fast_forward,
        outcome.head.as_deref().map_or("-", short),
        outcome.root.as_deref().map_or("-", short),
    );
    Ok(())
}

fn cmd_publish(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let branch = arguments
        .first()
        .ok_or_else(|| fs_cli::Failure::usage("usage: layerfs publish <branch> [<commit>]"))?;
    let commit = arguments.get(1).map(String::as_str);
    let outcome = repo.publish(branch, commit)?;
    println!(
        "published {} -> main layer {} (kind={}, overwrote={})",
        branch,
        short(&outcome.layer),
        outcome.kind,
        outcome.overwrote_count
    );
    Ok(())
}

fn cmd_diff(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.len() < 2 {
        return Err(fs_cli::Failure::usage("usage: layerfs diff <left> <right>").into());
    }
    let diffs = repo.diff_branches(&arguments[0], &arguments[1])?;
    if diffs.is_empty() {
        println!("no differences");
        return Ok(());
    }
    for d in diffs {
        println!("{:<10} {}", d.kind, d.path);
    }
    Ok(())
}

fn cmd_log(
    repo: &Repo,
    active_branch: Option<&str>,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let named = arguments
        .first()
        .filter(|text| !text.starts_with('-') && text.parse::<usize>().is_err());
    let branch = named
        .map(String::as_str)
        .or(active_branch)
        .unwrap_or(DEFAULT_BRANCH);
    let limit = arguments
        .iter()
        .find_map(|text| text.parse::<usize>().ok())
        .unwrap_or(50);
    let commits = repo.log(branch, limit)?;
    if commits.is_empty() {
        println!("no commits on {branch}");
        return Ok(());
    }
    for c in commits {
        println!(
            "{}  root={}  {}",
            short(&c.id),
            short(&c.root),
            c.message.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_offline_status(
    repo: &Repo,
    active_branch: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let branch = active_branch.unwrap_or(DEFAULT_BRANCH);
    let Some(info) = repo.branches()?.into_iter().find(|b| b.name == branch) else {
        return Err(fs_cli::Failure::not_found(format!("branch {branch:?} not found")).into());
    };
    println!("branch:    {}", info.name);
    println!("base:      {}", short(&info.base));
    println!(
        "head:      {}",
        info.head.as_deref().map_or("(none)", short)
    );
    if let Some((mount_point, pid)) = mountcmd::live_mount_of(&info.name) {
        println!("mounted:   {mount_point} (pid {pid})");
    } else {
        println!("mounted:   no");
    }
    Ok(())
}

fn cmd_stats(repo: &Repo) -> Result<(), Box<dyn std::error::Error>> {
    let stats = repo.stats()?;
    println!("profile:         {}", stats.profile.as_str());
    println!("volume_size:     {} bytes", stats.volume_size_bytes);
    println!("store_file:      {} bytes", stats.store_file_bytes);
    println!("branches:        {}", stats.branches);
    println!("head_layer:      {}", short(&stats.head_layer));
    println!("logical_bytes:   {}", stats.logical_bytes);
    println!("canonical_bytes: {}", stats.canonical_bytes);
    println!("stored_bytes:    {}", stats.stored_bytes);
    Ok(())
}

// ------------------------------------------------------------------ pipe helpers

fn pipe_status(here: &fs_cli::discovery::Record) -> Result<(), Box<dyn std::error::Error>> {
    let request = fs_cli::pipe::Request {
        v: fs_cli::pipe::PROTOCOL,
        op: "status".to_owned(),
        args: serde_json::json!({}),
    };
    match fs_cli::pipe_win::call(&here.pipe, &request, fs_cli::pipe_win::CONNECT_WAIT_MS) {
        Ok(fs_cli::pipe::Response::Status {
            workspace,
            branch,
            base_commit,
            base_root,
            dirty,
            handles,
        }) => {
            println!("workspace: {workspace}");
            println!("branch:    {branch}");
            println!("base:      {}", short(&base_commit));
            println!("root:      {}", short(&base_root));
            println!("dirty:     {}", if dirty { "yes" } else { "no" });
            println!("handles:   {handles}");
            Ok(())
        }
        Ok(other) => Err(format!("the mount refused `status`: {other:?}").into()),
        Err(error) => Err(fs_cli::Failure::in_use(format!(
            "the mount at {} is not answering ({error})",
            here.mount_point.display()
        ))
        .into()),
    }
}

fn pipe_commit(
    here: &fs_cli::discovery::Record,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let message = arguments
        .iter()
        .position(|a| a == "-m" || a == "--message")
        .and_then(|i| arguments.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let force = arguments.iter().any(|a| a == "--force");
    let request = fs_cli::pipe::Request {
        v: fs_cli::pipe::PROTOCOL,
        op: "commit".to_owned(),
        args: serde_json::json!({ "message": message, "force": force }),
    };
    match fs_cli::pipe_win::call(&here.pipe, &request, fs_cli::pipe_win::CONNECT_WAIT_MS) {
        Ok(fs_cli::pipe::Response::Failed { kind, fields, .. }) if kind == "committed" => {
            let commit = fields
                .get("commit")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let root = fields
                .get("root")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            println!("commit {} (root {})", short(commit), short(root));
            Ok(())
        }
        Ok(fs_cli::pipe::Response::Failed { kind, detail, .. }) => {
            Err(fs_cli::Failure::from_pipe_kind(&kind, detail).into())
        }
        Ok(other) => Err(format!("the mount answered {other:?}").into()),
        Err(error) => Err(fs_cli::Failure::in_use(format!(
            "the mount at {} is not answering ({error})",
            here.mount_point.display()
        ))
        .into()),
    }
}

fn switch_mounted(
    here: &fs_cli::discovery::Record,
    branch: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_current_dir(&here.store).map_err(|error| {
        fs_cli::Failure::in_use(format!(
            "cannot step out of {} ({error}); run this from outside the mount",
            here.mount_point.display()
        ))
    })?;
    let request = fs_cli::pipe::Request {
        v: fs_cli::pipe::PROTOCOL,
        op: "switch".to_owned(),
        args: serde_json::json!({ "workspace": branch }),
    };
    match fs_cli::pipe_win::call(&here.pipe, &request, fs_cli::pipe_win::CONNECT_WAIT_MS) {
        Ok(fs_cli::pipe::Response::Switched {
            workspace,
            branch,
            dirs_invalidated,
        }) => {
            println!(
                "switched {} to {workspace} (branch {branch})",
                here.mount_point.display()
            );
            if dirs_invalidated > 0 {
                println!("{}", fs_cli::dirs_invalidated(dirs_invalidated));
            }
            Ok(())
        }
        Ok(fs_cli::pipe::Response::RestoreRefused { files, dirs }) => {
            Err(fs_cli::Failure::in_use(fs_cli::switch_refusal(files, dirs)).into())
        }
        Ok(fs_cli::pipe::Response::Failed { kind, detail, .. }) => {
            Err(fs_cli::Failure::from_pipe_kind(&kind, detail).into())
        }
        Ok(other) => Err(format!("the mount answered {other:?}").into()),
        Err(error) => Err(fs_cli::Failure::in_use(format!(
            "the mount at {} is not answering ({error})",
            here.mount_point.display()
        ))
        .into()),
    }
}

// ------------------------------------------------------------------ helpers

fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn take_option(arguments: &mut Vec<String>, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    if let Some(index) = arguments.iter().position(|a| a.starts_with(&prefix)) {
        let value = arguments[index][prefix.len()..].to_owned();
        arguments.remove(index);
        return Some(value);
    }
    let index = arguments.iter().position(|a| a == name)?;
    arguments.remove(index);
    if index < arguments.len() {
        Some(arguments.remove(index))
    } else {
        None
    }
}
