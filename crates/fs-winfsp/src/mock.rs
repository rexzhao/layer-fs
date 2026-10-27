//! An in-memory `WorkspaceOps` backend for the contract tests (P3-3a card §1, "mock.rs").
//!
//! The mock is not a stub: it keeps a real `(parent, name) -> child` binding table, allocates **fresh, never reused**
//! file ids, and updates bindings on rename. §3.9's path-reuse trick only reds a name-based implementation if the mock
//! behaves like a namespace, so a mock that "just remembers the last path" would turn the sharpest criterion in the card
//! into a formality.
//!
//! What it deliberately does **not** do: persist anything, model handles, share bits, or `DELETE_ON_CLOSE`. Those are
//! adapter policy (card §2) and are tested through the adapter's own handle table, not here.
//!
//! Timestamps come from a logical clock that only moves when the test asks it to ([`MockBackend::tick`]). Wall-clock and
//! randomness are excluded on purpose: a criterion that depends on them cannot be replayed when it fails.

use std::collections::BTreeMap;
use std::sync::Mutex;

use fs_semantics::disposition::Disposition;

use crate::ops::{
    Attributes, DirEntry, Kind, NodeInfo, OpenOutcome, OpsError, OpsResult, RenameOutcome, Times,
    WorkspaceOps,
};

/// The root directory's id.
///
/// **0**, matching `fs_namespace::FileId::ROOT`. It used to be 1, which was a mock-only choice — and it made every
/// scenario that passes `ROOT` as `parent` **wrong against the real backend**, silently: the scenario would ask the core
/// about serial 1 (the first *file*) and get `NotADirectory`. The trait-level constant has to agree with the core, because
/// the scenarios are shared.
pub const ROOT: u64 = 0;

/// One call the adapter made, in order.
///
/// Recorded rather than counted: M12's criteria are about **orderings** (`remove_at` after the last close,
/// `rename` before `remove_at`), and a bare counter cannot express "before".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `create_or_open`.
    CreateOrOpen(Vec<u8>, Disposition),
    /// `read`.
    Read(u64, u64, usize),
    /// `write`.
    Write(u64, u64, usize),
    /// `set_size`.
    SetSize(u64, u64),
    /// `read_dir`.
    ReadDir(u64),
    /// `get_info`.
    GetInfo(u64),
    /// `rename`.
    Rename(Vec<u8>, Vec<u8>, bool),
    /// `remove_at`.
    RemoveAt(u64, Vec<u8>, u64),
    /// `flush`.
    Flush,
}

#[derive(Clone, Debug)]
struct Node {
    kind: Kind,
    content: Vec<u8>,
    times: Times,
    attributes: Attributes,
    live: bool,
}

#[derive(Debug)]
struct Inner {
    nodes: BTreeMap<u64, Node>,
    children: BTreeMap<(u64, Vec<u8>), u64>,
    next_id: u64,
    clock: i64,
    calls: Vec<Call>,
    flush_count: usize,
}

