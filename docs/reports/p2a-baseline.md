# P2a 性能基线

本报告是 P2a 的**性能数据**，不是门槛：没有任何阈值断言。所有数字都由
`crates/fs-api/examples/baseline.rs` 产出，可用 §7 的命令在任何机器上复现。

## 0. 测量规则

沿用 `docs/reports/p1-baseline.md` §0 的 R1–R5：

| 规则 | 内容 | 本报告如何满足 |
| --- | --- | --- |
| R1 | 每场景 ≥3 次重复，报告**全部**原始 run（min/median/max） | 每个小节的表格逐行列出 run 0/1/2，并给 min/median/max |
| R2 | 探测必须持续足够长以穿透写缓存 | write 场景每 run 3 000 次写；snapshot 场景每次都是 1 000 文件的完整发布 |
| R3 | 每个数字注明载荷特征 | 每个小节标题下写明文件数、大小、内容是否互异、冷热 |
| R4 | 报告测的是哪块盘、哪个目录 | §1；数据目录由 `WCAS_BENCH_ROOT` 指定，本报告为 `F:\qs-bench\bench` |
| R5 | 不取最佳单次值，不宣称盘间优劣 | 只报 F: 一块盘，不做跨盘比较 |

## 1. 机器与工具链

| 项 | 值 |
| --- | --- |
| CPU | 12th Gen Intel(R) Core(TM) i7-12700KF（20 逻辑核） |
| OS | Windows 10 22H2 |
| 工具链 | rustc 1.92.0 |
| 构建 | `--release`（profile 未改动） |
| 数据目录 | `F:\qs-bench\bench\wcas-baseline`（F: 盘） |
| 时间 | 2026-10-23，单机、无并发负载（除并发场景本身） |

**这不是一次"干净机器"的测量**：同日稍早同一目录测过一轮，下面保留的全部是**修复后**（见 §8）重跑的数字。

## 2. write_at（overlay 路径）

**载荷**：1 000 个文件，每个 64 字节，内容互异（首 8 字节是序号）；文件**先创建**，
写入在热 overlay 上、每次写一个事务（`synchronous=NORMAL`，不 fsync）。

| run | min | median | max | 3 000 次写后 flush |
| --- | --- | --- | --- | --- |
| 0 | 0.126 ms | 0.147 ms | 5.302 ms | 1.438 ms |
| 1 | 0.123 ms | 0.147 ms | 4.812 ms | 1.301 ms |
| 2 | 0.124 ms | 0.142 ms | 4.570 ms | 1.048 ms |

- 中位 **0.14–0.15 ms/次**，比 P1 直写路径（`create`+`write` 一对 0.93–2.76 ms，每次
  fsync 提交）快一个数量级——差别是**每次写不再是提交点**（ADR-006 §8：flush 才是）。
- max 到 5 ms 的那几次是 WAL checkpoint 撞进写路径，不是常态：3 000 次里出现 1–2 次。
- max 列在 P1 报告里已有同类现象（§2.2 的方差），这里只记录不解释成"偶发故障"。

## 3. flush 成本 vs 脏页数量

**载荷**：N 个文件各写 64 字节，然后 flush 一次（`synchronous=FULL` + `wal_checkpoint(FULL)`）。

| N | run 0 | run 1 | run 2 | min / median / max |
| --- | --- | --- | --- | --- |
| 100 | 2.782 ms | 3.085 ms | 2.822 ms | 2.782 / 2.822 / 3.085 ms |
| 1 000 | 1.732 ms | 1.111 ms | 1.590 ms | 1.111 / 1.590 / 1.732 ms |
| 10 000 | 2.823 ms | 3.240 ms | 3.441 ms | 2.823 / 3.240 / 3.441 ms |

- **flush 与脏页数量几乎无关**（1 ms–3 ms，三个数量级都在这区间）。这与 ADR-006 §8
  的设计一致：flush 是"把 WAL 推到盘 + checkpoint"，成本由 WAL 大小和一次 fsync 决定，
  不是逐页提交。
