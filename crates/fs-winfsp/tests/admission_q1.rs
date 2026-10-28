//! **Q1** — the admission state machine on its own, with no mount and no adapter.
//!
//! Each case is one transition, and each exists because getting it wrong produces a different, invisible failure: an open that is admitted during a restore
//! lands on a view that is being replaced; a `Closing` open that *waits* deadlocks the unmount that is waiting for the dispatcher; a refusal that does not
//! restore the gate hangs every later open. So the machine is tested as a machine, where a thread can be seen to block.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fs_winfsp::admission::{Admission, Gatekeeper, Quiesce, QuiesceKind, Refused};

#[test]
fn an_open_gate_admits_immediately() {
    let gate = Gatekeeper::new();
    let ticket = gate.admit().expect("an open gate admits");
    assert_eq!(
        gate.admission(),
        Admission::Open,
        "cell Q1: state stays Open"
    );
    drop(ticket);
    // Still usable after a ticket came back.
    let _again = gate
        .admit()
        .expect("the gate reopens after a ticket returns");
}

#[test]
fn a_paused_gate_makes_an_open_wait_and_then_lets_it_through() {
    let gate = Arc::new(Gatekeeper::new());
    let quiesced = match gate.quiesce(QuiesceKind::Restore, || (0, 0), Duration::from_secs(5)) {
        Quiesce::Quiesced(guard) => guard,
        other => panic!("cell Q1: an idle gate must quiesce, got {other:?}"),
    };

    let admitted = Arc::new(AtomicBool::new(false));
    let waiter = {
        let gate = Arc::clone(&gate);
        let admitted = Arc::clone(&admitted);
        std::thread::spawn(move || {
            let ticket = gate
                .admit()
                .expect("a paused gate waits, it does not refuse");
            admitted.store(true, Ordering::SeqCst);
            drop(ticket);
        })
    };
    // Measured, not assumed: the opened thread must still be blocked while the guard is held.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !admitted.load(Ordering::SeqCst),
        "cell Q1: an open during a restore must wait, not proceed"
    );
    drop(quiesced);
    waiter
        .join()
        .expect("the waiter finishes once the gate reopens");
    assert!(
        admitted.load(Ordering::SeqCst),
        "cell Q1: the waiting open must be let through afterwards"
    );
    assert_eq!(
        gate.admission(),
        Admission::Open,
        "cell Q1: the gate reopens"
    );
}

#[test]
fn a_closing_gate_refuses_immediately() {
    let gate = Gatekeeper::new();
    let started = Instant::now();
    let quiesced = match gate.quiesce(QuiesceKind::Switch, || (0, 0), Duration::from_secs(5)) {
        Quiesce::Quiesced(guard) => guard,
        other => panic!("cell Q1: an idle gate must quiesce, got {other:?}"),
    };
    assert_eq!(
        gate.admit().err(),
        Some(Refused::Closing),
        "cell Q1: during a switch an open fails rather than waits — waiting would deadlock the unmount"
    );
    // The refusal is immediate, not "eventually": this is the difference the card draws between Paused and Closing.
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cell Q1: the refusal took {:?}, which is not immediate",
        started.elapsed()
    );
    drop(quiesced);
}

