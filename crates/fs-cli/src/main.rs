//! Command line interface for the Workspace CAS filesystem.
//!
//! Git-style and explicit: a *repository* is a directory, a *branch* is a named commit,
//! and a *workspace* is where edits live until `commit`. The CLI keeps exactly one piece
//! of state of its own — which workspace is current, in `<store>/cli/current` — because
//! that is a convenience of this entry point, not a library concept (ADR-006 §13).
//!
//! Every command opens what it needs, does one thing, and exits: there are no open
//! handles to leak and no session to corrupt.

mod mountcmd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fs_api::{ApiError, Repo, Workspace};

const USAGE: &str = "\
fs-cli [--store <dir>] <command> [args]

  init <path>                     create a repository (Store + scope + main branch)
  branch [<name>] [--from <src>]  list branches, or create one
  branch delete <name>            delete a branch (its history stays a GC root)
  checkout <branch>               make a workspace on a branch current
  workspace discard <name>        drop a workspace's overlay and row (its branch is untouched)

  put   <local> <path>            write a host file's bytes
  get   <path> [local]            read a file out (or to stdout)
  ls    [path]                    list the current workspace, or <commit>:[path]
  stat  <path>                    show metadata
  mkdir <path>                    create a directory
  rm    <path>                    remove a file or an empty directory
  mv    <from> <to>               rename
  cat   <commit>:<path>           read a file from history

  commit -m <message> [--force]   publish the workspace as a commit
  status                          current workspace, base and dirty state
  log   [limit]                   commit history of the current branch
  reset <commit> [--expect <c>]   move the branch head
  restore <commit> [--discard]    replace the view with a commit's tree
  revert                          undo the head with a new commit

  verify [--deep]                 check objects and references
  stats                           object and repository counts
  gc [--dry-run]                  collect unreachable objects (offline: refuses while a writer holds the store)
  mount <mount_point> [--workspace <name>] [--no-version-gate]
                                  serve a workspace over WinFsp until `umount` asks it to stop (store from --store)
  umount <mount_point> [--force]  ask a running mount to unmount (--force clears a stale record too)
  list                            the mounts recorded in %TEMP%/layerfs-mounts
  version                         the WinFsp DLL's version versus the layout this build was measured on
  snapshot [list | create | drop <root>]
                                  named roots that keep a tree alive without moving a branch";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("layerfs: {error}");
            // **Exit codes are part of the interface** (ADR-010 §8.3): a script has to tell "retry when the other process finishes" from "this command
            // is wrong". Everything that is not a `Failure` keeps the old answer, so this changes nothing that did not opt in.
            let code = error
                .downcast_ref::<fs_cli::Failure>()
                .map_or(1, |failure| failure.code);
            ExitCode::from(code)
        }
    }
}

