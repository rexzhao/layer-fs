//! Scratch reproduction (architect triage, not for commit): truncate-down leaves the
//! boundary page's tail in the overlay, so a later extension resurrects discarded bytes.

use std::path::PathBuf;

use fs_api::Repo;

fn temp_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "arch-truncate-repro-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp root");
    path
}

#[test]
fn truncate_down_then_extend_reads_zeros() {
    let root = temp_root("pure");
    let repo = Repo::create(root.join("store")).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xAA; 100]).expect("write");
    workspace.truncate("/f", 10).expect("shrink");
    workspace.truncate("/f", 200).expect("extend");
    let mut got = vec![0u8; 200];
    let read = workspace.read_at("/f", 0, &mut got).expect("read");
    assert_eq!(read, 200);
    assert_eq!(got[..10], [0xAA; 10], "surviving prefix");
    assert_eq!(got[10..], [0u8; 190], "discarded bytes came back");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn truncate_down_then_write_past_eof_reads_zeros_between() {
    let root = temp_root("write");
    let repo = Repo::create(root.join("store")).expect("create");
    let workspace = repo.workspace_create("w1", "main").expect("workspace");
    workspace.create("/f").expect("create");
    workspace.write_at("/f", 0, &[0xAA; 100]).expect("write");
    workspace.write_at("/f", 300, &[0xBB; 10]).expect("write far");
    workspace.truncate("/f", 50).expect("shrink");
    workspace.write_at("/f", 400, &[0xCC; 1]).expect("write past");
    let mut got = vec![0u8; 401];
    let read = workspace.read_at("/f", 0, &mut got).expect("read");
    assert_eq!(read, 401);
    assert_eq!(got[..50], [0xAA; 50], "surviving prefix");
    assert_eq!(got[50..400], [0u8; 350], "discarded bytes came back");
    assert_eq!(got[400], 0xCC, "the write itself");
    let _ = std::fs::remove_dir_all(root);
}
