//! The reference model's supporting machinery: the model itself, the random driver, and the
//! full-tree comparison (ADR-006 §18-1).
//!
//! Split out of `reference_model.rs` so the test file reads as "what is checked" and this
//! file reads as "how".

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fs_api::{ApiError, Kind, Repo, Workspace};

// ------------------------------------------------------------------ the model

/// What a path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A directory.
    Dir,
    /// A regular file: its bytes.
    File(Vec<u8>),
}

/// Why an operation was refused, by category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The path, or a parent, does not exist.
    NotFound,
    /// The name is taken.
    AlreadyExists,
    /// A component that must be a directory is a file.
    NotADirectory,
    /// The operation needs a file but found a directory.
    IsADirectory,
    /// The directory is not empty.
    NotEmpty,
    /// A directory would move into its own subtree.
    IntoOwnSubtree,
    /// The root cannot be created, renamed or removed.
    IsRoot,
}

/// One step of a sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Create a directory.
    Mkdir(String),
    /// Create an empty file.
    Create(String),
    /// Write bytes at an offset.
    WriteAt {
        /// Target file.
        path: String,
        /// Where the write starts.
        offset: u64,
        /// Bytes to write.
        bytes: Vec<u8>,
    },
    /// Set a file's size.
    Truncate {
        /// Target file.
        path: String,
        /// New size.
        size: u64,
    },
    /// Rename or move.
    Rename {
        /// Current path.
        from: String,
        /// New path.
        to: String,
    },
    /// Remove a file.
    Unlink(String),
    /// Remove an empty directory.
    Rmdir(String),
}

impl Op {
    /// A short label for failure messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Op::Mkdir(_) => "mkdir",
            Op::Create(_) => "create",
            Op::WriteAt { .. } => "write_at",
            Op::Truncate { .. } => "truncate",
            Op::Rename { .. } => "rename",
            Op::Unlink(_) => "unlink",
            Op::Rmdir(_) => "rmdir",
        }
    }
}

/// The in-memory reference tree.
///
/// An independent implementation of the specification: it shares no code with
/// `fs_namespace::Rules`, so a disagreement between the two is a real signal rather than a
/// tautology.
#[derive(Debug, Clone, Default)]
pub struct Reference {
    nodes: BTreeMap<String, Node>,
}

