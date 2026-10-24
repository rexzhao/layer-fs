# ADR-008：Pack 存储、批量发布与离线 GC

- 状态：**草案（Proposed），分叉已决**——architect 起草，P2b-2/P2b-6 的设计输入；worker 落地核对后转
  Accepted。分叉 G2/G3/G4 的 owner 结论见 §2.1
- 日期：2026-10-23
- 起草：architect
- 依据：spec §2 P2.11、§4（Seal/GC 顺序、故障注入点）、§5.4；ADR-002（Store 布局、发布顺序、恢复）；
  ADR-006 §12（批量发布草案）、§15（GC 根集合）、F7；KI-7 处置（fsync 在索引事务之外）；
  p2a-baseline §4.1（首次 snapshot 约 8 ms/对象）
- 修订：**修订** ADR-002 §3（布局）、§4（schema v3）、§5（发布顺序）、§7（恢复）；**取代** ADR-006 §12
  （批量发布改为 pack 批量）；兑现 ADR-006 §15

---

## 1. 背景与问题

- 每个对象一个文件，每次 put 一次文件 fsync + 一次目录 fsync + 一次索引事务：首次 snapshot 1 000 个
  小文件 8–9 s（p2a-baseline §4.1）。ADR-006 §12 的"批量索引"只能去掉索引事务，**去不掉每对象的文件
  fsync**——F7 的"先批量、实测、不达标上 pack"已被 P2a 数据回答 **[G4 已定]**；
- CDC（ADR-007）使对象平均 16 KiB，对象数比 64 KiB 固定块多约 4 倍，每对象一个文件更不可行；
- P2 必须有"不回收被 Root、活跃 Workspace 或未完成事务引用的数据"的最小正确 GC（spec P2.11）。

## 2. 决策摘要

| # | 决策 |
| --- | --- |
| P1 | 新对象一律写入**不可变 pack 文件**；一批（一次物化 / 一次 Seal 运行）→ 一个或少数几个 pack |
| P2 | 发布顺序：去重 → 写 tmp pack → fsync → rename → fsync 目录 → **一个短 `BEGIN IMMEDIATE` 事务**写入这一批索引行（fsync 全在事务外，KI-7） |
| P3 | cas-store schema v3：`object_location` 增加 `kind`/`pack_id`/`offset`；loose 对象读路径保留，写路径只写 pack；v2 Store 拒绝 **[G4 已定]** |
| P4 | 无索引行引用的 pack 文件 = 崩溃残留：普通打开只读不删（ADR-002 §7.1 精神），GC 时删除 |
| P5 | GC **离线**：独占打开主库与全部 overlay；有任何使用者即拒绝 **[G3 已定]** |
| P6 | 根集合 = 提交（各 branch head 的祖先 ∪ `branch_log` 出现过的提交的祖先）∪ `snapshot` 表 ∪ 每个 workspace 的 `base_root`、root(`base_commit`)、overlay `pending_root`、全部 `sealed_content` |
| P7 | 保留策略：P2b **不做过期**；新增 `snapshot drop`；删除 branch 保留其 `branch_log` **[G2 已定]** |
| P8 | mark 用临时 SQLite 文件存标记集（不全量驻留内存，spec §5.4）；sweep：loose 直接删，pack 存活率低于阈值则**拷贝式重打包** |
| P9 | 每一步：新数据先落盘 → 索引原子切换 → 旧数据删除；任意点崩溃只留多余文件，绝无悬空引用 |

### 2.1 owner 决策记录

以下分叉均由 **owner 于 2026-10-23 通过 qclaw@2qcuc12p 确认**按 architect 建议执行（经 manager 转达，
mailbox `manager/c7yba9ud6s`、`manager/taek3xts1d`）：