/// The `--workspace` given on the command line, set once at startup.
///
/// Replaceable (ADR-010 §5): with `cli/current` gone, every command that needs a workspace takes it from the option, and a command that needs one
/// without having it fails with [`fs_cli::NO_WORKSPACE`] — never with a guessed name.
static WORKSPACE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn require_workspace() -> Result<String, Box<dyn std::error::Error>> {
    match WORKSPACE.get().and_then(Option::clone) {
        Some(name) => Ok(name),
        None => Err(fs_cli::Failure::usage(fs_cli::NO_WORKSPACE).into()),
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.is_empty() || arguments.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let explicit_store = take_option(&mut arguments, "--store").map(PathBuf::from);
    // Read **before** the command name is taken: `--workspace` is a global option (ADR-010 §5), not a subcommand flag.
    let workspace_option = take_option(&mut arguments, "--workspace");
    let command = arguments.remove(0);

    // **Path discovery happens here, once** (ADR-010 §4.2): a command run from inside a mount works without saying so, and a command that names both
    // explicitly keeps what it named. The three verbs that do not need a repository keep their place *before* this, so discovery costs them nothing.
    let needs_repository = !matches!(
        command.as_str(),
        "mount" | "umount" | "list" | "version" | "init" | "gc"
    ) && !fs_cli::FILE_COMMANDS.contains(&command.as_str());
    let (store, workspace_option) = if needs_repository {
        let current = std::env::current_dir().ok();
        let discovered = current.and_then(|cwd| {
            fs_cli::discovery::discover(&cwd, fs_cli::winpaths::volume_root, mountcmd::live_records)
        });
        let target = fs_cli::discovery::resolve(
            explicit_store.as_deref(),
            workspace_option.as_deref(),
            discovered.as_ref(),
        );
        (target.store, target.workspace)
    } else {
        (
            explicit_store.unwrap_or_else(|| PathBuf::from(".")),
            workspace_option,
        )
    };
    let _ = WORKSPACE.set(workspace_option.clone());

    // **The removed file commands answer before the store is touched** (A0c): with a `--store` that does not exist, "store does not exist" would be a
    // true sentence about the wrong problem — the command is gone, and that is what the caller has to be told.
    if let Some(removed) = fs_cli::FILE_COMMANDS
        .iter()
        .find(|name| **name == command.as_str())
    {
        return Err(fs_cli::Failure::usage(fs_cli::file_command_removed(removed)).into());
    }

    if command == "init" {
        let path = arguments
            .first()
            .map_or_else(|| store.clone(), PathBuf::from);
        let repo = Repo::create(&path)?;
        let head = repo.branch_head("main")?;
        // **`init` leaves the repository usable**, not merely created: a fresh store with no workspace makes the very next lifecycle command fail for a
        // reason the user cannot see. Creating `main` is the same thing `checkout main` does, done once at the moment it is obviously wanted. No
        // `cli/current` is written any more (ADR-010 §5): the workspace is named on the command line from now on.
        let workspace = std::sync::Arc::new(repo).workspace_create("main", "main")?;
        println!(
            "initialised {} (main = {}, workspace {})",
            path.display(),
            short(&head),
            workspace.name()
        );
        workspace.close()?;
        return Ok(());
    }

    // `gc` runs **before** a `Repo` is opened: the collector takes the store exclusively, and a live
    // `Repo` holds two connections to it, so opening first would make the collector refuse itself.
    if command == "gc" {
        return gc(&store, &arguments);
    }

    // The interactive entry points run **before** a `Repo` is opened: `mount` opens its own (it needs an `Arc` for the workspace), and
    // `list`/`umount`/`version` need no store at all — opening one first made them fail on a directory that is not a repository.
    match command.as_str() {
        "mount" => return mountcmd::mount(&store, workspace_option.as_deref(), &arguments),
        "umount" => return mountcmd::umount(&arguments),
        "list" => return mountcmd::list(),
        "version" => return mountcmd::version(),
        _ => {}
    }

    let repo = Repo::open(&store)?;
    match command.as_str() {
        "branch" => branch(&repo, &arguments),
        "checkout" => checkout(&repo, &store, &arguments),
        "snapshot" => snapshot(&repo, &store, &arguments),
        "workspace" => workspace(&repo, &arguments),
        "commit" => commit(&repo, &store, &arguments.clone()),
        "status" => with_workspace(&repo, &store, |workspace| {
            let status = workspace.status()?;
            println!("workspace: {}", status.name);
            println!("branch:    {}", status.branch);
            println!("base:      {}", short(&status.base_commit));
            println!("dirty:     {}", if status.dirty { "yes" } else { "no" });
            Ok(())
        }),
        "log" => log(&repo, &store, &arguments),
        "reset" => reset(&repo, &store, &arguments),
        "restore" => with_workspace(&repo, &store, |workspace| {
            let commit = one(&arguments, "restore <commit> [--discard]")?;
            let discard = arguments.iter().any(|a| a == "--discard");
            workspace.restore(&commit, discard)?;
            workspace.flush()?;
            println!("restored {} to {}", workspace.name(), commit);
            Ok(())
        }),
        "revert" => {
            let branch = branch_for(&repo, &arguments, "--branch")?;
            let info = repo.revert(&branch)?;
            println!("{}: revert {}", branch, info.id);
            Ok(())
        }
        "verify" => verify(&repo, &arguments),
        "stats" => {
            let stats = repo.stats()?;
            println!("objects:            {}", stats.objects);
            println!("branches:           {}", stats.branches);
            println!("commits:            {}", stats.commits);
            println!("workspaces:         {}", stats.workspaces);
            println!("snapshots:          {}", stats.snapshots);
            println!("branch_log entries: {}", stats.branch_log_entries);
            Ok(())
        }
        other => Err(format!("unknown command {other}; see --help").into()),
    }
}

// ------------------------------------------------------------------ commands

/// `gc [--dry-run]`: the offline collector.
///
/// Refuses while any writer or workspace holds the store, which is ADR-008 §6.1's whole point; the
/// error it prints is `StoreInUse` ("a maintainer is running, do not retry"), not `Busy`.
/// `snapshot [list | drop <root>]`.
///
/// A snapshot is a root that keeps a tree alive without moving a branch; dropping one is how a user
/// says "this tree may go", and the objects follow on the **next** collection rather than immediately.
/// `workspace discard <name>`: remove a workspace row and its overlay.
///
/// Needed before `branch delete`, which refuses while a workspace is still on the branch: deleting the
/// branch first would leave a workspace pointing at a name nobody has.
fn workspace(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        Some("discard") => {
            let name = arguments
                .get(1)
                .ok_or("expected workspace discard <name>")?;
            repo.workspace_discard(name)?;
            println!("discarded workspace {name}");
            Ok(())
        }
        other => Err(format!("unknown workspace action {other:?}").into()),
    }
}