/// An in-memory namespace that implements [`WorkspaceOps`].
#[derive(Debug)]
pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn split(path: &[u8]) -> OpsResult<Vec<Vec<u8>>> {
    if path.first() != Some(&b'/') {
        return Err(OpsError::InvalidParameter);
    }
    // **The root is a legal path with no components.** Refusing `/` here forced every caller that can name a directory to carry its own root
    // special case: `create_or_open` had one, `open_dir` did not — which is why `stat` on the volume root failed while every other operation on the
    // same path worked. One answer at the entry point instead of N special cases downstream.
    if path == b"/" {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for component in path[1..].split(|byte| *byte == b'/') {
        if component.is_empty() {
            // A trailing slash is a malformed path here: the adapter normalises before calling, and accepting it would
            // hide a normalisation bug behind a lenient mock.
            return Err(OpsError::InvalidParameter);
        }
        out.push(component.to_vec());
    }
    Ok(out)
}

impl MockBackend {
    /// A backend whose root directory exists and whose clock starts at 0.
    #[must_use]
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT,
            Node {
                kind: Kind::Directory,
                content: Vec::new(),
                times: Times::default(),
                attributes: Attributes(u32::from(0x10_u8)), // FILE_ATTRIBUTE_DIRECTORY
                live: true,
            },
        );
        Self {
            inner: Mutex::new(Inner {
                nodes,
                children: BTreeMap::new(),
                next_id: ROOT + 1,
                clock: 0,
                calls: Vec::new(),
                flush_count: 0,
            }),
        }
    }

    /// Advances the logical clock by `nanos`, so a test can assert that a timestamp changed **without** sleeping.
    pub fn tick(&self, nanos: i64) {
        self.inner.lock().expect("mock lock").clock += nanos;
    }

    /// Every call so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().expect("mock lock").calls.clone()
    }

    /// Forgets the recorded calls (counters keep their values).
    pub fn clear_calls(&self) {
        self.inner.lock().expect("mock lock").calls.clear();
    }

    /// How many times `flush` was called.
    #[must_use]
    pub fn flush_count(&self) -> usize {
        self.inner.lock().expect("mock lock").flush_count
    }

    /// Creates a directory (a mock-only helper: `WorkspaceOps` has no `mkdir`, because the adapter creates directories
    /// through `create_or_open` with a kind hint).
    ///
    /// # Errors
    ///
    /// [`OpsError::NameCollision`] when the name is taken, [`OpsError::NotFound`] for a missing parent.
    pub fn mkdir(&self, path: &[u8]) -> OpsResult<u64> {
        let mut inner = self.inner.lock().expect("mock lock");
        let clock = inner.clock;
        let components = split(path)?;
        let (parent, name) = parent_and_name(&inner, &components)?;
        if inner.children.contains_key(&(parent, name.clone())) {
            return Err(OpsError::NameCollision);
        }
        let id = inner.allocate(Kind::Directory, clock);
        inner.children.insert((parent, name), id);
        Ok(id)
    }

    /// Resolves a path to an id, or `None`.
    #[must_use]
    pub fn resolve(&self, path: &[u8]) -> Option<u64> {
        let inner = self.inner.lock().expect("mock lock");
        let components = split(path).ok()?;
        let mut current = ROOT;
        for component in components {
            current = *inner.children.get(&(current, component))?;
        }
        Some(current)
    }

    /// Whether an id is still live (not retired).
    #[must_use]
    pub fn is_live(&self, id: u64) -> bool {
        self.inner
            .lock()
            .expect("mock lock")
            .nodes
            .get(&id)
            .is_some_and(|node| node.live)
    }

    /// Directory entries as raw names, unsorted (the mock does not sort: the adapter owns ordering).
    #[must_use]
    pub fn raw_names(&self, dir: u64) -> Vec<Vec<u8>> {
        let inner = self.inner.lock().expect("mock lock");
        inner
            .children
            .keys()
            .filter(|(parent, _)| *parent == dir)
            .map(|(_, name)| name.clone())
            .collect()
    }
}

/// Resolves the parent of `components` and returns it with the final name.
fn parent_and_name(inner: &Inner, components: &[Vec<u8>]) -> OpsResult<(u64, Vec<u8>)> {
    let Some((name, parents)) = components.split_last() else {
        return Err(OpsError::InvalidParameter);
    };
    let mut current = ROOT;
    for component in parents {
        let next = inner
            .children
            .get(&(current, component.clone()))
            .copied()
            .ok_or(OpsError::NotFound)?;
        match inner.nodes.get(&next).map(|node| node.kind) {
            Some(Kind::Directory) => current = next,
            Some(Kind::File) => return Err(OpsError::NotADirectory),
            None => return Err(OpsError::NotFound),
        }
    }
    Ok((current, name.clone()))
}

impl Inner {
    fn allocate(&mut self, kind: Kind, clock: i64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.insert(
            id,
            Node {
                kind,
                content: Vec::new(),
                times: Times {
                    created: clock,
                    accessed: clock,
                    written: clock,
                    changed: clock,
                },
                attributes: Attributes(match kind {
                    Kind::Directory => u32::from(0x10_u8),
                    Kind::File => u32::from(0x80_u8), // FILE_ATTRIBUTE_NORMAL
                }),
                live: true,
            },
        );
        id
    }