| 分叉 | 结论 | 落在本 ADR |
| --- | --- | --- |
| G2 GC 保留策略 | 不做历史过期；只回收孤儿、被 drop 的 snapshot、被 discard 的 workspace 的封存内容；`branch_log` 永久保留 | P7、§6.2 |
| G3 GC 方式 | 离线：独占打开，有使用者即拒绝 | P5、§6.1 |
| G4 pack 与 schema | pack 直接上；cas-store schema v3，v2 Store 拒绝、不迁移（存储格式变更已批准） | P1–P3、§3–§5 |

**与 ADR-006 F7 的衔接**（owner 指出，按 manager 与 architect 的判断处理）：F7 定的是"先做批量发布、实测、
不达标再上 pack"。P2a 实测（首次 snapshot 约 8 ms/对象，瓶颈是每对象一次文件 fsync）已经回答了"是否
达标"，所以 F7 的"批量发布"阶段**提前收口**：不再单独实现 ADR-006 §12 的"对象文件逐个 durable + 索引
分块提交"，直接以本 ADR §5 的 pack 批量发布取代。ADR-006 §12 标注"已由 ADR-008 §5 取代"。

## 3. Pack 格式

```text
<store>/packs/<pack_id>.pack        pack_id = 随机 128 bit hex（名字不承载内容身份）
  对象帧 × n                         每帧 = ADR-001 的完整规范帧（WCAS 头 + 值），首尾相接
  尾部：magic "WPK1" | u32 count | u64 frames_len | 32 B BLAKE3(全部帧字节)
```

- 尾部校验用于 `verify` 与 GC 扫描整个 pack；读单个对象**不**读尾部，按索引的 `(offset, length)` 读一帧，
  照旧解码并校验身份（ADR-002 §6）——单对象读无放大；
- pack 写完即不可变；只会被整体删除（GC）；
- 单个 pack 的大小上限 64 MiB（写入 `store_policy.pack_max_bytes`），一批超出则顺序切成多个 pack。

## 4. 元数据 schema v3（cas-store）

```sql
CREATE TABLE object_location (
    oid TEXT NOT NULL PRIMARY KEY,
    role INTEGER NOT NULL,
    length INTEGER NOT NULL,          -- 帧长
    kind INTEGER NOT NULL,            -- 0 = loose（P1/P2a 形态，只读），1 = pack
    location TEXT NOT NULL,           -- loose: 相对路径；pack: pack_id
    offset INTEGER NOT NULL           -- loose: 0；pack: 帧在 pack 内的偏移
) STRICT;
CREATE TABLE pack (
    pack_id TEXT NOT NULL PRIMARY KEY,
    bytes INTEGER NOT NULL,
    objects INTEGER NOT NULL,
    created_ns INTEGER NOT NULL
) STRICT;
```

`store_policy.schema_version = 3`；**任何 ≠ 3 的 Store → `UnsupportedSchemaVersion`**，不迁移（P2a 零部署）**[G4 已定]**。

> **编号澄清（architect 落地核对，2026-10-24）**：P2a 实际现存的是 `SCHEMA_VERSION = 1`（`crates/cas-store/src/schema.rs`，
> `PRAGMA user_version = 1`），**2 从未发布**；G4 里的"v2 Store 拒绝"应读作"任何非 3 版本拒绝"，
> 编号仍是 3（owner 已确认）。因此验收夹具必须用**真实的 P2a v1 DDL**（5 列 `object_location`、无
> `kind/pack_id/offset`），不是把新 DDL 的 `user_version` 改成 2 的合成库——合成 v2 恰好会漏掉真实旧库。
> 另：`cas-store/src/open.rs` 现在是 `configure()`（含 `journal_mode = WAL`）在 `validate()` **之前**，
> 所以"被拒时文件不得被改动"需以字节哈希断言验证，红了就把 `validate` 提前或先以只读连接读版本。

## 5. 批量发布（取代 ADR-006 §12）

```rust
let mut batch = store.batch();               // PageSink 实现
let oid = batch.put(role, value)?;           // 内存去重；已在索引中 → AlreadyPresent，不写
batch.commit()?;                             // 下面的 1–4
```