- N=100 略高于 N=1 000 是噪声（三次都在 2.8–3.1，200 次数据的方差盖过趋势）；不宣称
  "N 越小越慢"。

## 4. snapshot / commit

**载荷**：1 000 个文件各 64 字节，内容互异；时间含整次调用（物化 + 发布协议四步）。
`changes=N` 表示快照前改动了 N 个文件。

| changes | snapshot (min / median / max) | commit (min / median / max) |
| --- | --- | --- |
| 0 | 2 334 / 2 384 / 2 391 ms | 1 839 / 1 908 / 2 068 ms |
| 10 | 2 204 / 2 317 / 2 405 ms | 1 787 / 1 795 / 1 977 ms |
| 100 | 2 152 / 2 166 / 2 272 ms | 1 791 / 1 896 / 1 922 ms |

**变更量几乎不影响耗时**：0/10/100 三条曲线重叠。原因见 §5 与 KI-8——物化器每次都全量
遍历并重编码，未变部分靠内容寻址变成 `AlreadyPresent`，**省了写盘但没省 CPU**。

### 4.1 首次 vs 后续快照（同一棵不变的树）

| run | 首次 | 后续（无变更） |
| --- | --- | --- |
| 0 | 7 945 ms | 2 332 ms |
| 1 | 8 152 ms | 2 372 ms |
| 2 | 9 305 ms | 2 164 ms |

- 首次约 8–9 秒，后续约 2.2–2.4 秒：**首次要为每个对象写文件 + fsync + 索引行**，
  后续全部命中 `already_present`，只剩"读 overlay + 编码 + 查索引"。
- 后续快照仍有 2.3 秒，即"全量物化"的固定成本（约束见 KI-8）。这是 P2a 最重要的
  性能结论：**同一棵树第二次发布接近免费（不写盘），但仍需一次全量遍历**。

## 5. read_at 三条路径

**载荷**：1 000 个 64 字节文件，每路径每次 1 000 次读（表内是**单次读**的平均）。

| run | hot（overlay） | base（overlay 空、读快照） | history（`read_view`） |
| --- | --- | --- | --- |
| 0 | 3.553 µs | 3.251 µs | 0.878 µs |
| 1 | 3.811 µs | 3.587 µs | 0.890 µs |
| 2 | 3.442 µs | 3.125 µs | 1.056 µs |

- 三者都是**微秒级**（注意单位：µs，不是 ms）。
- `history` 最快：`read_view` 不碰 overlay 库，直接沿快照读；但它每次调用都重开
  `SnapshotReader`（一次根对象读），在 1 000 次循环里被摊薄。
- `hot` 与 `base` 差异在噪声内：base 路径是"overlay 查不到 → 落到快照"，一次 64 字节
  读只取一个 extent/Blob。

## 6. 存储效率

**载荷**：1 000 个文件各 64 字节，内容互异（**不构成去重场景**）；第二次提交改动 1 个文件。

| run | logical | canonical | 首次提交后 stored | 第二次提交后 stored | 第二次新增 |
| --- | --- | --- | --- | --- | --- |
| 0/1/2 | 64 000 | 64 000 | 160 524 | 165 970 | 5 446 |

- `logical = canonical = 64 000`：内容就是用户写的那 64 KB，没有额外编码。
- `stored` 约 **2.5×** logical：差额是快照结构（inode 表页、目录页、105 字节根、对象信封
  14 字节/个）。1 000 个 64 字节文件 = 1 000 个 Blob 对象，小对象信封与小页开销占大头。
  **这是小文件的最坏情况**；文件越大，开销占比越低（P1 报告 §3.2 有大文件对照）。
- 第二次提交只增 **5 446 字节**：改了一个文件，旧的树页全部复用。这就是内容寻址带来的
  增量存储——没有为整棵树重新付钱。

## 7. 复现命令