    fn retire(&mut self, id: u64) {
        if let Some(node) = self.nodes.get_mut(&id) {
            node.live = false;
            node.content.clear();
        }
        // Retiring an id also drops every name bound to it: a tombstone must not be reachable by name.
        self.children.retain(|_, child| *child != id);
    }

    fn live_node(&self, id: u64) -> OpsResult<&Node> {
        self.nodes
            .get(&id)
            .filter(|node| node.live)
            .ok_or(OpsError::NotFound)
    }

    fn outcome(
        &self,
        id: u64,
        action: fs_semantics::disposition::Action,
        replaced: Option<u64>,
    ) -> OpenOutcome {
        let node = self.nodes.get(&id).expect("just allocated");
        let parent = self
            .children
            .iter()
            .find(|(_, child)| **child == id)
            .map(|((parent, _), _)| *parent)
            .unwrap_or(ROOT);
        let name = self
            .children
            .iter()
            .find(|(_, child)| **child == id)
            .map(|((_, name), _)| name.clone())
            .unwrap_or_default();
        OpenOutcome {
            file_id: id,
            kind: node.kind,
            action,
            size: node.content.len() as u64,
            times: node.times,
            attributes: node.attributes,
            binding: (parent, name),
            replaced,
        }
    }
}

impl WorkspaceOps for MockBackend {
    fn open_dir(&self, path: &[u8]) -> OpsResult<u64> {
        // Resolve first, then check the kind: a file at that name is `NotADirectory`, not "not found" — the two must stay distinguishable, and
        // both backends answer the same way (the shared contract asserts it).
        let id = MockBackend::resolve(self, path).ok_or(OpsError::NotFound)?;
        let inner = self.inner.lock().expect("mock lock");
        let kind = inner.live_node(id)?.kind;
        if kind != Kind::Directory {
            return Err(OpsError::NotADirectory);
        }
        Ok(id)
    }

    fn mkdir(&self, path: &[u8]) -> OpsResult<u64> {
        MockBackend::mkdir(self, path)
    }

    fn create_or_open(&self, path: &[u8], disposition: Disposition) -> OpsResult<OpenOutcome> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner
            .calls
            .push(Call::CreateOrOpen(path.to_vec(), disposition));
        let clock = inner.clock;
        let components = split(path)?;
        // **The root is answered here, once.** It has no parent segment, so `parent_and_name` cannot place it — and a caller that merely opens
        // the volume (`FILE_OPEN` on `\`) must succeed, or every later operation on that path dies with "path not found". A creating disposition
        // on the root is a collision: the root always exists.
        if components.is_empty() {
            return match disposition {
                Disposition::Open | Disposition::OpenIf => {
                    let node = inner.nodes.get(&ROOT).ok_or(OpsError::NotFound)?;
                    Ok(OpenOutcome {
                        file_id: ROOT,
                        kind: node.kind,
                        action: fs_semantics::disposition::Action::Open,
                        size: node.content.len() as u64,
                        times: node.times,
                        attributes: node.attributes,
                        binding: (ROOT, Vec::new()),
                        replaced: None,
                    })
                }
                _ => Err(OpsError::NameCollision),
            };
        }
        let (parent, name) = parent_and_name(&inner, &components)?;
        use fs_semantics::disposition::Action;