1. 未在索引中的对象按 put 顺序写入 `tmp/<pack_id>.pack`（含尾部），`sync_file`；
2. rename 到 `packs/`，`sync_directory(packs/)`；
3. **一个** `BEGIN IMMEDIATE` 事务：插入 `pack` 行与该 pack 全部 `object_location` 行（`INSERT OR IGNORE`：并发
   发布同一对象时，先提交者胜，后者的那一帧成为 pack 内的死字节，由 GC 重打包回收）；
4. 超过 `pack_max_bytes` 时 1–3 按 pack 顺序重复——子对象所在 pack 的索引先于父对象提交，
   "已发布对象只引用已发布对象"成立。

- **fsync 全在事务外**，事务只含 INSERT：持锁时间与对象数无关（KI-7 纪律；P2b-2 记录最长单次锁持有作证据）；
- 物化与 Seal 的 PageSink 改用 batch：一次 snapshot 通常只产生一个 pack，一次 fsync；
- `Store::put` 单对象接口保留（内部 = 一个对象的 batch），供测试与零散写入。

### 5.1 崩溃语义（逐步）

| 崩溃点 | 磁盘 | 可见性 | 恢复 |
| --- | --- | --- | --- |
| 1 中 | tmp 下残缺 pack | 无 | 打开时清 tmp（ADR-002 §7 不变） |
| 1 后 2 前 | tmp 下完整 pack | 无 | 同上 |
| 2 后 3 前 | packs/ 下无索引的 pack | 无 | 普通打开只读不删；GC 删除 |
| 3 后 | 完整 | 全部可见 | — |

## 6. GC

### 6.1 离线（P5）

- 打开方式：`Repo::open_exclusive`：主库连接 `locking_mode=EXCLUSIVE` 并立即取写锁；随后**逐个独占打开**
  `workspaces/*/overlay.sqlite`。任何一步失败 → `StoreInUse` / `WorkspaceInUse`，不做任何修改；
- 运行期间其他进程的 `Repo::open` 失败（不等待：GC 可能很长，有界等待没有意义）；
- 理由：在线 GC 的经典竞争——物化中 put 一个"索引里已有"的对象（`AlreadyPresent`，不写），GC 恰好把它
  当垃圾删掉，随后发布的根引用了它 → 悬空。离线排除了一切并发写入者，正确性论证最简单；
  另一个原因：活跃 workspace 的 overlay 被独占锁定，GC 读不到其中的 `pending_root` 与 `sealed_content`。

### 6.2 根集合（P6）

| 来源 | 内容 |
| --- | --- |
| 主库 `branch` | 每个 head 沿 `parent_commit` 链的全部提交的 `root_oid` |
| 主库 `branch_log` | 出现过的每个 `old_commit`/`new_commit` 及其祖先的根（force 覆盖、reset 移走的历史） |
| 主库 `snapshot` | 全部 `root_oid` |
| 主库 `workspace` | `base_root`、root(`base_commit`) |
| 每个 overlay | `ov_meta.pending_root`（非空时）、`ov_inode.sealed_content` 引用的对象（Blob 的 chunk，或 Extents 树根） |

`Repo::roots()`（P2a-6）只覆盖前四行的一部分；P2b-6 扩展为上表，并由 `verify --deep` 共用。

**保留策略（P7）[G2 已定]**：P2b 不做过期。因此 P2b 的 GC 实际回收：

- 孤儿对象：失败或未完成的发布、`NothingToCommit` 的物化产物、Seal 竞争放弃的 chunk、被 discard 的
  workspace 的封存 chunk、`BranchMoved` 后的物化产物；
- `snapshot drop <root>` 删除的 snapshot 行之后不再可达的对象；
- 崩溃残留的 pack 与 loose 孤儿文件、pack 内的死帧（重打包）。

### 6.3 算法

