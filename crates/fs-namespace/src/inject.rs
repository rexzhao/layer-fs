//! A deterministic mid-operation failure for the atomicity tests (F-T3-2).
//!
//! Compiled only with the `failure-injection` feature, which the crate enables
//! for its own tests and no consumer receives by default — the same gate pattern
//! as `cas-store`'s crash points.
//!
//! The injection is a real statement failure, not a simulated flag: the helper
//! deletes a row the operation needs, so the next statement inside the
//! transaction fails exactly where a genuine fault would.

use rusqlite::Connection;

/// Which side of the failing operation a fault is planted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Make the change fail *before* any row is written: remove the identity
    /// allocator row, so `next_serial` fails as the first statement of a
    /// `mkdir` transaction body.
    BeforeMutation,
    /// Make the change fail *after* its rows are written: point the allocator at
    /// an already-taken serial, so the file row insert collides after the
    /// directory entry exists.
    AfterMutation,
    /// Make the **publication** step fail, after every row change of any
    /// operation: remove the namespace row, which `publish` reads to find the
    /// previous head. This applies to all four mutating operations.
    BeforePublication,
}

/// Plants the fault described by `fault` on `connection`.
///
/// # Errors
///
/// Propagates database failures from planting the fault itself.
pub fn plant(connection: &Connection, fault: Fault) -> rusqlite::Result<()> {
    match fault {
        Fault::BeforeMutation => {
            connection.execute("DELETE FROM file_id_allocator", [])?;
        }
        Fault::AfterMutation => {
            // Make the file row collide: point the allocator at a serial that is
            // already taken. `mkdir` writes the directory entry first and then
            // inserts the file row, so the failure lands *after* the operation
            // has already written one row — which is exactly the case a
            // non-transactional implementation would leave half-applied.
            let namespace: String = connection
                .query_row("SELECT name FROM namespace LIMIT 1", [], |row| row.get(0))
                .unwrap_or_else(|_| "default".to_owned());
            connection.execute(
                "INSERT INTO file
                 (namespace, file_id, kind, size, content_root, created_ns, modified_ns)
                 VALUES (?1, 1000, 2, 0, NULL, 0, 0)",
                [&namespace],
            )?;
            connection.execute(
                "UPDATE file_id_allocator SET next_serial = 1000 WHERE namespace = ?1",
                [&namespace],
            )?;
        }
        Fault::BeforePublication => {
            // A trigger that aborts any root insert: publication fails after the
            // operation has already changed its rows, while every read path
            // (head, records, entries) still works — so the test can assert the
            // state is unchanged rather than merely unreadable.
            connection.execute_batch(
                "CREATE TRIGGER inject_fail_publication
                 BEFORE INSERT ON root
                 BEGIN
                     SELECT RAISE(ABORT, 'injected publication failure');
                 END;",
            )?;
        }
    }
    Ok(())
}
