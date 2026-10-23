//! Temporary probe for the large-file partial-write case.

use std::path::PathBuf;

use fs_api::{Fs, OpenMode, BLOCK_BYTES};

#[test]
fn probe_large_partial_write() {
    let dir: PathBuf = std::env::temp_dir().join(format!("fs-probe-lw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fs = Fs::open(&dir, "default", OpenMode::Create).unwrap();

    let payload: Vec<u8> = (0..(BLOCK_BYTES as usize * 3 + 123))
        .map(|index| (index % 251) as u8)
        .collect();
    fs.create("/big").unwrap();
    fs.write_at("/big", 0, &payload).unwrap();

    let size = fs.stat("/big").unwrap().size as usize;
    println!("size={size} expected={}", payload.len());

    let mut buffer = vec![0xAAu8; size];
    let read = fs.read_at("/big", 0, &mut buffer).unwrap();
    println!("read={read}");
    let first_poison = buffer[..read].iter().position(|b| *b == 0xAA);
    println!("first poison byte at {first_poison:?}");
    if let Some(i) = first_poison {
        let end = (i + 16).min(read);
        println!("buffer[{}..{}]={:?}", i, end, &buffer[i..end]);
        println!("expected      ={:?}", &payload[i..end]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
