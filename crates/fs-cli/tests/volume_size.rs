//! **虚拟卷容量**：`total` 是 store 配置（默认 128 GiB），`free` = total − 视图逻辑总量。
//!
//! 两条测量规矩写在这里，因为它们都是踩出来的：
//!
//! - **读之前跨过 250 ms 缓存窗口**（`view_logical_bytes` 的缓存）。否则判据测的是缓存，不是事实。
//! - **用"≤ 一个 4 KiB 单元"而不是精确相等**：`used` 是字节精确的（5 字节文件 ⇒ size=5），但 Windows 把
//!   `total − used` 向上取整到分配粒度后才显示（5 + 100000 = 100005 ⇒ 报告差 102400）。把取整当缺陷，
//!   判据就会逼着产品去模仿一个显示层的行为。

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;
const UNIT: u64 = 4096;

struct MountChild(Child);

impl Drop for MountChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fs-cli"))
}

fn temp_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "fs-cli-volume-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("temp");
    path
}

fn records_dir(store: &std::path::Path) -> PathBuf {
    store.join("mounts")
}

/// `(total, free)` of a mounted volume, as Windows reports it.
fn capacity(letter: &str) -> (u64, u64) {
    let script = format!(
        "$d=[System.IO.DriveInfo]'{letter}'; '{{0}} {{1}}' -f $d.TotalSize, $d.AvailableFreeSpace"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let total = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let free = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (total, free)
}

fn mount(store: &std::path::Path, letter: &str) -> (MountChild, PathBuf) {
    let mount_point = PathBuf::from(letter);
    wait_until_free(&mount_point);
    let child = MountChild(
        Command::new(binary())
            .arg("--store")
            .arg(store)
            .arg("mount")
            .arg(&mount_point)
            .arg("--workspace")
            .arg("main")
            .env("LAYERFS_MOUNTS_DIR", records_dir(store))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mount"),
    );
    let record = records_dir(store).join(format!("{}.mount", fs_cli::record_name(&mount_point)));
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15)
        && !std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe=")
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::read_to_string(&record)
            .unwrap_or_default()
            .contains("pipe="),
        "cell setup: the mount must be recorded"
    );
    let root = PathBuf::from(format!("{letter}\\"));
    let probe = root.join("probe.txt");
    let started = Instant::now();
    loop {
        match std::fs::write(&probe, b"probe") {
            Ok(()) => break,
            Err(_) if started.elapsed() < Duration::from_secs(15) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(error) => panic!("cell setup: the volume never became writable: {error:?}"),
        }
    }
    let _ = std::fs::remove_file(&probe);
    // **Past the cache window** before anything reads the capacity.
    std::thread::sleep(Duration::from_millis(400));
    (child, root)
}

fn wait_until_free(mount_point: &std::path::Path) {
    if !mount_point.to_string_lossy().ends_with(':') {
        return;
    }
    let started = Instant::now();
    while fs_winfsp::mounted::drive_present(mount_point) != Some(false) {
        assert!(
            started.elapsed() <= Duration::from_secs(20),
            "cell setup: {} never became free",
            mount_point.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// **C1** — a store that was never told a size advertises **128 GiB**.
#[test]
fn a_default_store_advertises_128_gib() {
    common::strict_gate();
    let store = temp_dir("default");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let (_child, _root) = mount(&store, "N:");
    let (total, _free) = capacity("N:");
    println!("C1 reading: total={total} ({} GiB)", total / GIB);
    assert_eq!(
        total,
        128 * GIB,
        "cell C1: the default volume size is 128 GiB"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **C2** — `init --size 256M` is what the volume then advertises.
#[test]
fn an_explicit_size_is_what_the_volume_advertises() {
    common::strict_gate();
    let store = temp_dir("explicit");
    let output = Command::new(binary())
        .arg("init")
        .arg(&store)
        .arg("--size")
        .arg("256M")
        .output()
        .expect("init");
    assert!(output.status.success(), "cell C2: init --size must work");
    let (_child, _root) = mount(&store, "M:");
    let (total, _free) = capacity("M:");
    println!("C2 reading: total={total} ({} MiB)", total / (1024 * 1024));
    assert_eq!(
        total,
        256 * 1024 * 1024,
        "cell C2: the configured size is advertised"
    );
    let _ = std::fs::remove_dir_all(&store);
}

/// **C3** — `total − free` is the **logical size of the view**, up to the allocation unit Windows rounds to.
#[test]
fn the_used_amount_is_the_logical_size_of_the_view() {
    common::strict_gate();
    let store = temp_dir("used");
    Command::new(binary())
        .arg("init")
        .arg(&store)
        .output()
        .expect("init");
    let (_child, root) = mount(&store, "L:");
    let (total_before, free_before) = capacity("L:");
    assert_eq!(
        total_before - free_before,
        0,
        "cell C3: a fresh mount has used nothing"
    );

    std::fs::write(root.join("small.txt"), b"hello").expect("write 5 bytes");
    std::fs::write(root.join("large.bin"), vec![0u8; 100_000]).expect("write 100000 bytes");
    // Past the cache window, again: this is what makes the reading a fact.
    std::thread::sleep(Duration::from_millis(400));
    let (total, free) = capacity("L:");
    let used = total - free;
    let expected = 5 + 100_000;
    println!(
        "C3 reading: used={used} expected={expected} delta={}",
        used as i64 - expected as i64
    );
    assert_eq!(
        total,
        128 * GIB,
        "cell C3: the total is unchanged by writes"
    );
    let delta = used.abs_diff(expected);
    assert!(
        delta < UNIT,
        "cell C3: used must be the view's logical size, up to one allocation unit (used {used}, expected {expected}, delta {delta})"
    );
    let _ = std::fs::remove_dir_all(&store);
}