fn snapshot(
    repo: &Repo,
    _store: &std::path::Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        None | Some("list") => {
            let rows = repo.snapshots()?;
            if rows.is_empty() {
                println!("no snapshots");
                return Ok(());
            }
            for row in rows {
                println!("{}	{}", row.root, row.workspace);
            }
            Ok(())
        }
        Some("create") => {
            let name = require_workspace()?;
            let mut workspace = repo.workspace_open(&name)?;
            let root = workspace.snapshot()?;
            workspace.close()?;
            println!("snapshot {root}");
            Ok(())
        }
        Some("drop") => {
            let root = arguments.get(1).ok_or("expected snapshot drop <root>")?;
            if repo.snapshot_drop(root)? {
                println!("dropped {root}: its objects stay until the next gc");
            } else {
                return Err(format!("no snapshot for {root}").into());
            }
            Ok(())
        }
        Some(other) => Err(format!("unknown snapshot action {other}").into()),
    }
}

fn gc(store: &std::path::Path, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let dry_run = arguments.iter().any(|argument| argument == "--dry-run");
    let summary = fs_api::gc(store, dry_run)?;
    println!(
        "gc: {} (repack_threshold {})",
        if summary.dry_run { "dry run" } else { "done" },
        summary.repack_threshold
    );
    println!("  marked objects:      {}", summary.marked_objects);
    println!("  freed objects:       {}", summary.freed_objects);
    if summary.dry_run {
        println!("  planned objects:     {}", summary.planned_objects);
    }
    println!("  freed bytes:         {}", summary.freed_object_bytes);
    println!("  reclaimed dead bytes:{}", summary.reclaimed_dead_bytes);
    println!(
        "  packs: {} deleted, {} repacked",
        summary.packs_deleted, summary.packs_repacked
    );
    for pack in &summary.missing_packs {
        println!("  warning: pack {pack} has a row but no file (not repaired: a missing file is a finding)");
    }
    Ok(())
}

fn branch(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.is_empty() {
        for branch in repo.branches()? {
            println!("{}\t{}", branch.name, branch.head);
        }
        return Ok(());
    }
    let name = &arguments[0];
    let from = arguments
        .iter()
        .position(|a| a == "--from")
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str);
    if name == "delete" {
        let victim = arguments.get(1).ok_or("expected branch delete <name>")?;
        repo.branch_delete(victim)?;
        println!("deleted branch {victim} (its commits remain roots)");
        return Ok(());
    }

    let created = repo.branch_create(name, from)?;
    println!("{} -> {}", created.name, short(&created.head));
    Ok(())
}

