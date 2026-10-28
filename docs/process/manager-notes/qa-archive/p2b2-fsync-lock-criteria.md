# P2b-2 验收判据：fsync 全在事务外 + 锁持有规模不变性 + pack 边界

来源：architect，对 ADR-008 §5/§5.1、KI-7。对象：`crates/cas-store`（batch 发布）与集成测试。
每条都写成"哪个断言会红"。与卡冲突时先回 manager 裁决，不得在测试里放宽。

---

## A. 探针：必须测生产路径

- **钩子**：直接埋在生产函数内——`durability.rs::{sync_file, sync_directory}`、`rename_replace`、pack 写入完成点（`finish()` 之后、`sync_file` 之前）。
- **开关**：照 `Cargo.toml` 已有的 `crash-points` 先例加 `sync-probe`（默认关）。同一 feature 给 rusqlite 开 `trace`（`sync-probe = ["rusqlite/trace"]`），事务边界用 SQLite 自己报的 `BEGIN IMMEDIATE`/`COMMIT` 作**独立见证**。
- **副作用**：只做原子自增 + 预分配槽位。不得在探针里 sync/IO/sleep，不得自己开/提交事务，不得改变事务边界或写路径。
- **排序**：durability 事件与 trace 事件共用一个 `AtomicU64` 序号，按序号排序（不要用墙钟）。
- **非真空性**：测试同时断言"观测到了预期数量的事件"（`PackWrite ≥ 1`、`SyncFile ≥ 1`、`TxnBegin ≥ 1`、INSERT 行数 == 预期新对象数）。探针没接线时这些先红。
- **阳性对照**：同 feature 下提供 `#[doc(hidden)] probe_inject_sync_inside_next_transaction(&File)`，在事务内对 scratch 文件调用**同一个** `sync_file`。同一检查器必须**失败**；B 的 V1 与 D 的判定式都要各自被它判红一次。

## B. 事件序列模板与违反清单

ADR-008 §5 第 4 条是"**每个 pack 一个事务**"，模板 per-pack：

```text
per pack p（按 pack 提交顺序）：
  PackWrite(p)   # tmp/<p>.pack 写完（帧缓冲 finish 后）
  SyncFile(p)    # sync_file(tmp/<p>.pack)
  Rename(p)      # tmp/<p>.pack → packs/<p>.pack
  SyncDir(packs) # sync_directory(packs/)
  TxnBegin(p)    # trace: "BEGIN IMMEDIATE"
  SqlStmt*       # 只允许 INSERT / INSERT OR IGNORE INTO pack|object_location
  TxnCommit(p)   # trace: "COMMIT"
Batch = 上述块按 pack 顺序拼接
```

违反即失败（每条独立断言，消息带 pack_id + 序号）：

- **V1**：任何 `PackWrite|SyncFile|Rename|SyncDir` 落在某组 `TxnBegin..TxnCommit` 之间。
- **V2**：块内顺序错（`SyncFile` 先于 `PackWrite`、`Rename` 先于 `SyncFile`、`SyncDir` 先于 `Rename`、`TxnBegin` 先于 `SyncDir`）。
- **V3**：事务重叠/嵌套（`TxnBegin(p2)` 在 `TxnCommit(p1)` 之前），或 `TxnBegin` 数 ≠ `TxnCommit` 数。
- **V4**：`BEGIN..COMMIT` 之间出现非 INSERT 语句（`PRAGMA`/`UPDATE`/`DELETE`/`SELECT`）。
- **V5**：同一 pack 出现 >1 次 `SyncFile`/`SyncDir`/`TxnBegin`/`TxnCommit`。
- **V6**：新增对象 ≥1 的批次 `PackWrite == 0`（探针没接线）。
- **V7**：`SyncDir` 每批一次而非每 pack 一次（偏离 §5 第 2/4 步）。

**不算违反**：`COMMIT` 时 SQLite 自己对 WAL 的 fsync（`synchronous=FULL` 固有）。判据只看本仓库的 sync/rename。

**AlreadyPresent / 去重**：全部已存在 → 0 pack、0 sync、0 事务（若仍开事务，须 0 条 INSERT 且仅一次 BEGIN/COMMIT，记为观察）；部分新增 → 事件只覆盖新对象；同批重复 put 同一对象只产生一帧。

## C. 计数（含 `pack_max_bytes` 两侧；目录一次性项）

记 k = 实际 pack 数，`k_expected = max(1, ceil(Σframe_len(新对象) / pack_max_bytes))`：

```text
PackWrite == k == k_expected
SyncFile  == k
SyncDir   == k
TxnBegin  == k == TxnCommit
sync 总数 == 2 * k          # > 2k 即 FAIL
SyncFile  <  objects        # N ≥ 2 时成立：反"每对象一次 fsync"
```

两侧用例（测试里把 `store_policy.pack_max_bytes` 调小，帧长固定）：

| 对象 | M | k | sync 总数 |
| --- | --- | --- | --- |
| 2 × 1000 B | 2400 | 1 | 2 |
| 3 × 1000 B | 2400 | 2 | 4 |
| 5 × 1000 B | 2400 | 3 | 6 |

另断言：`Σ pack.objects == 新对象数`；`Σ pack.bytes == 帧字节总和 + 48×k`。

