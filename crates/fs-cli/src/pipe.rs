//! The control protocol (ADR-010 §10.2): one line of JSON per request, one line back, three operations in S1.
//!
//! What lives here is the part with **no operating system in it**: the frame limit, the version gate, the request/response shapes, and the dispatch table. The
//! pipe itself — SDDL, `ConnectNamedPipe`, the accept loop, the threads — is next to this module and is the only code that needs those calls.
//!
//! Splitting it this way is the same decision as §4.1 (path discovery): the rules that have a table of cases are testable without mounting anything, so the
//! criteria stay cheap enough to be re-run.

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
pub const PROTOCOL: u32 = 1;

/// The largest frame accepted, in bytes (`PIPE_FRAME_MAX`, ADR-010 §10.2). A line longer than this is refused **and the connection is dropped**, but the server
/// keeps serving the next one — which is what makes "a bad client cannot take the mount down" a property rather than a hope.
pub const FRAME_MAX: usize = 64 * 1024;

/// One request line.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    /// Protocol version the client speaks.
    pub v: u32,
    /// Which operation to run.
    pub op: String,
    /// Operation-specific payload; absent means `{}`.
    #[serde(default)]
    pub args: serde_json::Value,
}

/// What the mount answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// `hello`: everything the record says about this mount.
    Hello {
        /// Repository the mount serves.
        store: String,
        /// Workspace being served.
        workspace: String,
        /// Branch that workspace publishes to.
        branch: String,
        /// Process serving the mount.
        pid: u32,
        /// Where it is attached.
        mount_point: String,
        /// Protocol version the server speaks.
        protocol: u32,
    },
    /// `status`: read-only view of the workspace behind the mount.
    Status {
        /// Workspace name.
        workspace: String,
        /// Its branch.
        branch: String,
        /// The commit its changes sit on.
        base_commit: String,
        /// The root the view is composed over. **Not derivable from `base_commit`**: a `restore` moves the root and deliberately leaves the base commit where
        /// the branch head was (history grows), so a report that named only the commit would say "on c2" while serving c1's tree.
        #[serde(default)]
        base_root: String,
        /// Whether there are unpublished changes.
        dirty: bool,
        /// How many file objects are open on the mount right now.
        handles: usize,
    },
    /// `unmount`: the mount is gone (answered **after** unmount, flush and record removal).
    Unmounted {},
    /// A frame over the limit.
    FrameTooLarge {
        /// The limit, so the client can see what it hit.
        max: usize,
    },
    /// A version this build does not speak.
    ProtocolVersion {
        /// Versions that would work.
        supported: Vec<u32>,
    },
    /// Nothing could be made of the request.
    BadRequest {
        /// What was wrong, in one sentence.
        detail: String,
    },
    /// The stop sequence has begun; nothing further will be answered.
    ShuttingDown,
    /// A remote `unmount` found files still open and refused, leaving everything as it was.
    HandlesOpen {
        /// How many were open when the wait expired.
        handles: usize,
    },
    /// The mount now serves another workspace: the answer to `switch` (S3 card §5.2).
    Switched {
        /// The workspace it serves now.
        workspace: String,
        /// The branch that workspace publishes to.
        branch: String,
        /// Directory handles that were pointing at the old view.
        dirs_invalidated: usize,
    },
    /// A remote `restore` was refused because **file** handles were open (D8 = B): the objects under them could vanish.
    ///
    /// Its own variant rather than `HandlesOpen` because the two answer different questions: `unmount` needs **every** handle closed (the volume goes away),
    /// `restore` draws the line at files and lets directory handles through. Folding them into one shape would let a reader assume the numbers mean the same
    /// thing.
    RestoreRefused {
        /// Open file handles — the blocking kind.
        files: usize,
        /// Open directory handles — reported so the message can say the view will be stale, not blocking.
        dirs: usize,
    },
    /// The operation itself failed.
    Failed {
        /// **Which kind of failure** — a script can branch on this, where a message can only be read (ADR-010 §10.4).
        kind: String,
        /// Message for the human on the other end.
        detail: String,
        /// Structured fields, so the caller does not have to parse `detail` for the one value it needs.
        #[serde(default)]
        fields: serde_json::Value,
    },
}

/// What the dispatcher is allowed to touch. Injected so the three operations can be tested without a mount.
pub trait Context {
    /// The record's fields, for `hello` and `status`.
    fn hello(&self) -> Response;
    /// The workspace's current state, for `status`.
    fn status(&self) -> Response;
    /// Publishes the workspace, for `commit`.
    ///
    /// The server calls this **outside** the pipe's own bookkeeping: a commit can take as long as the tree is big, and the answer has to be the publish's own
    /// outcome (`BranchMoved`, `NothingToCommit`) rather than a generic failure.
    fn commit(&self, message: &str, force: bool) -> Response;
    /// Takes a snapshot, for `snapshot`.
    fn snapshot(&self) -> Response;
    /// Makes the mount serve `workspace` instead, for `switch` (S3 card §5).
    fn switch(&self, workspace: &str) -> Response;