        let existing = inner.children.get(&(parent, name.clone())).copied();
        let kind = existing.and_then(|id| inner.nodes.get(&id).map(|node| node.kind));
        // The disposition table (`fs-semantics`) decides; the mock only carries it out. `Failure` outcomes become typed
        // errors, which is the one place this mock adds judgement of its own.
        let (id, action, replaced) = match fs_semantics::disposition::decide(
            disposition,
            match kind {
                None => fs_semantics::disposition::Exists::Absent,
                Some(Kind::File) => fs_semantics::disposition::Exists::File,
                Some(Kind::Directory) => fs_semantics::disposition::Exists::Directory,
            },
        ) {
            fs_semantics::disposition::Outcome::Failed(failure) => {
                return Err(match failure {
                    fs_semantics::disposition::Failure::NameCollision => OpsError::NameCollision,
                    fs_semantics::disposition::Failure::LeafMissing
                    | fs_semantics::disposition::Failure::ParentMissing => OpsError::NotFound,
                    fs_semantics::disposition::Failure::NotADirectory => OpsError::NotADirectory,
                    fs_semantics::disposition::Failure::IsADirectory => OpsError::IsADirectory,
                    fs_semantics::disposition::Failure::DirectoryNotEmpty => OpsError::NotEmpty,
                    fs_semantics::disposition::Failure::AccessDenied => OpsError::ReadOnly,
                    fs_semantics::disposition::Failure::InvalidName
                    | fs_semantics::disposition::Failure::InvalidParameter => {
                        OpsError::InvalidParameter
                    }
                    other => OpsError::Other(format!("{other:?}")),
                })
            }
            fs_semantics::disposition::Outcome::Opened(action) => match action {
                // SUPERSEDE shares the truncate arm (ADR-009 §4.2 ruling (a)): same id, content dropped, attributes
                // replaced. A handle opened before it therefore reads the **overwritten** content, because on a real
                // system both dispositions arrive as an overwrite of the same file object.
                Action::TruncateExisting => {
                    let id = existing.expect("truncate implies an existing entry");
                    let node = inner.nodes.get_mut(&id).expect("existing");
                    node.content.clear();
                    // The **only** remaining difference between the dispositions that share this action
                    // (ADR-009 §4.2 ruling (a), `ReplaceFileAttributes`):
                    //   SUPERSEDE ⇒ replace the stored attributes wholesale (they go back to "nothing set");
                    //   OVERWRITE / OVERWRITE_IF ⇒ keep them (the content is dropped, the metadata is not).
                    // A backend that got this backwards would be invisible to every FileId/内容 assertion.
                    if disposition == Disposition::Supersede {
                        node.attributes = crate::ops::Attributes(0);
                    }
                    node.times.written = clock;
                    node.times.changed = clock;
                    (id, action, None)
                }
                Action::CreateNew | Action::Open => match existing {
                    Some(id) => (id, action, None),
                    None => {
                        let id = inner.allocate(Kind::File, clock);
                        inner.children.insert((parent, name.clone()), id);
                        (id, action, None)
                    }
                },
            },
        };
        Ok(inner.outcome(id, action, replaced))
    }

    fn read(&self, file_id: u64, offset: u64, len: usize) -> OpsResult<Vec<u8>> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::Read(file_id, offset, len));
        let node = inner.live_node(file_id)?;
        if node.kind == Kind::Directory {
            return Err(OpsError::IsADirectory);
        }
        let mut out = vec![0u8; len];
        let start = usize::try_from(offset).map_err(|_| OpsError::OutOfRange)?;
        if start < node.content.len() {
            let available = (node.content.len() - start).min(len);
            out[..available].copy_from_slice(&node.content[start..start + available]);
        }
        Ok(out)
    }

    fn write(&self, file_id: u64, offset: u64, bytes: &[u8]) -> OpsResult<usize> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::Write(file_id, offset, bytes.len()));
        let clock = inner.clock;
        let node = inner.nodes.get_mut(&file_id).ok_or(OpsError::NotFound)?;
        if !node.live {
            return Err(OpsError::NotFound);
        }
        if node.kind == Kind::Directory {
            return Err(OpsError::IsADirectory);
        }
        let start = usize::try_from(offset).map_err(|_| OpsError::OutOfRange)?;
        let end = start.checked_add(bytes.len()).ok_or(OpsError::OutOfRange)?;
        if node.content.len() < end {
            node.content.resize(end, 0);
        }
        node.content[start..end].copy_from_slice(bytes);
        node.times.written = clock;
        node.times.changed = clock;
        Ok(bytes.len())
    }

    fn set_size(&self, file_id: u64, len: u64) -> OpsResult<()> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::SetSize(file_id, len));
        let clock = inner.clock;
        let node = inner.nodes.get_mut(&file_id).ok_or(OpsError::NotFound)?;
        if !node.live {
            return Err(OpsError::NotFound);
        }
        if node.kind == Kind::Directory {
            return Err(OpsError::IsADirectory);
        }
        let new_len = usize::try_from(len).map_err(|_| OpsError::OutOfRange)?;
        node.content.resize(new_len, 0);
        node.times.written = clock;
        node.times.changed = clock;
        Ok(())
    }

    fn read_dir(&self, dir_id: u64) -> OpsResult<Vec<DirEntry>> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::ReadDir(dir_id));
        let node = inner.live_node(dir_id)?;
        if node.kind == Kind::File {
            return Err(OpsError::NotADirectory);
        }
        let mut entries = Vec::new();
        for ((parent, name), child) in &inner.children {
            if *parent != dir_id {
                continue;
            }
            let child_node = inner.nodes.get(child).ok_or(OpsError::NotFound)?;
            if !child_node.live {
                continue;
            }
            entries.push(DirEntry {
                name: name.clone(),
                kind: child_node.kind,
                // The id the backend would answer `get_info` with for this entry — the field's whole purpose (367).
                file_id: *child,
            });
        }
        Ok(entries)
    }

    fn get_info(&self, file_id: u64) -> OpsResult<NodeInfo> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::GetInfo(file_id));
        let node = inner.live_node(file_id)?;
        Ok(NodeInfo {
            file_id,
            kind: node.kind,
            size: node.content.len() as u64,
            times: node.times,
            attributes: node.attributes,
        })
    }

    fn set_times(
        &self,
        file_id: u64,
        accessed: Option<i64>,
        written: Option<i64>,
        changed: Option<i64>,
        created: Option<i64>,
    ) -> OpsResult<()> {
        let mut inner = self.inner.lock().expect("mock lock");
        let node = inner.nodes.get_mut(&file_id).ok_or(OpsError::NotFound)?;
        if !node.live {
            return Err(OpsError::NotFound);
        }
        if let Some(value) = accessed {
            node.times.accessed = value;
        }
        if let Some(value) = written {
            node.times.written = value;
        }
        if let Some(value) = changed {
            node.times.changed = value;
        }
        if let Some(value) = created {
            node.times.created = value;
        }
        Ok(())
    }

    fn set_attributes(&self, file_id: u64, attributes: Attributes) -> OpsResult<()> {
        // `DIRECTORY` (0x10) and `NORMAL` (0x80) are **derived**, not stored, so they are not in the value domain of this
        // call: refusing is the honest answer, and silently ignoring them would be worse — the caller would believe it
        // set DIRECTORY (i.e. turned a file into a directory) while nothing happened. Windows behaves the same way:
        // `SetFileAttributes` rejects `FILE_ATTRIBUTE_DIRECTORY`; a directory is made with `CreateDirectory`.
        // The rule lives in the core (`fs_namespace::validate_attributes`) and this call site only translates the error:
        // otherwise the mock would refuse derived bits while P3-3b's real backend — which talks to the core directly —
        // would not, and the same rule would exist twice.
        fs_semantics::attributes::validate(attributes.0).map_err(|failure| match failure {
            fs_semantics::disposition::Failure::InvalidParameter => OpsError::InvalidParameter,
            other => OpsError::Other(format!("{other:?}")),
        })?;
        let mut inner = self.inner.lock().expect("mock lock");
        let node = inner.nodes.get_mut(&file_id).ok_or(OpsError::NotFound)?;
        if !node.live {
            return Err(OpsError::NotFound);
        }
        node.attributes = attributes;
        Ok(())
    }

    fn rename(&self, from: &[u8], to: &[u8], replace: bool) -> OpsResult<RenameOutcome> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner
            .calls
            .push(Call::Rename(from.to_vec(), to.to_vec(), replace));
        let clock = inner.clock;
        let from_components = split(from)?;
        let to_components = split(to)?;
        let (from_parent, from_name) = parent_and_name(&inner, &from_components)?;
        let (to_parent, to_name) = parent_and_name(&inner, &to_components)?;
        let source = inner
            .children
            .get(&(from_parent, from_name.clone()))
            .copied()
            .ok_or(OpsError::NotFound)?;
        let source_kind = inner.live_node(source)?.kind;

        // The destination's identity is remembered for two separate reasons: `r9` (a directory may not move into its
        // own subtree) needs the ancestor chain, and the `replaced` field is the caller's handle on what it just lost.
        if source_kind == Kind::Directory {
            let mut current = Some(to_parent);
            while let Some(id) = current {
                if id == source {
                    return Err(OpsError::InvalidParameter);
                }
                current = inner
                    .children
                    .iter()
                    .find(|(_, child)| **child == id)
                    .map(|((parent, _), _)| *parent);
            }
        }

        let mut replaced = None;
        if let Some(existing) = inner.children.get(&(to_parent, to_name.clone())).copied() {
            if existing != source {
                if !replace {
                    return Err(OpsError::NameCollision);
                }
                let existing_kind = inner.live_node(existing)?.kind;
                match (source_kind, existing_kind) {
                    (Kind::File, Kind::File) => {}
                    (Kind::Directory, Kind::Directory) => {
                        if inner.children.keys().any(|(parent, _)| *parent == existing) {
                            return Err(OpsError::NotEmpty);
                        }
                    }
                    _ => return Err(OpsError::TypeMismatch),
                }
                inner.children.remove(&(to_parent, to_name.clone()));
                inner.retire(existing);
                replaced = Some(existing);
            }
        }

        inner.children.remove(&(from_parent, from_name));
        inner.children.insert((to_parent, to_name.clone()), source);
        if let Some(node) = inner.nodes.get_mut(&source) {
            node.times.changed = clock;
        }
        Ok(RenameOutcome {
            file_id: source,
            binding: (to_parent, to_name),
            replaced,
        })
    }

    fn remove_at(&self, parent: u64, name: &[u8], expected_id: u64) -> OpsResult<()> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner
            .calls
            .push(Call::RemoveAt(parent, name.to_vec(), expected_id));
        // Three cases, matching the core's three cases (card §10 M6, ADR-009's ruling). The **structure** is the same;
        // the **error type** is folded, because `OpsError` has no `InvalidRecord`: the core returns
        // `NamespaceError::InvalidRecord { field }` for case 1 and this returns `NotFound`. That fold is recorded for
        // P3-3b: the real adapter will see `InvalidRecord` there, and the two map to *different* NTSTATUS values, so
        // either `OpsError` grows an `InvalidRecord` variant or the mapping layer documents the fold — it must not be
        // reported as a bug at that point.
        //   1. the name belongs to **another** inode  -> `NotFound` (deleting "whatever is there" would delete the
        //      wrong object: this is the guard that makes §3.9's path-reuse case safe). The core reports
        //      `InvalidRecord` here; see the fold note above;
        //   2. the name is free and `expected_id` is **already retired** -> `Ok(())`: a repeated removal is a no-op,
        //      not an error, because NT sends `Cleanup` and `Close` on the same handle;
        //   3. the name is free and `expected_id` is still **live** -> `NotFound`: the entry moved away and the caller's
        //      identity no longer names anything.
        match inner.children.get(&(parent, name.to_vec())).copied() {
            // The name now belongs to a **different** entry. That is not "not found" — the caller's handle is stale, and
            // the core says `InvalidRecord` here for the same reason. Folding it into `NotFound` would report a missing
            // file where the truth is "your identity no longer names this name".
            Some(bound) if bound != expected_id => Err(OpsError::InvalidRecord(format!(
                "remove_at guard: {parent}/{} is bound to {bound}, caller expected {expected_id}",
                String::from_utf8_lossy(name)
            ))),
            Some(bound) => {
                // **A directory with children cannot be removed.** The rule belongs here, at the real deletion, not in `CanDelete` (which only
                // answers "may this be deleted"): the measured defect was `rmdir` on a non-empty directory succeeding, which is not a wrong status
                // code but **data loss** — the children went with it.
                let has_children = inner.children.keys().any(|(parent, _)| *parent == bound);
                if has_children {
                    return Err(OpsError::NotEmpty);
                }
                inner.children.remove(&(parent, name.to_vec()));
                inner.retire(bound);
                Ok(())
            }
            None => match inner.nodes.get(&expected_id) {
                Some(node) if !node.live => Ok(()),
                _ => Err(OpsError::NotFound),
            },
        }
    }

    fn flush(&self) -> OpsResult<()> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(Call::Flush);
        inner.flush_count += 1;
        Ok(())
    }
}