#[test]
fn quiesce_waits_for_the_ticket_in_flight() {
    let gate = Arc::new(Gatekeeper::new());
    let ticket = gate.admit().expect("admit");
    let quiesce_done = Arc::new(AtomicBool::new(false));
    let quiescer = {
        let gate = Arc::clone(&gate);
        let done = Arc::clone(&quiesce_done);
        std::thread::spawn(move || {
            let ok = matches!(
                gate.quiesce(QuiesceKind::Restore, || (0, 0), Duration::from_secs(5)),
                Quiesce::Quiesced(_)
            );
            done.store(true, Ordering::SeqCst);
            ok
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !quiesce_done.load(Ordering::SeqCst),
        "cell Q1: quiesce must wait while a ticket is out"
    );
    drop(ticket);
    let ok = quiescer.join().expect("the quiescer finishes");
    assert!(
        quiesce_done.load(Ordering::SeqCst),
        "cell Q1: quiesce returns once the last ticket comes back"
    );
    assert!(ok, "cell Q1: and it succeeds");
}

#[test]
fn a_file_handle_refuses_and_leaves_the_gate_open() {
    let gate = Gatekeeper::new();
    match gate.quiesce(QuiesceKind::Restore, || (1, 0), Duration::from_secs(5)) {
        Quiesce::HandlesOpen { files, dirs } => {
            assert_eq!(
                (files, dirs),
                (1, 0),
                "cell Q1: the counts travel with the refusal"
            );
        }
        other => panic!("cell Q1: a file handle must refuse, got {other:?}"),
    }
    // **The state came back**: a refusal that left the gate Paused would hang every later open, and that hang would look like a mount problem.
    assert_eq!(
        gate.admission(),
        Admission::Open,
        "cell Q1: a refusal must restore the gate"
    );
    let _ticket = gate.admit().expect("and an open works again");
}

#[test]
fn directory_handles_do_not_refuse() {
    // D8 = B: the shell that starts the command holds one of these, so counting them would make "checkout from inside the mount" impossible forever.
    let gate = Gatekeeper::new();
    let outcome = gate.quiesce(QuiesceKind::Restore, || (0, 3), Duration::from_secs(5));
    match outcome {
        Quiesce::Quiesced(guard) => drop(guard),
        other => panic!("cell Q1: directory handles must be reported, not blocking, got {other:?}"),
    }
}

#[test]
fn a_quiesce_that_never_drains_times_out() {
    let gate = Gatekeeper::new();
    let ticket = gate.admit().expect("admit");
    let started = Instant::now();
    let outcome = gate.quiesce(QuiesceKind::Restore, || (0, 0), Duration::from_millis(300));
    let waited = started.elapsed();
    println!("Q1 timeout reading: waited {waited:?} for a ticket that never came back");
    match outcome {
        Quiesce::TimedOut { in_flight } => {
            assert_eq!(in_flight, 1, "cell Q1: the count is reported")
        }
        other => panic!("cell Q1: expected a timeout, got {other:?}"),
    }
    assert!(
        waited >= Duration::from_millis(300),
        "cell Q1: the wait must be honoured (waited {waited:?})"
    );
    assert_eq!(
        gate.admission(),
        Admission::Open,
        "cell Q1: a timeout must also restore the gate"
    );
    drop(ticket);
}

#[test]
fn the_adapter_quiesces_on_its_own_handle_table() {
    // The counts `quiesce` judges on must come from the **same** adapter: a caller that passed somebody else's numbers would be applying the rule to the wrong
    // mount, and the mistake would look like "restore refused for no reason".
    let adapter = fs_winfsp::Adapter::new(fs_winfsp::mock::MockBackend::new());
    match adapter.quiesce(
        fs_winfsp::admission::QuiesceKind::Restore,
        Duration::from_secs(5),
    ) {
        Quiesce::Quiesced(guard) => drop(guard),
        other => panic!("cell Q1: an idle adapter must quiesce, got {other:?}"),
    }
    // One file handle, opened through the **public** door, is enough to refuse — and the table it is counted in is this adapter's.
    let id = adapter
        .open_handle(1, None, false)
        .expect("register a file handle");
    let outcome = adapter.quiesce(
        fs_winfsp::admission::QuiesceKind::Restore,
        Duration::from_secs(5),
    );
    match outcome {
        Quiesce::HandlesOpen { files, dirs } => {
            println!("adapter quiesce reading: files={files} dirs={dirs}");
            assert_eq!(files, 1, "cell Q1: the file handle is counted");
        }
        other => panic!("cell Q1: a file handle must refuse, got {other:?}"),
    }
    assert_eq!(
        adapter.admission_kind(),
        Admission::Open,
        "cell Q1: the adapter gate comes back"
    );
    let _ = id;
}
