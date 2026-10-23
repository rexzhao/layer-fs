# ADR-005：快照格式——不可变文件系统对象图

- 状态：**草案（Proposed）**——architect 初稿，作为 P2a-1 卡的设计输入；worker 按"先写后码"核对、补测试清单后提交定稿，manager 批准后转 Accepted
- 日期：2026-10-23
- 起草：architect（P2a 架构设计，mailbox 线程 `manager/3yinqic7xe`）
- 依据：spec §2 P2.7/P2.8、§3 逻辑数据模型、§4；Root 审查 `docs/research/2026-10-22-root-model-review.md` Q2 三条 P2 约束；ADR-001 §4.2（role 5–8 预留）/§4.3（16 MiB 上限）/§5.3（D9）；ADR-003 §4.3（serial 永不复用）、§5.1（名字 = 原始字节、≤255 B）
- 相关：ADR-006（workspace / overlay / snapshot 发布协议，本 ADR 只定**格式**，不定**何时、由谁**生成）

本 ADR 定义"一棵文件系统树"作为**纯 CAS 对象图**的规范编码。读一个快照只需要它的根 ObjectId 与
Store 的对象，**不读任何命名空间表**（Root 审查 Q2 约束 1）。

---

## 1. 背景与问题

P1 的树活在可变 SQLite 表里（ADR-003 D+），root 只是发布点记录，旧树不可读。P2 需要：

1. 旧版本在后续修改之后仍可**完整读取**（spec P2.8、§6 P1-4 的完整形态）；
2. 多个 Workspace **共享**同一 base 而不复制（spec P2 交付）；
3. Branch/Commit 以**内容身份**为令牌（Root 审查 Q2 约束 3）。

需要回答：对象图长什么样；页如何切分；参数冻结在哪；如何校验；读取的代价上界。

## 2. 决策摘要

| # | 决策 |
| --- | --- |
| S1 | 对象图 = FilesystemRoot(8) → InodeTable(7) B 树 → 每个目录一棵 Directory(5/6) B 树、每个大文件一棵 Extent(3/4) B 树；文件字节在 Bytes(1) 对象 |
| S2 | **目录绑定与 inode 记录分离**：目录页只存 `name → serial`；size、时间、内容引用都在 inode 表。改文件内容不重写任何目录页；目录改名只改两个目录的页 |
| S3 | **内容决定分页**：页边界只由键的哈希决定（加最小条目数与最大字节数两条护栏）。同一集合 ⇒ 同一棵树 ⇒ 同一根 id（**历史无关**） |
| S4 | 根对象只描述树内容：**不含** parent、提交时间、提交信息（它们属于 commit 行，ADR-006） |
| S5 | 全部参数组成 **snapshot profile**，`profile_id = BLAKE3(描述串)` 写入每个根；读取时 profile 不符即拒 |
| S6 | 不新增 role、不升对象 `version`：只使用 ADR-001 已冻结的 1/3/4/5/6/7/8 |
| S7 | 不变量：**inode 表的键集合 = 从根目录可达的 serial 集合**；目录无环；每个 inode 恰被一个目录项引用（P2 不支持硬链接）。生成方保证，`verify --deep` 校验 |
| S8 | FileId = `(scope_id, serial)`；scope 存在根里；serial 0 = 根目录 |
| S9 | 文件内容三种形态：`Empty`（全零，含 size 0）、`Blob`（单对象，长度 == size）、`Extents`（extent 树，空隙为零） |
| S10 | P2a 的 extent 叶仍指向 64 KiB 固定块；P2b 换 CDC 时**只改切块，不改 extent 树格式** |

## 3. 对象与值编码

全部整数**大端**、定长；`oid` = 32 字节 ObjectId；`name` = `u8 len | len 字节`（1–255，与
ADR-003 §5.1 的 `MAX_NAME_BYTES` 一致；0 长度拒绝）。每个值的第一个字节是**值格式号**
`fmt = 1`（与对象信封的 `version` 无关：信封版本管对象帧，`fmt` 管该 role 的值布局）。

### 3.1 FilesystemRoot（role 8）

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `fmt` | u8 | = 1 |
| `profile_id` | 32 B | §5 |
| `scope_id` | 32 B | 树所属的 serial 作用域（ADR-006 §6 分配） |
| `root_serial` | u64 | 恒为 0（保留字段以免以后改格式） |
| `inode_table` | oid | InodeTable 根页；**永不为空**（根目录总存在） |

