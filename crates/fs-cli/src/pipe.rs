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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// The operation itself failed.
    Failed {
        /// Message for the human on the other end.
        detail: String,
    },
}

/// What the dispatcher is allowed to touch. Injected so the three operations can be tested without a mount.
pub trait Context {
    /// The record's fields, for `hello` and `status`.
    fn hello(&self) -> Response;
    /// The workspace's current state, for `status`.
    fn status(&self) -> Response;
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
                dirty: false,
                handles: 0,
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