fn checkout(
    repo: &Repo,
    _store: &Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let branch = arguments.first().ok_or("expected checkout <branch>")?;
    // The workspace is named after its branch: one workspace per branch, and a second `checkout` of the same branch resumes the first.
    //
    // **Existence is asked of the workspace *list*, not of `workspace_open`** (A3b): opening is what a mount holds, so asking it would make `checkout dev`
    // fail exactly when `dev` is mounted — the case where the user most needs to be told it is already there and where.
    let existing = repo
        .workspaces()?
        .into_iter()
        .find(|workspace| &workspace.name == branch)
        .or_else(|| {
            repo.workspaces()
                .ok()?
                .into_iter()
                .find(|workspace| &workspace.branch == branch)
        });
    let Some(existing) = existing else {
        // No row: create it (the branch must exist, or `workspace_create` says so).
        let created = match repo.workspace_create(branch, branch) {
            Ok(workspace) => workspace,
            Err(error) => {
                return Err(fs_cli::Failure::not_found(format!(
                    "cannot create a workspace on {branch}: {error}; \
                     if that branch does not exist, create it with `fs-cli branch {branch} --from main`"
                ))
                .into())
            }
        };
        println!("on {} (workspace {})", created.branch(), created.name());
        created.close()?;
        return Ok(());
    };

    // The workspace exists. If it is **mounted**, say so and keep the mount: `checkout` is not a request to unmount anything (A3b).
    match mountcmd::live_mount_of(&existing.name) {
        Some((mount_point, pid)) => {
            println!(
                "on {} (workspace {}) — mounted at {mount_point} (pid {pid})",
                existing.branch, existing.name
            );
            Ok(())
        }
        None => {
            let workspace = repo.workspace_open(&existing.name)?;
            println!("on {} (workspace {})", workspace.branch(), workspace.name());
            workspace.close()?;
            Ok(())
        }
    }
}

fn commit(
    repo: &Repo,
    store: &Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut message = None;
    let mut force = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-m" => {
                message = arguments.get(index + 1).cloned();
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            other => return Err(format!("unexpected argument {other}; see --help").into()),
        }
    }
    let message = message.ok_or("expected commit -m <message>")?;
    with_workspace(repo, store, |workspace| {
        let id = workspace.commit(&message, force)?;
        println!("{}: {}", workspace.branch(), id);
        Ok(())
    })
}

fn log(repo: &Repo, _store: &Path, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // `log [branch] [n]`: with a branch named on the command line no overlay is opened (A1), so `log dev` works while `dev` is mounted.
    let named = arguments
        .first()
        .filter(|text| !text.starts_with('-') && text.parse::<usize>().is_err());
    let branch = match named {
        Some(branch) => branch.clone(),
        None => current_branch(repo)?,
    };
    let limit = arguments
        .iter()
        .find_map(|text| text.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    for info in repo.log(&branch)?.into_iter().take(limit) {
        println!("{} {}", info.id, info.message);
    }
    Ok(())
}

fn reset(
    repo: &Repo,
    _store: &Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let commit = one(arguments, "reset <commit> [--expect <c>]")?;
    let expect = arguments
        .iter()
        .position(|a| a == "--expect")
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str);
    let branch = branch_for(repo, arguments, "--branch")?;
    repo.reset(&branch, &commit, expect)?;
    println!("{branch} -> {commit}");
    Ok(())
}

fn verify(repo: &Repo, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let deep = arguments.iter().any(|a| a == "--deep");
    let report = repo.verify(deep)?;
    println!("objects:  {}", report.objects);
    println!("roots:    {}", report.roots_checked);
    if deep {
        println!("inodes:   {}", report.inodes_checked);
    }
    println!(
        "index:    {}",
        if report.index_clean { "ok" } else { "broken" }
    );
    for dangling in &report.dangling {
        println!("dangling: {} ({})", dangling.object, dangling.reason);
    }
    if report.is_clean() {
        println!("ok");
        Ok(())
    } else {
        Err(format!("{} problem(s) found", report.dangling.len()).into())
    }
}