```sh
# 环境（R4）
export WCAS_BENCH_ROOT=F:/qs-bench/bench

# 四件套（常规回归，须 ≤2 分钟）
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
cargo doc --workspace --no-deps

# 性能（每场景 3 次重复）
cargo run -p fs-api --release --example baseline -- write    --repeat 3
cargo run -p fs-api --release --example baseline -- flush    --repeat 3
cargo run -p fs-api --release --example baseline -- snapshot --repeat 3
cargo run -p fs-api --release --example baseline -- read     --repeat 3
cargo run -p fs-api --release --example baseline -- storage  --repeat 3
cargo run -p fs-api --release --example baseline -- everything --repeat 3

# 验收矩阵
cargo test -p fs-api --test acceptance
cargo test -p fs-api --test acceptance -- --ignored a1_     # A1（重场景，约 14 s）
```

## 8. 过程记录：这张卡改了两个缺陷

本报告的第一轮数字暴露出两个真实缺陷，修好之后才重跑。数字与结论都基于**修复后**的代码。

### 8.1 一个 workspace 只能新建 1 024 个文件（P2a-6 的缺陷）

第一轮 flush 场景在 N=10 000 时失败：

```
create: invalid overlay record in ov_meta.serial_next: reserved range is exhausted at 1025 (end 1025)
```

成因：`WorkspaceStore::top_up`（低水位补段）只挂在 `TreeStore::write` 上，而
`create`/`write_at`/`truncate` 走的是 overlay 自己的内容 API，**从不经过 `write`**。
于是 segment 用尽后没有任何补段机会。修复：`fs_workspace::Workspace` 增加
`create_file`/`write_at`/`truncate`/`read_at`/`content_size`，内容操作经它调用
（先 `top_up` 再委托），`fs-api` 全部改走这些方法。回归测试
`a_workspace_can_create_more_files_than_one_serial_segment`（2 500 个文件）。

### 8.2 小文件全零时快照非法（P2a-6 的缺陷）

同一个场景暴露出：

```
first commit: Snapshot(Malformed { detail: "inode 1: a file smaller than one block must be a Blob" })
```

成因：物化器把"小于一块且全零"的文件编码成 `Empty`。ADR-005 §3.2 规定
`0 < size < block_bytes` **必须是 Blob，即使字节全零**（`Empty` 只用于 size=0 或
"≥ 一块且全零"）。修复在 `fs-workspace/src/materialize.rs::encode_file`。

### 8.3 十路并发发布有 2/10 提前失败（P2a-6 的缺陷，A1 抓出）

验收矩阵 A1（10 workspace 同时发布）第一轮：2 个线程拿到 `Busy { waited_ms: 5000 }`，
但**只过了约 200 ms** 就返回——5 秒的有界等待根本没被用上。

成因：`record_commit` 用 `unchecked_transaction()`（`BEGIN DEFERRED`），先 `SELECT head_commit`
拿到读快照，再 `INSERT` 升级成写锁。WAL 下升级冲突返回 `SQLITE_BUSY_SNAPSHOT`，SQLite
**不会为它调用 busy handler**，所以等待时间形同虚设。这正是 ADR-006 §9.1 写
`BEGIN IMMEDIATE` 的原因。修复：两处发布事务（snapshot 与 commit）改成
`transaction_with_behavior(Immediate)`。回归测试
`a_publication_waits_for_the_write_lane_before_reading_the_head`（另一连接持有写锁 300 ms，
断言本次发布会等待并成功）。

修复后 A1 连续 3 次通过（每次约 14 s）。

### 8.4 沉淀

三条缺陷全部来自"**量级一变大就崩**"：1 024 是预留段大小、`size < block` 是归一规则边界、
`BEGIN DEFERRED` 只在有竞争时才暴露。三个场景（N=10 000、全零小文件、十路并发）都是
P2a-8 的规模，而不是每日回归会碰到的形状——这是验收卡与性能报告本身的价值。