impl Reference {
    /// An empty tree: just the root.
    #[must_use]
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_owned(), Node::Dir);
        Self { nodes }
    }

    /// Every path, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    /// The node at a path.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Node> {
        self.nodes.get(path)
    }

    /// The direct children of a directory, sorted.
    pub fn children<'a>(&'a self, dir: &'a str) -> impl Iterator<Item = String> + 'a {
        let prefix = if dir == "/" {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        self.nodes.keys().filter_map(move |path| {
            let rest = path.strip_prefix(&prefix)?;
            if rest.is_empty() || rest.contains('/') {
                None
            } else {
                Some(rest.to_owned())
            }
        })
    }

    /// Applies an operation, or reports why it is refused.
    ///
    /// # Errors
    ///
    /// [`Refusal`] with the category of the refusal.
    pub fn apply(&mut self, op: &Op) -> Result<(), Refusal> {
        match op {
            Op::Mkdir(path) => {
                if path == "/" {
                    return Err(Refusal::IsRoot);
                }
                self.parent_is_dir(path)?;
                if self.nodes.contains_key(path) {
                    return Err(Refusal::AlreadyExists);
                }
                self.nodes.insert(path.clone(), Node::Dir);
                Ok(())
            }
            Op::Create(path) => {
                if path == "/" {
                    return Err(Refusal::AlreadyExists);
                }
                self.parent_is_dir(path)?;
                if self.nodes.contains_key(path) {
                    return Err(Refusal::AlreadyExists);
                }
                self.nodes.insert(path.clone(), Node::File(Vec::new()));
                Ok(())
            }
            Op::WriteAt {
                path,
                offset,
                bytes,
            } => {
                let offset = usize::try_from(*offset).unwrap_or(usize::MAX);
                let Some(Node::File(existing)) = self.nodes.get_mut(path) else {
                    return Err(match self.nodes.get(path) {
                        Some(_) => Refusal::IsADirectory,
                        None => Refusal::NotFound,
                    });
                };
                let end = offset.saturating_add(bytes.len());
                if existing.len() < end {
                    existing.resize(end, 0);
                }
                if let Some(slice) = existing.get_mut(offset..end) {
                    slice.copy_from_slice(bytes);
                }
                Ok(())
            }
            Op::Truncate { path, size } => {
                let size = usize::try_from(*size).unwrap_or(usize::MAX);
                let Some(Node::File(existing)) = self.nodes.get_mut(path) else {
                    return Err(match self.nodes.get(path) {
                        Some(_) => Refusal::IsADirectory,
                        None => Refusal::NotFound,
                    });
                };
                existing.resize(size, 0);
                Ok(())
            }
            Op::Rename { from, to } => {
                if from == "/" || to == "/" {
                    return Err(Refusal::IsRoot);
                }
                let Some(node) = self.nodes.get(from).cloned() else {
                    return Err(Refusal::NotFound);
                };
                // Renaming a path to itself is a legal no-op — the name already refers to
                // exactly what the caller asked for — not a collision. The first run of this
                // suite pinned that down: the model said AlreadyExists, the workspace Ok.
                if from == to {
                    return Ok(());
                }
                // Order matters, and this suite's first run taught it: a *directory* moving
                // into its own subtree is R6's `IntoOwnSubtree`, but a *file* has no
                // subtree, so the same target is merely a path whose parent is not a
                // directory.
                if node == Node::Dir && is_under(to, from) {
                    return Err(Refusal::IntoOwnSubtree);
                }
                match parent_of(to).and_then(|parent| self.nodes.get(&parent).cloned()) {
                    None => return Err(Refusal::NotFound),
                    Some(Node::File(_)) => return Err(Refusal::NotADirectory),
                    Some(Node::Dir) => {}
                }
                // R3/R4/R5/R6: what an existing destination name means.
                //  - file over file: the file is replaced (R3);
                //  - cross-kind: never (R4/R5);
                //  - directory over an empty directory: replaced (R7);
                //  - directory over a non-empty one: refused (R6).
                if let Some(existing) = self.nodes.get(to).cloned() {
                    match (&node, &existing) {
                        (Node::File(_), Node::File(_)) => {}
                        (Node::Dir, Node::Dir) => {
                            if self.children(to).next().is_some() {
                                return Err(Refusal::NotEmpty);
                            }
                        }
                        _ => return Err(Refusal::AlreadyExists),
                    }
                    self.nodes.remove(to);
                }
                let moving: Vec<(String, Node)> = self
                    .nodes
                    .iter()
                    .filter(|(path, _)| is_under(path, from))
                    .map(|(path, node)| (path.clone(), node.clone()))
                    .collect();
                for (path, _) in &moving {
                    self.nodes.remove(path);
                }
                for (path, node) in moving {
                    let rest = path.strip_prefix(from).unwrap_or_default();
                    self.nodes.insert(format!("{to}{rest}"), node);
                }
                Ok(())
            }
            Op::Unlink(path) => match self.nodes.get(path) {
                None => Err(Refusal::NotFound),
                Some(Node::Dir) => Err(Refusal::IsADirectory),
                Some(Node::File(_)) => {
                    self.nodes.remove(path);
                    Ok(())
                }
            },
            Op::Rmdir(path) => {
                if path == "/" {
                    return Err(Refusal::IsRoot);
                }
                match self.nodes.get(path) {
                    None => Err(Refusal::NotFound),
                    Some(Node::File(_)) => Err(Refusal::NotADirectory),
                    Some(Node::Dir) => {
                        if self.children(path).next().is_some() {
                            Err(Refusal::NotEmpty)
                        } else {
                            self.nodes.remove(path);
                            Ok(())
                        }
                    }
                }
            }
        }
    }

    fn parent_is_dir(&self, path: &str) -> Result<(), Refusal> {
        let Some(parent) = parent_of(path) else {
            return Err(Refusal::IsRoot);
        };
        match self.nodes.get(&parent) {
            None => Err(Refusal::NotFound),
            Some(Node::File(_)) => Err(Refusal::NotADirectory),
            Some(Node::Dir) => Ok(()),
        }
    }
}

/// The parent of an absolute path; `None` for the root.
#[must_use]
pub fn parent_of(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_owned()),
        Some(index) => Some(path[..index].to_owned()),
        None => None,
    }
}

/// Whether `path` is `root` or inside it.
#[must_use]
pub fn is_under(path: &str, root: &str) -> bool {
    if path == root {
        return true;
    }
    let prefix = if root == "/" {
        "/".to_owned()
    } else {
        format!("{root}/")
    };
    path.starts_with(&prefix)
}

// ------------------------------------------------------------------ randomness