定长 1 + 32 + 32 + 8 + 32 = **105 字节**。

### 3.2 InodeTable（role 7）

一个 role 承载叶与分支，用 `node` 字节区分（ADR-001 只给 inode 表留了一个 role）。

叶：`fmt | node=0 | u32 count | count × inode_record`
分支：`fmt | node=1 | u8 height(≥1) | u32 count | count × (u64 first_serial | oid child | u64 subtree_count)`

`inode_record`：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `serial` | u64 | 键，页内严格递增 |
| `kind` | u8 | 1 = Directory，2 = File（与 P1 `Kind` 数值一致） |
| `size` | u64 | 文件逻辑大小；目录恒 0 |
| `created_ns` | i64 | |
| `modified_ns` | i64 | |
| `content` | u8 tag + 载荷 | 0 = Empty（无载荷）；1 = Blob（oid）；2 = Extents（oid，extent 树根）；3 = Dir（oid，目录树根） |

kind 与 content 的合法组合（其余一律解码拒绝）：

| kind | 允许的 content | 约束 |
| --- | --- | --- |
| File | Empty | 读出全零（稀疏文件）；size 范围见下方规范化规则 |
| File | Blob | 对象值长度 **必须等于** size |
| File | Extents | 最后一个 extent 的结尾 ≤ size |
| Directory | Empty | 空目录（不为空目录生成空页） |
| Directory | Dir | 目录树至少一条目 |

**文件形态的规范化规则**（同一字节内容只能有一种编码，否则破坏 S3 的历史无关性）：

| 条件 | 唯一合法形态 |
| --- | --- |
| `size == 0` | Empty |
| `0 < size < block_bytes` | Blob（即使字节全零） |
| `size ≥ block_bytes` 且存在非零块 | Extents（全零块不写，见 §3.4） |
| `size ≥ block_bytes` 且全部块为零 | Empty |

解码器按此表拒绝非规范形态（例如 size < block_bytes 却用 Extents）。

> 与 P1 的差异：P1 的 blob 允许"值短于 size、尾部读零"（ADR-004 §5.3 的读路径容忍）。快照格式
> 收紧为相等。

### 3.3 Directory（role 5 叶 / role 6 分支）

叶（role 5）：`fmt | u32 count | count × (name | u64 child_serial)`
分支（role 6）：`fmt | u8 height(≥1) | u32 count | count × (name first_name | oid child | u64 subtree_count)`

键 = 名字原始字节，**逐字节比较、严格递增**（区分大小写，ADR-001 D8）。

### 3.4 Extent（role 3 叶 / role 4 分支）

叶（role 3）：`fmt | u32 count | count × (u64 logical_start | u64 length | oid object | u64 source_offset)`
分支（role 4）：`fmt | u8 height(≥1) | u32 count | count × (u64 first_start | oid child | u64 covered_end)`

- 键 = `logical_start`，严格递增；`length > 0`；区间**不重叠**（`start_i + length_i ≤ start_{i+1}`）；
- 区间之间的空隙 = 零字节；
- P2a 生成规则：块对齐 64 KiB、`source_offset = 0`、**全零块不写 extent**（留空隙）。这条是规范化
  规则，不是优化：否则"写了零"与"没写"两种历史得到两棵不同的树；
- 上界校验：`length ≤ profile.max_extent_len`、`source_offset + length ≤ profile.max_object_value`
  （按语法全局上限校验，不必读被指对象即可拒绝伪造叶）。

### 3.5 Bytes（role 1）

不变（ADR-001）。

## 4. 分页规则（S3）

同一规则用于四种树（inode 表、目录、extent；各用不同的域串）。

**输入**：严格递增的条目序列。**输出**：一层层的页，直到某层只剩一页（该页即树根）。

叶层切分：顺序扫描，把条目追加进当前页；追加条目 `e` 后，若满足下列任一条件则在 `e` 之后切页：

1. `count ≥ min_entries` **且** `low_bits(BLAKE3(domain_tree || level || key(e)), mask_bits) == 0`；
2. 当前页编码长度 ≥ `max_page_bytes − max_entry_bytes`（字节护栏，保证页不超上限）。

