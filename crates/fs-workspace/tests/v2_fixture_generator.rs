//! **Fixture generator — reproducibility only. Do not run this to "fix" a failing manifest.**
//!
//! The P3-0c card requires a *real* v2 store: one produced by the v2 writer, frozen read-only, so that
//! profile v3 (which refuses v2 roots) has something genuine to refuse. Once v3 lands the write path can no
//! longer produce v2, so this generator is kept **for provenance**: running it at the recorded commit must
//! reproduce the same root id.
//!
//! It is `#[ignore]`d and writes only when `V2_FIXTURE_OUT` is set, because a test that regenerated the
//! fixture on demand would destroy the property the fixture exists for.

use std::sync::Arc;

use fs_namespace::Path as FsPath;
use fs_workspace::Repo;

fn p(text: &str) -> FsPath {
    FsPath::parse(text).expect("path")
}

#[test]
#[ignore = "generator: run once, with V2_FIXTURE_OUT=<dir>, at the commit recorded in the fixture README"]
fn generate_a_v2_store() {
    let Ok(out) = std::env::var("V2_FIXTURE_OUT") else {
        eprintln!("V2_FIXTURE_OUT is not set: nothing generated");
        return;
    };
    let out = std::path::PathBuf::from(out);
    if out.exists() {
        std::fs::remove_dir_all(&out).expect("clear the target");
    }
    std::fs::create_dir_all(&out).expect("create the target");
    let store = out.join("store");

    let repo = Arc::new(Repo::open(&store, true).expect("open"));
    repo.init().expect("init");
    let workspace = repo
        .workspace_create("w1", "main")
        .expect("create workspace");

    // A directory plus a file, and the file spans more than one chunk so the fixture exercises the
    // extent/chunk path rather than a single whole-file object (card §2.1).
    // The tree rules live on `fs_namespace::Rules`; `Workspace` only exposes the content API.
    {
        let rules = fs_namespace::Rules::new(workspace.store());
        rules.mkdir(&p("/docs")).expect("mkdir");
    }
    workspace
        .create_file("/docs/readme.txt")
        .expect("create file");
    let block = b"0123456789abcdef".repeat(1024); // 16 KiB of a repeating pattern
    let mut content = Vec::new();
    for _ in 0..2 {
        content.extend_from_slice(&block);
    }
    workspace
        .write_at("/docs/readme.txt", 0, &content)
        .expect("write");
    workspace
        .truncate(
            "/docs/readme.txt",
            u64::try_from(content.len()).expect("fits"),
        )
        .expect("truncate");
    let published = workspace
        .commit("the v2 fixture commit", false)
        .expect("commit");
    let commit = published.commit.clone().expect("commit id");
    let root = published.root.to_string();

    // A snapshot too: P3-2 needs a snapshot root to refuse, not just a commit root.
    let snapshot = workspace.snapshot().expect("snapshot");
    workspace.close().expect("close");
    drop(repo);

    println!("commit={commit}");
    println!("root={root}");
    println!("snapshot_root={}", snapshot.root);
    println!("content_len={}", content.len());
    println!("content_blake3={}", blake3::hash(&content).to_hex());
}