/// A small deterministic generator (splitmix64).
///
/// The workspace has no `rand` dependency and this test needs reproducibility, not
/// statistical quality: a failing seed must replay exactly.
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seeds the generator.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        }
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value below `bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(bound).unwrap_or(1)).unwrap_or(0)
    }

    /// A path that already exists, chosen from the model.
    pub fn existing(&mut self, reference: &Reference) -> String {
        let paths = reference.paths();
        let index = self.below(paths.len());
        paths.get(index).cloned().unwrap_or_else(|| "/".to_owned())
    }

    /// A fresh-looking name under `/`, to give operations something to bind.
    pub fn fresh_name(&mut self) -> String {
        format!("/n{}", self.next_u64() % 12)
    }

    /// A fresh name *inside* `dir`, so a rename target cannot accidentally produce a path
    /// with an empty segment.
    pub fn fresh_child(&mut self, dir: &str) -> String {
        if dir == "/" {
            format!("/m{}", self.next_u64() % 4)
        } else {
            format!("{dir}/m{}", self.next_u64() % 4)
        }
    }
}

/// Draws one operation.
///
/// The mix is deliberately biased towards operations that can fail: a sequence of only
/// successful calls would never exercise the refusal paths, which is half of what the
/// reference model is for.
#[must_use]
pub fn random_op(rng: &mut Rng, reference: &Reference) -> Op {
    let existing = rng.existing(reference);
    let name = rng.fresh_name();
    // A rename target must be a syntactically legal path: either a fresh top-level name, or
    // a fresh name inside the source itself (which is what exercises R6/IntoOwnSubtree).
    let rename_target = if rng.below(2) == 0 {
        name.clone()
    } else {
        rng.fresh_child(&existing)
    };
    let offset = u64::try_from(rng.below(200)).unwrap_or(0);
    let size = u64::try_from(rng.below(300)).unwrap_or(0);
    let length = 1 + rng.below(64);
    let fill = u8::try_from(rng.below(251)).unwrap_or(0);
    match rng.below(7) {
        0 => Op::Mkdir(name),
        1 => Op::Create(name),
        2 => Op::WriteAt {
            path: existing,
            offset,
            bytes: vec![fill; length],
        },
        3 => Op::Truncate {
            path: existing,
            size,
        },
        4 => Op::Rename {
            from: existing,
            to: rename_target,
        },
        5 => Op::Unlink(existing),
        _ => Op::Rmdir(existing),
    }
}

// ------------------------------------------------------------------ the driver

/// Runs one operation against the workspace.
///
/// # Errors
///
/// Whatever the workspace refused with.
pub fn perform(workspace: &Workspace, op: &Op) -> Result<(), ApiError> {
    match op {
        Op::Mkdir(path) => workspace.mkdir(path),
        Op::Create(path) => workspace.create(path),
        Op::WriteAt {
            path,
            offset,
            bytes,
        } => workspace.write_at(path, *offset, bytes).map(|_| ()),
        Op::Truncate { path, size } => workspace.truncate(path, *size),
        Op::Rename { from, to } => workspace.rename(from, to),
        Op::Unlink(path) => match workspace.stat(path)? {
            stat if stat.kind == Kind::File => workspace.remove(path),
            _ => Err(not_unlinkable(path)),
        },
        Op::Rmdir(path) => match workspace.stat(path)? {
            stat if stat.kind == Kind::Directory => workspace.remove(path),
            _ => Err(not_removable(path)),
        },
    }
}

fn not_unlinkable(path: &str) -> ApiError {
    ApiError::Namespace(fs_namespace::NamespaceError::IsADirectory {
        path: path.to_owned(),
    })
}

fn not_removable(path: &str) -> ApiError {
    ApiError::Namespace(fs_namespace::NamespaceError::NotADirectory {
        path: path.to_owned(),
    })
}

/// **The check that matters**: the whole tree, every step.
///
/// Every path the model knows must have the same kind, size and bytes in the workspace, and
/// every directory must list the same names. Paths the model does not know must not be
/// reachable.
pub fn assert_tree_matches(
    workspace: &Workspace,
    reference: &Reference,
    seed: u64,
    step: usize,
    op: &Op,
) {
    let context = format!("seed={seed} step={step} op={} {op:?}", op.label());

    for path in reference.paths() {
        let expected = reference.get(&path).expect("a path that exists");
        let stat = match workspace.stat(&path) {
            Ok(stat) => stat,
            Err(error) => panic!("{context}: {path} must exist, got {error:?}"),
        };
        match expected {
            Node::Dir => {
                assert_eq!(stat.kind, Kind::Directory, "{context}: {path} kind");
                let mut actual: Vec<String> = workspace
                    .read_dir(&path)
                    .unwrap_or_else(|error| panic!("{context}: read_dir {path}: {error:?}"))
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect();
                actual.sort();
                let expected_names: Vec<String> = reference.children(&path).collect();
                assert_eq!(actual, expected_names, "{context}: {path} children");
            }
            Node::File(bytes) => {
                assert_eq!(stat.kind, Kind::File, "{context}: {path} kind");
                assert_eq!(
                    stat.size,
                    u64::try_from(bytes.len()).unwrap_or(0),
                    "{context}: {path} size"
                );
                let mut buffer = vec![0u8; bytes.len()];
                let read = workspace
                    .read_at(&path, 0, &mut buffer)
                    .unwrap_or_else(|error| panic!("{context}: read {path}: {error:?}"));
                buffer.truncate(read);
                assert_eq!(&buffer, bytes, "{context}: {path} bytes");
            }
        }
    }
}

// ------------------------------------------------------------------ fixture

/// A temp directory that cleans itself up.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(seed: u64) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::var_os("WCAS_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "fs-api-reference-{seed}-{}-{sequence}",
                std::process::id()
            ));
        std::fs::create_dir_all(&path).expect("temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A repository in a fresh directory, kept alive for the length of one sequence.
pub struct Fixture {
    /// The repository, opened for the sequence.
    pub repo: Repo,
    _root: TempRoot,
}

impl Fixture {
    /// Creates a fresh repository for `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let root = TempRoot::new(seed);
        let repo = Repo::create(root.path.join("store")).expect("create repository");
        Self { repo, _root: root }
    }
}
