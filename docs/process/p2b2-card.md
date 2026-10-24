# P2b-2 任务卡草稿（pack + 批量发布 + cas-store schema v3）

> 状态：**已派发**（2026-10-24，`worker/q3wo48e9v2`）。基座 main `3490c30`。R3-1 已合入。
> 依据：ADR-008 §2（P1–P4）、§3（pack 格式）、§4（schema v3 + 编号澄清）、§5/§5.1（批量发布与崩溃语义）、§7（verify 扩展）、§9 测试 1–4；KI-7 纪律。
> 不在本卡：GC 全部内容（§6、测试 5/6/7）→ P2b-6；`snapshot drop` / `branch delete` CLI（§6.5）→ P2b-6。

## 0. 前置

- R3-1 合入后从最新 main 重建分支（旧的 `p2b/pack` 停在 `bc87371`，**删掉重开**）。
- 不动 main、不 push；四件套全绿。

## 1. 范围（逐条对应 ADR-008）

1. **cas-store schema v3**（§4）：`object_location` 增 `kind`/`location`/`offset`，新增 `pack` 表；`store_policy.schema_version = 3`；**任何 ≠3 的 Store → `UnsupportedSchemaVersion`**，不迁移（G4）。loose 读路径保留，写路径只写 pack。
   - **编号事实**：现存是 `SCHEMA_VERSION = 1`，2 从未发布；G4 的"v2 拒绝"= 任何非 3 拒绝（ADR-008 §4 澄清，`5d493d2`）。
   - **夹具**：必须用**真实 P2a v1 DDL**（5 列 `object_location`、无 `kind/location/offset`），不是合成 user_version=2 的新库。
2. **pack 写读**（§3）：帧 = ADR-001 规范帧首尾相接；尾部 `magic "WPK1" | u32 count | u64 frames_len | 32B BLAKE3`；单 pack ≤ 64 MiB（`store_policy.pack_max_bytes`），超出顺序切多个。单对象读**不读尾部**，按 `(offset, length)` 读一帧后校验身份。
3. **批量发布**（§5）：`store.batch()` 实现 PageSink；内存去重（`AlreadyPresent` 不写）；tmp pack → fsync → rename → fsync 目录 → **一个短 `BEGIN IMMEDIATE`**（fsync 全在事务外，KI-7）；超限时 1–3 按 pack 顺序重复，保证"已发布对象只引用已发布对象"。`Store::put` = 单对象 batch。
4. **物化与 Seal 切到 batch**：一次 snapshot 通常一个 pack、一次 fsync。
5. **verify 扩展**（§7）：pack 尾部 BLAKE3；索引 `(offset, length)` 在范围内且恰好一帧。`verify --deep` 全根遍历 → P2b-6。
6. **ADR-008 落地核对**：差异逐条列出（含 §9 未覆盖项），据此把 ADR-008 从 Proposed 转 Accepted 或提修订点给 architect。

## 2. 验收（可判定条件，régime：不给"整体通过"）

### 2.1 schema / 拒绝路径
- 被拒版本集合 `{0,1,2,4}`：1 用**真实 P2a v1 库**，其余合成；全部返回 `UnsupportedSchemaVersion { found, supported: 3 }`。
- **同一夹具连续两次打开都失败**（证明无半迁移）。
- **字节不变**：被拒前后对 `cas.sqlite` 取哈希必须相同；打开前放一个伪 `tmp/` 文件，被拒后它仍在（`clear_temporaries` 在 `validate` 之后）。
- 若哈希断言在当前 `configure() → validate()` 顺序下失败：把 `validate` 提前，或先以只读连接读 `user_version`（ADR-008 §4 已写入该风险）。

### 2.2 pack 写读
- 往返：写入 → 索引 kind=pack → 读回逐字节相等；尾部 BLAKE3 损坏被 `verify` 报出（给出具体错误）。
- **单对象读只读一帧**：用读字节计数或 IO 计数器证明，不是靠"看起来没读"。
- `pack_max_bytes` 两侧各一例（恰好一个 pack / 恰好两个 pack），第二个 pack 的索引顺序在父对象之前。

### 2.3 批量发布与崩溃
- §5.1 四个崩溃点逐个注入；每个注入点后重开 → `verify` 干净 + 全部根可读。
- **"fsync 全在事务外"必须可观测证明**（architect 判据，待补全为断言形式）：① 事件序列探针，断言 `txn_begin..txn_commit` 之间零 sync；② 锁持有时间的**规模不变性**（10 对象 vs 1000 对象，单次 `BEGIN IMMEDIATE` 最长持有不随对象数线性增长）；③ 每批 sync 次数符合 §5（每 pack 1 次 file + 1 次目录），并记录最长单次锁持有原始数据。
- 并发两个 workspace 发布同一对象：`INSERT OR IGNORE`，两边成功、索引唯一。

### 2.4 性能
- 首次 snapshot 1000 文件 vs P2a 基线 8–9 s（`p2a-baseline` §4.1），三次重复给原始数据。
- **R3-1 遗留嫌疑**：若变慢，先查"`write_at` + `truncate` 到同长"是否每文件多一次 4 KiB 页 UPDATE（`truncate` 跨界页"存在即写回"），修法 `data[keep..].iter().any(|b| *b != 0)` 守卫，并补一条"同长 truncate 不产生页写入"的断言。

### 2.5 回归
- 既有 loose 读路径测试全过；`SnapshotReader::open_any` 的 v1/v2 profile 兼容不变。
- 参考模型常规序列 + 崩溃前缀断言全绿；soak 仍绿。