    /// Moves the mount's workspace back to `commit`, for `restore` (S3 card §4).
    ///
    /// Goes through the mount because the mount is the only thing that can (a) quiesce its own opens and (b) hold the publication gate: a second handle opened
    /// by the CLI would be a second view of the same overlay, which is exactly what the release lock exists to prevent.
    fn restore(&self, commit: &str, discard: bool) -> Response;
    /// Whether the stop sequence has begun.
    fn shutting_down(&self) -> bool;
}

/// Turns one frame into one response — the whole protocol surface, as a function.
///
/// Order matters and is the ADR's: **size**, then **version**, then **parse**, then **op**. Checking the op first would answer `BadRequest` to a frame whose
/// version we cannot even read, which tells the client the wrong thing about why it failed.
#[must_use]
pub fn dispatch(frame: &[u8], context: &impl Context) -> Response {
    if frame.len() > FRAME_MAX {
        return Response::FrameTooLarge { max: FRAME_MAX };
    }
    if context.shutting_down() {
        return Response::ShuttingDown;
    }
    let text = String::from_utf8_lossy(frame);
    let request: Request = match serde_json::from_str(text.trim()) {
        Ok(request) => request,
        Err(error) => {
            return Response::BadRequest {
                detail: error.to_string(),
            }
        }
    };
    if request.v != PROTOCOL {
        return Response::ProtocolVersion {
            supported: vec![PROTOCOL],
        };
    }
    match request.op.as_str() {
        "hello" => context.hello(),
        "status" => context.status(),
        "commit" => {
            let message = request
                .args
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let force = request
                .args
                .get("force")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            context.commit(message, force)
        }
        "snapshot" => context.snapshot(),
        "switch" => {
            let workspace = request
                .args
                .get("workspace")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            context.switch(workspace)
        }
        "restore" => {
            let commit = request
                .args
                .get("commit")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let discard = request
                .args
                .get("discard")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            context.restore(commit, discard)
        }
        // `unmount` is answered by the server loop itself (it has to stop the mount), so reaching here means the loop forgot to intercept it.
        "unmount" => Response::BadRequest {
            detail: "unmount is handled by the server, not the dispatcher".to_owned(),
        },
        other => Response::BadRequest {
            detail: format!("unknown op {other:?}"),
        },
    }
}

