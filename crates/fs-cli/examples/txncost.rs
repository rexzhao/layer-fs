//! What does **one small transaction** cost on this machine, with the pragmas the product uses?
//!
//! A diagnostic, not a scenario: the benchmark said a file create costs ~8.6 ms and that the open path is only 0.185 ms, so the cost is in "write a row and
//! commit". This measures that half in isolation — same SQLite, same `journal_mode=WAL`, same `synchronous`, `locking_mode=EXCLUSIVE` — with no layerfs code in
//! the way. If one commit here costs milliseconds, the product is paying the storage engine's price per operation; if it costs microseconds, the difference is
//! ours.
//!
//! Usage: `cargo run --release --example txncost -- [--sync NORMAL|FULL] [--rows 1000]`

fn main() {
    let mut synchronous = "NORMAL";
    let mut rows = 1000usize;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--sync" => {
                synchronous = match arguments.next().as_deref() {
                    Some("FULL") => "FULL",
                    _ => "NORMAL",
                }
            }
            "--rows" => {
                rows = arguments
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(rows)
            }
            other => panic!("unknown argument {other:?}"),
        }
    }
    let directory = std::env::temp_dir().join(format!("layerfs-txncost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp");
    let path = directory.join("probe.sqlite");

    let open = || {
        let connection = rusqlite::Connection::open(&path).expect("open");
        connection
            .execute_batch("PRAGMA journal_mode = WAL")
            .expect("wal");
        connection
            .execute_batch(&format!("PRAGMA synchronous = {synchronous}"))
            .expect("sync");
        connection
            .execute_batch("PRAGMA locking_mode = EXCLUSIVE")
            .expect("exclusive");
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS row (id INTEGER PRIMARY KEY, payload BLOB) STRICT",
            )
            .expect("schema");
        connection
    };

    let connection = open();
    // (a) one transaction per row — what the product does per file operation.
    let started = std::time::Instant::now();
    for index in 0..rows {
        connection
            .execute("INSERT INTO row (payload) VALUES (?1)", [vec![0u8; 32]])
            .expect("insert");
        let _ = index;
    }
    let per_row = started.elapsed();
    println!(
        "a) one commit per row  (sync={synchronous}): {} rows in {:?} ⇒ **{:.3} ms/row**",
        rows,
        per_row,
        per_row.as_secs_f64() * 1000.0 / rows as f64
    );

    // (b) the same rows inside a single transaction — what batching would buy.
    let started = std::time::Instant::now();
    connection.execute_batch("BEGIN IMMEDIATE").expect("begin");
    for _ in 0..rows {
        connection
            .execute("INSERT INTO row (payload) VALUES (?1)", [vec![0u8; 32]])
            .expect("insert");
    }
    connection.execute_batch("COMMIT").expect("commit");
    let batched = started.elapsed();
    println!(
        "b) one commit for all   (sync={synchronous}): {} rows in {:?} ⇒ **{:.3} ms/row**",
        rows,
        batched,
        batched.as_secs_f64() * 1000.0 / rows as f64
    );
    println!(
        "⇒ per-commit fixed cost ≈ {:.3} ms",
        (per_row.as_secs_f64() - batched.as_secs_f64()) * 1000.0 / rows as f64
    );
    let _ = std::fs::remove_dir_all(&directory);
}
