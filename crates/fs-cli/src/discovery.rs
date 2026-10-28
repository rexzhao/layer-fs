//! Path discovery (ADR-010 §4.2/§4.4): which `(store, workspace)` a command without options should work on.
//!
//! Split in two on purpose. [`resolve`] is the **combination table** and has no side effects at all; [`discover`] performs exactly two queries and takes them
//! as arguments. That is what lets the seven rows of the table — and the "records that point at somebody else's volume" case — be tested without mounting
//! anything, which is the only way they can be tested cheaply enough to stay in the suite.

use std::path::{Path, PathBuf};

/// One mount record, as much of it as discovery needs (ADR-010 §9.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The repository that mount is serving, canonicalised at write time.
    pub store: PathBuf,
    /// The workspace it is serving.
    pub workspace: String,
    /// Where it is mounted.
    pub mount_point: PathBuf,
    /// The process serving it (used in messages, not in judging liveness).
    pub pid: u32,
    /// The branch that workspace publishes to.
    pub branch: String,
}

/// Whether a record's owner is still alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// The owner still holds the record file open.
    Live,
    /// The owner is gone; the record is a leftover.
    Stale,
}

/// The answer [`resolve`] gives: a store, and a workspace that may still be missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The repository to open.
    pub store: PathBuf,
    /// The workspace to work on, when one is known.
    pub workspace: Option<String>,
}

/// The volume label a mounted workspace carries; anything else is not ours (ADR-010 §4.4).
pub const WORKSPACE_VOLUME_LABEL: &str = "layerfs";

/// The combination rules of ADR-010 §4.2, as one function.
///
/// The rule that is easy to get wrong is row 4: an explicit `--store` that disagrees with the mount around the current directory **discards** what was
/// discovered. Mixing the two would invent a `(store, workspace)` pair that exists nowhere — the store from the flag and the workspace from a mount of a
/// different repository.
#[must_use]
pub fn resolve(
    explicit_store: Option<&Path>,
    explicit_workspace: Option<&str>,
    discovered: Option<&Record>,
) -> Target {
    match (explicit_store, explicit_workspace) {
        (Some(store), Some(workspace)) => Target {
            store: store.to_path_buf(),
            workspace: Some(workspace.to_owned()),
        },
        (Some(store), None) => match discovered {
            // Row 3: the flag agrees with the mount, so the mount's workspace is the obvious one.
            Some(record) if same_path(store, &record.store) => Target {
                store: store.to_path_buf(),
                workspace: Some(record.workspace.clone()),
            },
            // Row 4: the flag disagrees — **discard** the discovery rather than borrow a workspace from another repository.
            _ => Target {
                store: store.to_path_buf(),
                workspace: None,
            },
        },
        (None, Some(workspace)) => Target {
            // Rows 5/6: the workspace is explicit, the store comes from the mount when there is one.
            store: discovered.map_or_else(|| PathBuf::from("."), |record| record.store.clone()),
            workspace: Some(workspace.to_owned()),
        },
        (None, None) => match discovered {
            Some(record) => Target {
                store: record.store.clone(),
                workspace: Some(record.workspace.clone()),
            },
            None => Target {
                store: PathBuf::from("."),
                workspace: None,
            },
        },
    }
}

/// Whether two store paths are the same place: canonicalised, then compared as text (ADR-010 §4.2 explains why once is enough).
#[must_use]
pub fn same_path(left: &Path, right: &Path) -> bool {
    normalise(left) == normalise(right)
}

fn normalise(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.display().to_string();
    text.strip_prefix("\\\\?\\")
        .unwrap_or(&text)
        .to_ascii_lowercase()
}

/// Finds the mount whose volume contains `cwd` (**ADR-010 §4.4**).
///
/// Two queries, both injected: what volume root and label the current directory sits on, and which records exist. Nothing else — no poking at drive letters,
/// no guessing from `GetLogicalDrives`. The label is the safety net: a record that says it is mounted at `C:\` must not put a `git`-style command into a
/// "mounting context" on somebody's system drive, and the label is what makes that impossible rather than unlikely.
#[must_use]
pub fn discover(
    cwd: &Path,
    volume_root: impl Fn(&Path) -> Option<(PathBuf, String)>,
    records: impl Fn() -> Vec<(Record, Liveness)>,
) -> Option<Record> {
    let (_root, label) = volume_root(cwd)?;
    if !label.eq_ignore_ascii_case(WORKSPACE_VOLUME_LABEL) {
        return None;
    }
    records()
        .into_iter()
        .find(|(record, liveness)| {
            *liveness == Liveness::Live && cwd.starts_with(&record.mount_point)
        })
        .map(|(record, _)| record)
}