/// One response as a frame: JSON plus the newline that ends it.
///
/// The newline is part of the protocol, so it is written here rather than at every call site — a client that forgets it would hang waiting for a frame end,
/// and the failure would look like a timeout instead of a formatting mistake.
#[must_use]
pub fn frame(response: &Response) -> Vec<u8> {
    let mut bytes =
        serde_json::to_vec(response).unwrap_or_else(|_| b"{\"result\":\"failed\"}".to_vec());
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::{dispatch, frame, Context, Request, Response, FRAME_MAX, PROTOCOL};

    struct Fake {
        shutting_down: bool,
    }

    impl Context for Fake {
        fn hello(&self) -> Response {
            Response::Hello {
                store: "C:/repo".to_owned(),
                workspace: "dev".to_owned(),
                branch: "dev".to_owned(),
                pid: 4242,
                mount_point: "U:".to_owned(),
                protocol: PROTOCOL,
            }
        }

        fn status(&self) -> Response {
            Response::Status {
                workspace: "dev".to_owned(),
                branch: "dev".to_owned(),
                base_commit: "00".to_owned(),
                base_root: "00".to_owned(),
                dirty: false,
                handles: 0,
            }
        }

        fn commit(&self, message: &str, force: bool) -> Response {
            // The fake answers **something identifiable**, so a dispatch test can tell "routed to commit" from "fell through to an error".
            Response::Failed {
                kind: "committed".to_owned(),
                detail: format!("{message}|{force}"),
                fields: serde_json::json!({}),
            }
        }

        fn snapshot(&self) -> Response {
            Response::Failed {
                kind: "snapshotted".to_owned(),
                detail: String::new(),
                fields: serde_json::json!({}),
            }
        }

        fn switch(&self, workspace: &str) -> Response {
            Response::Switched {
                workspace: workspace.to_owned(),
                branch: format!("branch-of-{workspace}"),
                dirs_invalidated: 1,
            }
        }

        fn restore(&self, commit: &str, discard: bool) -> Response {
            // Identifiable for the same reason as `commit`: a dispatch test must see that **both** arguments arrived, not merely that something answered.
            Response::Failed {
                kind: "restored".to_owned(),
                detail: format!("{commit}|{discard}"),
                fields: serde_json::json!({}),
            }
        }

        fn shutting_down(&self) -> bool {
            self.shutting_down
        }
    }

    fn frame_of(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).expect("encode")
    }

    /// **A11**: the protocol's own table — hello answers, wrong version is named, an oversized frame is refused **without killing the server**, and junk gets
    /// `BadRequest`. The "and then it still works" half is the one that matters: a server that answers correctly once and then refuses to accept is a server
    /// that a single bad client can take down.
    #[test]
    fn the_protocol_answers_and_keeps_answering() {
        let context = Fake {
            shutting_down: false,
        };
        assert!(matches!(
            dispatch(
                &frame_of(serde_json::json!({"v": 1, "op": "hello"})),
                &context
            ),
            Response::Hello { .. }
        ));
        assert_eq!(
            dispatch(
                &frame_of(serde_json::json!({"v": 2, "op": "hello"})),
                &context
            ),
            Response::ProtocolVersion {
                supported: vec![PROTOCOL]
            }
        );
        assert_eq!(
            dispatch(&vec![b'x'; FRAME_MAX + 1], &context),
            Response::FrameTooLarge { max: FRAME_MAX }
        );
        assert!(matches!(
            dispatch(b"not json at all", &context),
            Response::BadRequest { .. }
        ));
        assert!(matches!(
            dispatch(
                &frame_of(serde_json::json!({"v": 1, "op": "nope"})),
                &context
            ),
            Response::BadRequest { .. }
        ));
        // **The half that proves the server survives**: after every rejection above, a fresh request is still answered.
        assert!(matches!(
            dispatch(
                &frame_of(serde_json::json!({"v": 1, "op": "status"})),
                &context
            ),
            Response::Status { .. }
        ));
    }

    /// **P1**: `commit` and `snapshot` are routed to the context — and the routing is **not** a fall-through.
    ///
    /// The fake answers with an identifiable `kind`, so "the dispatcher sent it to `commit`" is a fact about this function rather than a guess from a message.
    /// A dispatcher that quietly answered `BadRequest` would make every client look like it had sent the wrong op.
    #[test]
    fn commit_and_snapshot_are_routed() {
        let context = Fake {
            shutting_down: false,
        };
        match dispatch(
            &frame_of(
                serde_json::json!({"v": 1, "op": "commit", "args": {"message": "m", "force": true}}),
            ),
            &context,
        ) {
            Response::Failed { kind, detail, .. } => {
                assert_eq!(kind, "committed", "cell P1: routed to commit");
                assert_eq!(detail, "m|true", "cell P1: message and force arrive");
            }
            other => panic!("cell P1: expected the commit route, got {other:?}"),
        }
        match dispatch(
            &frame_of(serde_json::json!({"v": 1, "op": "snapshot"})),
            &context,
        ) {
            Response::Failed { kind, .. } => {
                assert_eq!(kind, "snapshotted", "cell P1: routed to snapshot")
            }
            other => panic!("cell P1: expected the snapshot route, got {other:?}"),
        }
        // The default message is empty, not an error: `commit` without a message is a caller's choice, and the CLI passes one.
        match dispatch(
            &frame_of(serde_json::json!({"v": 1, "op": "commit"})),
            &context,
        ) {
            Response::Failed { detail, .. } => {
                assert_eq!(detail, "|false", "cell P1: absent args default")
            }
            other => panic!("cell P1: got {other:?}"),
        }
    }

    /// **A11c**: once the stop sequence has begun, every op answers `ShuttingDown` — including the ones that would otherwise work.
    #[test]
    fn a_shutting_down_mount_answers_shutting_down() {
        let context = Fake {
            shutting_down: true,
        };
        for op in ["hello", "status", "unmount", "nonsense"] {
            assert_eq!(
                dispatch(&frame_of(serde_json::json!({"v": 1, "op": op})), &context),
                Response::ShuttingDown,
                "cell A11c: {op}"
            );
        }
    }

    /// A request round-trips through the frame format: one line, newline-terminated.
    #[test]
    fn a_response_is_one_line_and_a_request_reads_back() {
        let encoded = frame(&Response::Unmounted {});
        assert_eq!(encoded.last(), Some(&b'\n'), "cell frame: terminated");
        assert_eq!(encoded.iter().filter(|byte| **byte == b'\n').count(), 1);
        let request: Request =
            serde_json::from_slice(b"{\"v\":1,\"op\":\"hello\"}\n").expect("parse");
        assert_eq!(request.op, "hello");
        assert_eq!(request.v, PROTOCOL);
    }
}