**`packs/` 目录首次创建的一次性 sync：不单列、不允许出现在批次计数里。**
`packs/` 属布局，应在 `Store::create` / 打开恢复时建好并 sync（ADR-002 §7）。判据：**首批与次批都必须是 `2k`**；若首批 `2k+1`（延迟建目录）→ FAIL，要求把创建挪到 open/create；挪不动则先找 manager 裁决，测试仍按 steady-state 断言并在报告标注偏差。

## D. 锁持有的规模不变性（判定式）

- **量**：`H(n)` = 从 `BEGIN IMMEDIATE` 发出前到 `COMMIT` 返回后的墙钟时长，单事务、n 个唯一对象（avg 16 KiB）。6 轮，丢弃第 1 轮，每轮取最大值再取中位数；`--release`、该测试 `--test-threads=1`。
- **自校准**：`S` = 同进程对 scratch 文件调用 20 次 `sync_file` 的中位耗时（同磁盘）。**不写死 ms 阈值**。
- **判定**：

```text
m = (H(1000) − H(10)) / 990
PASS ⇔ m ≤ S / 10  且  H(1000) ≤ 100 * S
若 H(1000) < H(10)（噪声）→ 视为通过（无增长）
```

理由：诚实实现 `m` 是每行 INSERT（约 1–5 µs），`S` 是单次 fsync（约 0.1–1 ms）；"每对象一次 fsync"会让 `m ≈ S`，被 10× 判红。
**必须报告原始数据**：`S`、`H(10)`、`H(1000)`、`m`、6 轮各自 max（ADR §5 要求"最长单次锁持有作证据"）。

## E. pack 尾部与单对象读（两个边界）

### E1. `count` / `frames_len` 与索引行数

- `count` = pack 文件中的**物理帧数**；`frames_len` = 全部帧长度之和（不含 48 B 尾部）。
- 单写者、无竞争：`count == 该 pack 的 object_location 行数 == pack.objects`；`frames_len == Σ object_location.length`；`frames_len + 48 == 文件大小`；`pack.bytes == 文件大小`。
- 并发（两个 workspace 同时发布同一对象，`INSERT OR IGNORE` 跳过）：帧仍在、行缺失 → `count ≥ 行数`，差额是死字节（GC 重打包回收）。并发用例断言 `count - rows == 被跳过的帧数`，**不要**断言相等。
- **空 pack 不产生**：一批无新对象 → 0 pack、0 sync；不允许写 `count == 0` 的 pack 文件。
- 单帧 pack 合法（`count == 1`），不作额外断言。
- 尾部篡改（任一 magic/count/frames_len/BLAKE3）→ `verify` 报错。

### E2. 单对象读不读尾部

两条同时成立：

1. 总读字节数 `== length`（该对象帧长，含 WCAS 头），不是整个 pack；
2. 读取区间并集 `⊆ [offset, offset + length)`，且**不含**尾部区间 `[file_size-48, file_size)`。

- 观测点：pack 读取器生产路径上 `read_exact_at(offset, len)` 的调用处（与 A 同 feature），记录 `(offset,len)`；不要在 `Store::get` 返回处统计——那证明不了读了多少。
- 允许"先读帧头再读值"的多段读：并集仍须 ⊆ 区间且总字节 == length。
- 反向用例：`verify` 必须读尾部并因篡改报错（证明尾部校验没被优化掉）。

## E3. 重试与 temp 清理（与 A/B/C 的交互，2026-10-24 追加）

`tmp/` 是共享目录、`Store::open` 会清扫它，因此**并发打开可能删掉另一进程正在 staging 的 pack**。
worker 以"有界重试（20×2ms，仅 `NotFound`/`PermissionDenied`）"处理。判据必须适配它：

- **重试必须从零重写**：每次尝试用**新的 temp 名**、重新 `File::create + write_all + sync_file`，
  不得在旧文件上续写/续 offset（旧文件可能已被删）。重试只允许包住 staging 一段（create/write/sync/rename），
  **不得**包住 DB 事务、读路径或已发布数据；重试耗尽要报原始错误，不得静默放弃。
- **探针事件必须带 attempt 标识**：`PackWrite(pack_id, attempt)`。重试会让同一 pack 出现多组
  `PackWrite/SyncFile`；B 的 V5（每 pack 恰好一次）与 C 的 `SyncFile == k` 只对**最后一次成功尝试**成立，
  此前失败的尝试必须断言**没有**任何 `TxnBegin`（即失败尝试从未进入发布阶段）。
- **计数用例分两类**：`单线程、无并发打开`的用例按 C 的严格等式断言；`并发 open + publish` 用例只断言
  V1（事务内零 sync）、事务数与成功 pack 数一致、索引唯一、pack 存在、`verify` 干净，并记录重试次数
  （不要求 `SyncFile == k`，因为失败尝试也有 sync，且都在事务外——这正是可接受的形态）。
- **tmp 的长期归属**：见 review 结论（交 manager）：短期保留重试并写进 ADR；P2b-6 把 `tmp/` 回收收归
  GC（离线独占），常规 `open` 不再清扫，从构造上消除竞争。GC 的临时标记库**不得放在 `tmp/`**（会被
  并发 opener 清扫），应放 store 根下独立名字。

## F. 交付物

1. 事件序列失败消息样例（含 pack_id/序号）与阳性对照的失败输出；
2. `S / H(10) / H(1000) / m` 原始数字（6 轮）；
3. 多 pack 用例的 `k_expected` 与逐 pack 计数；
4. feature 关闭时构建/测试也通过（证明生产路径未被探针改变）；
5. §5.1 四个崩溃点恢复后，同一套 verify/读取断言通过。