序列结束时剩余条目成为最后一页。上层：对下层每一页取 `(first_key, child oid, subtree_count)`
作为条目，`level + 1` 后用**同一规则**切分。

性质：

- **历史无关**：输出只依赖条目序列本身，不依赖插入顺序或旧树；
- **局部性**：边界只依赖键（不依赖值，护栏 2 除外），改一个条目的值只改它所在叶及其祖先路径；
  插入/删除一个键只扰动相邻一到两页；
- **全量重编码不写盘**：未变页编码出同一 oid，`Store::put` 返回 `AlreadyPresent`（只查索引，不写
  文件、不 fsync）。这是 owner"初版全量物化"可接受的前提，也让以后的增量物化不需要改格式。

> 反例（禁止）：按固定条目数切页。插入一条会让其后所有页错位，全量物化 = 全量重写。

空集合：不生成树，由上层用 `Empty` 表示（目录为空、文件全零）。inode 表永远非空。

## 5. Snapshot profile（S5）

描述串（规范、逐字节固定；任何字符变化 = 新 profile）：

```text
wcas/snapshot/v1;fmt=1;mask_bits=6;min_entries=4;max_page_bytes=16384;
block_bytes=65536;max_extent_len=65536;max_object_value=16777216;
name_max=255;hash=blake3;domains=inode,dir,extent
```

`profile_id = BLAKE3("wcas/snapshot-profile/v1\0" || 描述串)`。

- 目标平均扇出 2^6 = 64，页典型 3–5 KiB，硬上限 16 KiB（远低于 16 MiB 对象上限）；
- `block_bytes` 进入 profile 统一解决 C4（fs-api 与 fs-namespace 两份 `BLOCK_BYTES`）：唯一来源是
  profile，上层读取不复制；
- Store 记录"本 Store 写入用的 profile"（`namespace_policy` 的继任者，ADR-006 §9）；读取遇到
  未知 profile → `UnsupportedProfile`，**不做转换**。

数值是初值，P2a-1 卡用 §9 的基准确认（100k inode、单目录 100k 条目、1 GiB 文件的页数/深度/
编码耗时）；若调整，改的是 profile 描述串，仍在 P2a 内冻结。

## 6. 读取（`fs-snapshot` 读取器）

| 操作 | 代价 |
| --- | --- |
| 打开根：读 role 8，校验 `fmt`、`profile_id` | 1 次 get |
| `inode(serial)` | O(树深) 次 get，树深 = ⌈log₆₄ N⌉（10 万 inode ≈ 3） |
| `lookup(dir, name)` | O(目录树深) |
| `read_dir(dir, after, limit)` | O(树深 + limit/64) |
| `extents_in_range(file, from, to)` | O(树深 + 命中叶数) |
| `read_at` | 上述 + 命中块 get |

每次 `get` 由 cas-store 校验对象身份（ADR-002 §6）；解码校验见 §7。读取器**不缓存为正确性前提**
（spec §5.4）；页缓存是可选优化。

## 7. 解码校验（每次读页都做）

按固定顺序返回第一个失败项，均为类型化错误（ADR-001 D10）：

1. role 不是期望的 role（如 inode 表指针指向 role 5）→ `WrongRole`
2. `fmt` ≠ 1 → `UnsupportedFormat`
3. 截断/尾随字节/count 与长度不符 → `Malformed`
4. 键不严格递增、extent 重叠、`length == 0`、name 长度 0 或 > 255 → `Malformed`
5. kind/content 非法组合（§3.2 表）→ `Malformed`
6. 页编码长度 > `max_page_bytes` → `Malformed`
7. 分支 `height` 与实际下降不符，或子页首键 ≠ 分支记录的 `first_key` → `Corrupt`（下降时校验）

任何失败都不返回部分结果、不填零（spec §4）。

## 8. 写入（`fs-snapshot` 编码器）

编码器是纯函数式的"排序条目流 → 页流"：

