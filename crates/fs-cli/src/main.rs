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

use std::io::{Read, Write};
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
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.is_empty() || arguments.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let store = PathBuf::from(take_option(&mut arguments, "--store").unwrap_or_else(|| ".".into()));
    let command = arguments.remove(0);

    if command == "init" {
        let path = arguments
            .first()
            .map_or_else(|| store.clone(), PathBuf::from);
        let repo = Repo::create(&path)?;
        let head = repo.branch_head("main")?;
        // **`init` leaves the repository usable**, not merely created: a fresh store with no workspace makes the very next `put`/`ls` fail with
        // "no current workspace", which is a step the user cannot see a reason for. Creating `main` and recording it is the same thing `checkout
        // main` does, done once at the moment it is obviously wanted.
        let workspace = std::sync::Arc::new(repo).workspace_create("main", "main")?;
        let name = workspace.name().to_owned();
        workspace.close()?;
        write_current(&path, &name)?;
        println!(
            "initialised {} (main = {}, workspace {name})",
            path.display(),
            short(&head)
        );
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
        "mount" => return mountcmd::mount(&store, &arguments),
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
        "put" => with_workspace(&repo, &store, |workspace| {
            let (local, path) = two(&arguments, "put <local> <path>")?;
            let mut bytes = Vec::new();
            std::fs::File::open(&local)?.read_to_end(&mut bytes)?;
            if workspace.stat(&path).is_err() {
                workspace.create(&path)?;
            }
            workspace.write_at(&path, 0, &bytes)?;
            workspace.truncate(&path, u64::try_from(bytes.len()).unwrap_or(0))?;
            workspace.flush()?;
            println!("{path} <- {local} ({} bytes)", bytes.len());
            Ok(())
        }),
        "get" => with_workspace(&repo, &store, |workspace| {
            let (path, target) = one_or_two(&arguments, "get <path> [local]")?;
            let bytes = read_all(workspace, &path)?;
            match target {
                Some(target) => {
                    std::fs::write(&target, &bytes)?;
                    println!("{path} ({} bytes) -> {target}", bytes.len());
                }
                None => std::io::stdout().write_all(&bytes)?,
            }
            Ok(())
        }),
        "ls" => {
            // `ls <commit>:[path]` reads history; `ls [path]` reads the workspace.
            let spec = arguments.first().map_or("", String::as_str);
            match split_commit(spec) {
                Some((commit, path)) => {
                    let view = repo.read_view(&commit)?;
                    print_entries(&view.read_dir(&path)?);
                    Ok(())
                }
                None => with_workspace(&repo, &store, |workspace| {
                    let path = arguments.first().map_or("/", String::as_str);
                    print_entries(&workspace.read_dir(path)?);
                    Ok(())
                }),
            }
        }
        "cat" => {
            let spec = arguments.first().ok_or("expected cat <commit>:<path>")?;
            let (commit, path) = split_commit(spec).ok_or("expected cat <commit>:<path>")?;
            let view = repo.read_view(&commit)?;
            std::io::stdout().write_all(&view.read_all(&path)?)?;
            Ok(())
        }
        "stat" => with_workspace(&repo, &store, |workspace| {
            let (path, _) = one_or_two(&arguments, "stat <path>")?;
            print_stat(&path, &workspace.stat(&path)?);
            Ok(())
        }),
        "mkdir" => with_workspace(&repo, &store, |workspace| {
            let path = one(&arguments, "mkdir <path>")?;
            workspace.mkdir(&path)?;
            workspace.flush()?;
            Ok(())
        }),
        "rm" => with_workspace(&repo, &store, |workspace| {
            let path = one(&arguments, "rm <path>")?;
            workspace.remove(&path)?;
            workspace.flush()?;
            Ok(())
        }),
        "mv" => with_workspace(&repo, &store, |workspace| {
            let (from, to) = two(&arguments, "mv <from> <to>")?;
            workspace.rename(&from, &to)?;
            workspace.flush()?;
            Ok(())
        }),
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
            let branch = current_branch(&repo, &store)?;
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
    store: &std::path::Path,
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
            let name = read_current(store)?;
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
    store: &Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let branch = arguments.first().ok_or("expected checkout <branch>")?;
    // The workspace is named after its branch: one current workspace per branch, and a
    // second `checkout` of the same branch resumes the first.
    let workspace = match repo.workspace_open(branch) {
        Ok(workspace) => workspace,
        Err(ApiError::Workspace(fs_workspace::WorkspaceError::NotFound { .. })) => {
            repo.workspace_create(branch, branch)?
        }
        Err(error) => return Err(error.into()),
    };
    write_current(store, workspace.name())?;
    println!("on {} (workspace {})", workspace.branch(), workspace.name());
    Ok(())
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

fn log(repo: &Repo, store: &Path, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let branch = current_branch(repo, store)?;
    let limit = arguments
        .first()
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    for info in repo.log(&branch)?.into_iter().take(limit) {
        println!("{} {}", info.id, info.message);
    }
    Ok(())
}

fn reset(
    repo: &Repo,
    store: &Path,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let commit = one(arguments, "reset <commit> [--expect <c>]")?;
    let expect = arguments
        .iter()
        .position(|a| a == "--expect")
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str);
    let branch = current_branch(repo, store)?;
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
    store: &Path,
    body: impl FnOnce(&mut Workspace) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = read_current(store)?;
    let mut workspace = repo.workspace_open(&name)?;
    let outcome = body(&mut workspace);
    workspace.close()?;
    outcome
}