```text
mark：
  建临时库 gc-<id>.sqlite（marked(oid PRIMARY KEY)）
  从每个根出发，经 fs-snapshot 读取器遍历：root → inode 表页 → 目录树页 → extent 树页 → chunk
  已标记的对象不再展开（共享子树只走一次）
sweep：
  a) loose：不在 marked 中 → 删索引行（事务）→ 删文件
  b) pack：统计每个 pack 的存活字节比例
     比例 == 0      → 删全部索引行（事务）→ 删 pack 文件
     比例 < 0.5     → 重打包：存活帧写入新 pack（tmp → fsync → rename → fsync 目录）
                      → 一个事务：插入新 pack 行、把这些 oid 的索引行改指向新 pack、删旧 pack 行
                      → 删旧 pack 文件
     其余           → 保留（死字节留到存活率进一步下降）
  c) 删除 packs/ 下无 pack 行的文件、objects/ 下无索引行的 loose 文件、tmp/
  d) 删除临时标记库
```

阈值 0.5 写入 `store_policy.gc_repack_threshold`。

### 6.4 崩溃安全（P9，spec §4"GC 标记/回收之间"）

| 注入点 | 状态 | 下次打开 / 下次 GC |
| --- | --- | --- |
| mark 中 | 只写了临时标记库 | 临时库丢弃；Store 未改 |
| sweep a) 删索引后、删文件前 | loose 孤儿文件 | 下次 GC 的 c) 删除 |
| 重打包：新 pack 落盘后、切换事务前 | packs/ 下无 pack 行的新 pack | 下次 GC 的 c) 删除；旧 pack 仍被引用 |
| 重打包：切换后、删旧 pack 前 | 旧 pack 文件无 pack 行 | 下次 GC 的 c) 删除 |
| c) 中 | 部分孤儿已删 | 下次 GC 继续 |

每个注入点之后：重开（非独占）→ `verify --deep`（全部根）干净 → 全部根可读、逐字节等于 GC 前。

### 6.5 CLI 与 API

```text
fs-cli --store <s> gc [--dry-run]           # dry-run 只做 mark 与统计
fs-cli --store <s> snapshot drop <root>
fs-cli --store <s> branch delete <b>        # 保留 branch_log（P7）
Repo::open_exclusive(dir) -> MaintenanceRepo { gc(options) -> GcReport, ... }
GcReport { roots, marked_objects, freed_objects, freed_bytes, packs_repacked, packs_deleted, duration }
```

## 7. verify 的扩展

- 单对象：按 `kind` 读 loose 或 pack 帧，校验身份；
- pack：尾部 BLAKE3 校验；索引指向的 `(offset, length)` 在 pack 范围内且恰好一帧；
- `verify --deep`：对 §6.2 的全部根执行 ADR-005 V1–V6（含 overlay 的封存引用）。

## 8. 未实现 / 推迟

| 项 | 去向 |
| --- | --- |
| 在线 GC（epoch / 写屏障） | P3 之后 |
| 历史过期（reflog/snapshot 保留期） | owner 定策略后 **[G2 已定]** |
| pack 压缩、delta | 非 P2 |
| loose → pack 的批量迁移 | 不需要（v2 Store 拒绝；v3 新对象全部进 pack） |

## 9. 测试清单

1. pack 写读往返；单对象读只读一帧（读字节计数）；尾部校验损坏被 verify 报出；
2. 批量发布 §5.1 每个崩溃点；fsync 不在事务内（记录最长单次锁持有时间）；
3. 并发两个 workspace 发布相同对象：`INSERT OR IGNORE`，两边都成功，索引唯一；
4. 首次 snapshot 1 000 文件耗时与 P2a 基线（8–9 s）对照，三次重复；
5. GC：有使用者时拒绝；孤儿被回收、全部根仍逐字节可读；`snapshot drop` 后对应对象被回收；
   共享子树只回收不再可达的部分；
6. §6.4 每个注入点；
7. 重打包：存活率阈值两侧各一例；重打包后的 pack 统计与读路径；
8. 参考模型随机序列（P2b-0）中插入 gc（关闭 → GC → 重开），断言视图与全部历史根不变。