```rust
pub trait PageSink {                      // 由调用方实现：直接 put，或批量发布（ADR-006 §8）
    fn put(&mut self, role: Role, value: &[u8]) -> Result<ObjectId, SinkError>;
}
pub fn encode_directory(entries: impl Iterator<Item = (Name, u64)>, sink) -> Result<Option<ObjectId>>;
pub fn encode_extents(extents: impl Iterator<Item = Extent>, sink)       -> Result<Option<ObjectId>>;
pub fn encode_inode_table(records: impl Iterator<Item = InodeRecord>, sink) -> Result<ObjectId>;
pub fn encode_root(profile, scope_id, inode_table: ObjectId, sink)       -> Result<ObjectId>;
```

- 输入必须严格递增，否则 `Unsorted` 错误（不排序、不去重——那是调用方的 bug）；
- 内存 O(N/64)（只缓存各层未满页），1 GiB 文件的 extent 流与 10 万 inode 流都不需要整份驻留；
- **生成顺序**：子对象先于引用它的页 put（页 → 上层页 → … → 根），所以任何已发布的对象只引用
  已发布的对象（Root 审查 Q3 的"不悬空"在对象图内部同样成立）；
- 编码器不知道 overlay、workspace、路径语义；"从哪里拿条目"是物化器（ADR-006 §7）的事。

## 9. 不变量与 `verify --deep`

S7 的完整表述，`verify --deep <root>` 从根遍历逐条检查：

| # | 不变量 | 违反时 |
| --- | --- | --- |
| V1 | 每个可达对象存在且身份正确 | `DanglingReference` / `Corrupt` |
| V2 | 每页通过 §7 校验 | 同 §7 |
| V3 | 从根目录（serial 0）沿目录项遍历，每个 serial **至多到达一次**（无环、无硬链接） | `TopologyViolation` |
| V4 | inode 表键集合 == 可达 serial 集合（**无孤儿 inode**——P1 A1 缺陷在快照格式里的对应物） | `OrphanInode` / `MissingInode` |
| V5 | 目录项指向的 serial 的 kind 与 inode 表一致；serial 0 是 Directory | `TopologyViolation` |
| V6 | 路径深度 ≤ `MAX_DEPTH`（64） | `TopologyViolation` |

物化器按构造满足 V3–V6（inode 表**由遍历生成**，而不是扫描任何表——设计不变量 I2）。

## 10. 未实现特性（明确清单）

| 特性 | 去向 |
| --- | --- |
| 增量物化（只重算脏路径） | 以后；格式已支持，无需改 |
| CDC 分块 | P2b（只改 extent 叶的切块） |
| 硬链接、符号链接 | 未计划；V3 现在明确拒绝多重绑定 |
| 扩展属性、ACL、Windows 属性 | P3（届时 inode 记录 `fmt` 升级） |
| inline 小文件进 inode 记录 | 不做（保持"相同内容 = 一个对象"的可验证去重，spec §6 P1-1） |
| 快照间 diff | 以后；历史无关 + 局部性使其可按 oid 剪枝 |

## 11. 正确性风险与已知限制

1. **哈希切页的最坏情况**：恶意构造的键可以让页始终撞字节护栏（退化为按大小切），历史无关性
   仍成立（规则确定），只是局部性变差。可信本地域（spec §11-6）下不处理。
2. **时间戳进入内容**：只改 mtime 也产生新根（[LFS] 同样的已知缺口）。这是"树内容"的诚实定义，
   不修。
3. **全量物化成本** O(树页数) 的读与哈希；写盘只发生在变化部分。10 万文件规模的耗时由 P2a-8
   报告实测，不预先宣称。

## 12. 测试清单（P2a-1 验收）

1. 四种树各自编码 → 解码往返；
2. **历史无关**（属性测试）：同一条目集合以随机插入顺序构建 1000 次，根 id 全部相同；
3. **局部性**：N = 10 万条目改 1 条值 / 插 1 条键，新增页数 ≤ 树深 + 2；
4. §7 每一类解码拒绝至少一例（错 role、错 fmt、截断、尾随、乱序、重复、重叠、零长、非法
   kind/content、超页长、分支首键不符）；
5. profile 不符 → `UnsupportedProfile`；
6. `encode_*` 输入未排序 → `Unsorted`；
7. V1–V6 每条构造一个违反并被 `verify --deep` 报出；
8. 规模：10 万 inode、单目录 10 万条目、1 GiB 文件 16 384 个 extent——页数、深度、编码耗时、
   峰值内存（三次重复，报 P50，注明载荷特征，见 ADR-004 §13 收尾清单第 4 条）。