fn current_branch(repo: &Repo, store: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let name = read_current(store)?;
    Ok(repo.workspace_open(&name)?.branch().to_owned())
}

fn current_path(store: &Path) -> PathBuf {
    store.join("cli").join("current")
}

fn read_current(store: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(current_path(store))
        .map_err(|_| "no current workspace; run checkout <branch>")?;
    let name = text.trim();
    if name.is_empty() {
        return Err("no current workspace; run checkout <branch>".into());
    }
    Ok(name.to_owned())
}

fn write_current(store: &Path, workspace: &str) -> std::io::Result<()> {
    let path = current_path(store);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{workspace}\n"))
}

fn read_all(workspace: &Workspace, path: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let size = usize::try_from(workspace.stat(path)?.size).unwrap_or(usize::MAX);
    let mut bytes = vec![0u8; size];
    let read = workspace.read_at(path, 0, &mut bytes)?;
    bytes.truncate(read);
    Ok(bytes)
}

fn print_entries(entries: &[fs_api::Entry]) {
    for entry in entries {
        println!("{:?}\t{}", entry.kind, entry.name);
    }
}

fn print_stat(path: &str, stat: &fs_api::Stat) {
    println!("path:      {path}");
    println!("kind:      {:?}", stat.kind);
    println!("id:        {}", stat.id.serial());
    println!("size:      {}", stat.size);
    println!("created:   {}", stat.created_ns);
    println!("modified:  {}", stat.modified_ns);
}

/// Splits `<commit>:<path>`; `None` when the argument is a plain path.
fn split_commit(spec: &str) -> Option<(String, String)> {
    let (commit, path) = spec.split_once(':')?;
    if commit.is_empty() {
        return None;
    }
    let path = if path.is_empty() { "/" } else { path };
    Some((commit.to_owned(), path.to_owned()))
}

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

fn two(arguments: &[String], usage: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    match arguments {
        [first, second, ..] => Ok((first.clone(), second.clone())),
        _ => Err(format!("expected {usage}").into()),
    }
}

fn one_or_two(
    arguments: &[String],
    usage: &str,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    match arguments {
        [first] => Ok((first.clone(), None)),
        [first, second, ..] => Ok((first.clone(), Some(second.clone()))),
        _ => Err(format!("expected {usage}").into()),
    }
}