#[cfg(test)]
mod tests {
    use super::{discover, resolve, Liveness, Record, Target, WORKSPACE_VOLUME_LABEL};
    use std::path::{Path, PathBuf};

    fn record(mount_point: &str, store: &str, workspace: &str) -> Record {
        Record {
            store: PathBuf::from(store),
            workspace: workspace.to_owned(),
            mount_point: PathBuf::from(mount_point),
            pid: 4242,
            branch: workspace.to_owned(),
        }
    }

    /// **A6**: the seven rows of ADR-010 §4.2, as seven cases of the pure function.
    #[test]
    fn the_combination_table_has_seven_rows() {
        let mounted = record("U:", "C:/repo", "dev");
        let store = Path::new("C:/repo");
        let elsewhere = Path::new("C:/other");

        // 1. nothing explicit, inside a mount -> the mount's store and workspace.
        assert_eq!(
            resolve(None, None, Some(&mounted)),
            Target {
                store: PathBuf::from("C:/repo"),
                workspace: Some("dev".to_owned())
            }
        );
        // 2. nothing explicit, outside a mount -> "." and no workspace (the caller then refuses).
        assert_eq!(
            resolve(None, None, None),
            Target {
                store: PathBuf::from("."),
                workspace: None
            }
        );
        // 3. only `--store`, agreeing with the mount -> the mount's workspace.
        assert_eq!(
            resolve(Some(store), None, Some(&mounted)).workspace,
            Some("dev".to_owned())
        );
        // 4. only `--store`, **disagreeing** -> the discovery is discarded, not mixed.
        assert_eq!(
            resolve(Some(elsewhere), None, Some(&mounted)),
            Target {
                store: elsewhere.to_path_buf(),
                workspace: None
            },
            "cell row 4: a store that is not the mount's must not borrow its workspace"
        );
        // 5. only `--workspace`, inside a mount -> the mount's store.
        assert_eq!(
            resolve(None, Some("feature"), Some(&mounted)),
            Target {
                store: PathBuf::from("C:/repo"),
                workspace: Some("feature".to_owned())
            }
        );
        // 6. only `--workspace`, outside a mount -> "." with that workspace.
        assert_eq!(
            resolve(None, Some("feature"), None),
            Target {
                store: PathBuf::from("."),
                workspace: Some("feature".to_owned())
            }
        );
        // 7. both explicit -> explicit wins, discovery is not consulted at all.
        assert_eq!(
            resolve(Some(elsewhere), Some("feature"), Some(&mounted)),
            Target {
                store: elsewhere.to_path_buf(),
                workspace: Some("feature".to_owned())
            }
        );
    }

    /// **A6c**: a live record pointing at a volume whose label is not ours must not create a mounting context.
    #[test]
    fn a_record_on_another_volume_is_ignored() {
        let cwd = Path::new("C:\\work");
        let system_drive = record("C:\\", "C:/repo", "dev");
        assert_eq!(
            discover(
                cwd,
                |_| Some((PathBuf::from("C:\\"), "OS".to_owned())),
                || vec![(system_drive.clone(), Liveness::Live)]
            ),
            None,
            "cell A6c: the label, not the record, decides"
        );
        // The paired control: same record, our label -> it is found.
        assert_eq!(
            discover(
                cwd,
                |_| Some((PathBuf::from("C:\\"), WORKSPACE_VOLUME_LABEL.to_owned())),
                || vec![(system_drive.clone(), Liveness::Live)]
            )
            .map(|record| record.workspace),
            Some("dev".to_owned())
        );
    }

    /// A stale record is not a mounting context, and a record for another mount point is not either.
    #[test]
    fn only_a_live_record_over_the_current_directory_counts() {
        let cwd = Path::new("U:\\sub");
        let here = record("U:", "C:/repo", "dev");
        let other = record("V:", "C:/repo", "dev");
        let volume = |_: &Path| Some((PathBuf::from("U:\\"), WORKSPACE_VOLUME_LABEL.to_owned()));
        assert_eq!(
            discover(cwd, volume, || vec![(here.clone(), Liveness::Stale)]),
            None,
            "cell stale: a dead owner is not a mount"
        );
        assert_eq!(
            discover(cwd, volume, || vec![(other.clone(), Liveness::Live)]),
            None,
            "cell elsewhere: a different mount point does not contain this directory"
        );
        assert_eq!(
            discover(cwd, volume, || vec![(here.clone(), Liveness::Live)])
                .map(|record| record.workspace),
            Some("dev".to_owned())
        );
    }
}