## 3. 报告要求

提交说明：schema 变更点、崩溃点清单与注入证据、sync 事件序列与锁持有原始数据、性能对照表、与 ADR-008 的差异。交付后 compact。


## 4. 追加验收：architect 判据 A–E（arches，2026-10-24）

> 来源：`architect` 对 P2b-2 的判据（`F:\workrch-truncate
otes\p2b2-fsync-lock-criteria.md`，尾部长度已按
> ADR-008 §3 裁决为 **48 B**，原写 44 是算术错误）。

- **A 探针**：埋在生产函数内（`durability.rs::{sync_file, sync_directory}`、`rename_replace`、pack `finish()` 后）；
  `sync-probe` feature 默认关 + rusqlite `trace` 作事务边界独立见证；探针只做原子自增/预分配槽位，不得
  sync/IO/sleep、不得改事务边界；durability 与 trace 共用 `AtomicU64` 序号；断言"观测到预期数量事件"；
  **阳性对照** `inject_sync_inside_next_transaction` 必须让同一检查器判红。
- **B 事件模板**（per pack）：`PackWrite → SyncFile → Rename → SyncDir → TxnBegin → INSERT* → TxnCommit`；
  违反清单 V1–V7 每条独立断言（事务内出现 sync / 块内顺序错 / 事务重叠或计数不等 / 事务内非 INSERT /
  同一 pack 重复 sync 或事务 / 新增对象却无 `PackWrite` / `SyncDir` 每批一次）。
  `COMMIT` 时 SQLite 自身对 WAL 的 fsync **不算违反**。
- **C 计数**：`k = max(1, ceil(Σframe_len / pack_max_bytes))`；
  `PackWrite == SyncFile == SyncDir == TxnBegin == TxnCommit == k`；`sync 总数 == 2k`（>2k 即 FAIL）；
  `SyncFile < objects`（N≥2）；`Σ pack.objects == 新对象数`；`Σ pack.bytes == Σ帧长 + 48k`。
  两侧用例（调小 `pack_max_bytes`）：2/3/5 × 1000 B → k = 1/2/3 → sync = 2/4/6。
  **`packs/` 目录首次创建的一次性 sync 不得计入批次**：首批与次批都必须是 `2k`；若首批为 `2k+1`，说明目录延迟
  创建，应挪到 `open/create`。
- **D 锁持有规模不变性**：`H(n)` 为 `BEGIN IMMEDIATE` 前到 `COMMIT` 后的墙钟，6 轮丢首轮取 max 再中位；
  自校准 `S` = 20 次 `sync_file` 中位；判据 `m = (H(1000) − H(10))/990 ≤ S/10` 且 `H(1000) ≤ 100·S`；
  `H(1000) < H(10)` 视为通过。报告给 `S/H(10)/H(1000)/m` 与 6 轮原始值。
- **E pack 尾部与单对象读**：`count` = 物理帧数；`frames_len` = 帧长和（不含 48 B 尾部）；单写者时
  `count == 行数 == pack.objects`、`frames_len + 48 == 文件大小`；并发（`INSERT OR IGNORE` 跳过）时
  `count ≥ 行数`（差额是死字节），断言写"≥"；空 pack 一律 FAIL。单对象读两条同时成立：总读字节 == `length`；
  读取区间并集 ⊆ 帧区间且不含尾部 48 B——观测点必须在**读取器生产路径**（`read_at` 调用处），不能在
  `Store::get` 返回处。

## 5. 追加约束：`tmp/` 并发清扫（ADR-002 §7.0 / ADR-008 §5.1，main `3490c30`）

- staging 必须把 `tmp/` 视为"随时可能被清空"：失败后**以新临时名从零重写**，不得续写；
- 重试有界（20×2 ms），**仅** `NotFound` / `PermissionDenied`，只包 staging 一段，不包 DB 事务/读路径，
  耗尽报**原始错误**；
- 回归：① N 线程发布同一对象（索引唯一 + pack 存在 + `verify` 干净，报偶发率与重试次数，20 连跑）；
  ② "opener 清扫 vs 发布"（测试里手动清 `tmp/` 模拟并发 opener）；
- **探针按 attempt 作用域**：失败尝试零 `TxnBegin`，只有成功尝试进 V1–V7 与 `2k` 计数。
- GC 标记库不得放进 `tmp/`（ADR-008 §6.3，实现归 P2b-6）。

## 6. 验收口径补充（2026-10-24，manager 裁决）

- **E 的死字节按物理帧计**：并发 `INSERT OR IGNORE` 跳过时，被跳过的帧仍占字节。因此
  `Σ pack.bytes == Σ(物理帧长) + 48k`，而 `object_location` 行数可少于 `count`。`count ≥ 行数` 是正确断言，
  **不得**断言相等（参见 KI-10：`verify` 曾因要求"铺满"而把健康 Store 判为损坏）。
- **D 的边缘处理**：Windows 上 `S`（20 次 `sync_file` 中位）自身可能抖动。判据仍是 `m ≤ S/10` 且
  `H(1000) ≤ 100·S`，但**数据齐、阈值边缘（2 倍以内）**时记为"观察项"而非 FAIL；"每对象一次 fsync"
  （`m ≈ S`）这种数量级差异仍必须判红。
- **A 的探针归属**：允许验证方在**自己的 worktree**里加 additive、feature 默认关闭的探针来独立观测，
  条件是证明"开/关探针时 pack 字节、索引行与 `verify` 结果完全一致"；能交叉核对的部分（事务边界）必须
  用 `rusqlite::trace` 这种**不改产品代码**的观察器复算。