// ------------------------------------------------------------------ plumbing

/// Runs `body` against the current workspace, opening and closing it around the call.
fn with_workspace(
    repo: &Repo,
    _store: &Path,
    body: impl FnOnce(&mut Workspace) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = require_workspace()?;
    let mut workspace = match repo.workspace_open(&name) {
        Ok(workspace) => workspace,
        Err(error) => return Err(open_refusal(&name, error, "open")),
    };
    let outcome = body(&mut workspace);
    workspace.close()?;
    outcome
}

/// Turns "cannot open this workspace" into a message that says **why** (A3, ADR-010 §8.2).
///
/// Two different situations hide behind one error: a workspace this machine is serving (there is a record with a mount point and a pid) and one held by a
/// process with no record (another `fs-cli`). The first tells the user which mount to unmount; the second says so plainly. Rendering both the same way would
/// lose the only part of the message that is actionable.
fn open_refusal(workspace: &str, error: ApiError, command: &str) -> Box<dyn std::error::Error> {
    if let Some((mount_point, pid)) = mountcmd::live_mount_of(workspace) {
        return fs_cli::Failure::in_use(fs_cli::mounted_refusal(
            command,
            workspace,
            &mount_point,
            pid,
        ))
        .into();
    }
    // Not found is not "in use": it keeps its own exit code and its own way out.
    if matches!(
        error,
        ApiError::Workspace(fs_workspace::WorkspaceError::NotFound { .. })
    ) {
        return fs_cli::Failure::not_found(format!(
            "no workspace {workspace}; `fs-cli checkout <branch>` creates one"
        ))
        .into();
    }
    // Anything else while such a workspace exists is the lock talking: another process has it open.
    fs_cli::Failure::in_use(fs_cli::IN_USE_ELSEWHERE).into()
}

/// The branch a command works on: **`--branch`, or the workspace's branch** (ADR-010 §13 A1/A1b).
///
/// With `--branch` no overlay is opened at all, so `log`/`reset`/`revert` keep working while another process has the workspace mounted. The same has to
/// be true of the fallback: `current_branch` reads the **workspace row** (via `workspaces()`), never the overlay, or `--workspace dev log` would fail
/// exactly when `dev` is in use — which is the case that matters (A1b).
fn branch_for(
    repo: &Repo,
    arguments: &[String],
    flag: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(index) = arguments.iter().position(|argument| argument == flag) {
        if let Some(name) = arguments.get(index + 1) {
            return Ok(name.clone());
        }
    }
    current_branch(repo)
}

fn current_branch(repo: &Repo) -> Result<String, Box<dyn std::error::Error>> {
    let name = require_workspace()?;
    // **The row, not the overlay** (A1b): opening the workspace would fail while it is mounted, and answering "which branch am I on" must not require
    // the lock that a mount holds. The row is what `workspace_list` reads.
    match repo.workspaces()?.into_iter().find(|row| row.name == name) {
        Some(row) => Ok(row.branch),
        None => Err(fs_cli::Failure::not_found(format!(
            "no workspace {name}; `fs-cli checkout <branch>` creates one"
        ))
        .into()),
    }
}

/// The workspace this command runs against: **the `--workspace` option, or nothing** (ADR-010 §5).
///
/// `cli/current` is gone: a file that remembers a name for you is a second source of truth, and it made every command's behaviour depend on a store
/// nobody mentioned. The `store` parameter stays only so the call sites read unchanged while the S0 sweep removes them one by one.
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

fn one(arguments: &[String], usage: &str) -> Result<String, Box<dyn std::error::Error>> {
    match arguments {
        [first, ..] if !first.starts_with("--") => Ok(first.clone()),
        _ => Err(format!("expected {usage}").into()),
    }
}
