# P2b-2 任务卡草稿（pack + 批量发布 + cas-store schema v3）

> 状态：草稿，等 R3-1 合入后正式派发。
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
