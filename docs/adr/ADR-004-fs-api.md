# ADR-004：fs-api 语义与文件内容路径

- 状态：已接受（Accepted），适用于 P1 / `fs-namespace` schema 版本 2
- 日期：2026-10-23
- 决策者：manager（任务卡 T4；spec §1 P1.4/P1.5/P1.8、§3 API 草案、§6 矩阵；ADR-003 §9；`docs/architecture.md` §1/§2；调研 §4.4；Root 审查 `docs/research/2026-10-22-root-model-review.md`）
- 实施者：T4（本 ADR 与 `crates/fs-api`、`crates/fs-cli` 同步提交）
- 相关：[ADR-001](ADR-001-object-encoding.md)、[ADR-002](ADR-002-store-layout.md)、[ADR-003](ADR-003-namespace.md)

本 ADR 记录**对外文件 API 的语义**与**文件内容如何落进 CAS**。标注"已实现"的有测试覆盖；
"决策"条目归后续阶段；"未实现"清单里的特性一律不存在。

---

## 1. 背景与问题

T3 定下了命名空间（目录、FileId、路径操作、root 发布链），但 `file` 表只有
`content_root: Option<ObjectId>` 与 `size` 两个字段，没有任何代码写入它们。spec §1 P1.4 要求
"整文件 Blob 的创建与读取；大文件的初版可用固定块/Extent Index，但必须支持有界 `read_at`，
**不可为随机读取加载整个文件**"。

需要回答：

1. 对外 API 的形状：有句柄还是无状态？部分读写、短读、EOF、truncate 各是什么语义？
2. 小文件与大文件分别怎么编码，块参数冻结在哪；
3. 写路径的定序如何保证"命名空间绝不悬空引用"（Root 审查 Q3）；
4. `flush` 在 P1 是什么；
5. `verify` 如何发现坏引用（Root 审查 Q3 的补充修正）。

## 2. 决策摘要

| # | 决策 | 状态 |
| --- | --- | --- |
| A1 | **无状态 + 路径式 API**，不引入长期存活的 Handle | 已实现 |
| A2 | `read_at` 允许短读；EOF 返回 0；不跨越已写结尾返回填充字节 | 已实现 |
| A3 | `write_at` 允许短写；越过 EOF 自动扩展并补零 | 已实现 |
| A4 | `truncate` 扩大补零、缩小丢弃；不改变 FileId | 已实现 |
| A5 | `create` 创建空文件；已存在报 `AlreadyExists` | 已实现 |
| A6 | `flush` 是**显式 no-op**，文档写明"P1 无缓冲" | 已实现 |
| A7 | 小文件（< 64 KiB）= 单个 `Bytes` 对象作 `content_root` | 已实现 |
| A8 | 大文件 = 固定 **64 KiB** 块 + `file_extent` 行；`read_at` 按区间查询 | 已实现 |
| A9 | 写路径：块对象**先** `put`，**再**在命名空间事务里引用 | 已实现 |
| A10 | 读取放大计数是**生产指标**（非测试钩子），T5 复用 | 已实现 |
| A11 | `verify` 增加 namespace→store 闭包校验 | 已实现 |
| A12 | `fs-namespace` schema 1 → 2（新增 `file_extent`） | 已实现 |

## 3. 对外 API 形状：无状态 + 路径式（A1）

```rust
pub struct Fs { /* Store + Namespace */ }

impl Fs {
    pub fn open(store_root: &Path, namespace: &str, mode: OpenMode) -> FsResult<Self>;
    pub fn create(&self, path: &str) -> FsResult<()>;
    pub fn read_at(&self, path: &str, offset: u64, out: &mut [u8]) -> FsResult<usize>;
    pub fn write_at(&self, path: &str, offset: u64, bytes: &[u8]) -> FsResult<usize>;
    pub fn truncate(&self, path: &str, size: u64) -> FsResult<()>;
    pub fn flush(&self, path: &str) -> FsResult<()>;
    pub fn stat(&self, path: &str) -> FsResult<Stat>;
    pub fn read_dir(&self, path: &str) -> FsResult<Vec<Entry>>;
    pub fn mkdir(&self, path: &str) -> FsResult<()>;
    pub fn remove(&self, path: &str) -> FsResult<()>;   // file or empty directory
    pub fn rename(&self, from: &str, to: &str) -> FsResult<()>;
    pub fn verify(&self) -> FsResult<VerifyReport>;
    pub fn stats(&self) -> FsResult<Stats>;
    pub fn read_stats(&self) -> ReadStats;              // §6, the amplification counter
    pub fn file_id(&self, path: &str) -> FsResult<FileId>;  // identity, for callers that need it
}
```

There is deliberately **no separate `rmdir`**: `remove` dispatches on the target's kind, so
one command (`rm`) covers both cases the CLI needs. The namespace layer still keeps
`rmdir`/`unlink` distinct, where the POSIX distinctions matter.

### 3.1 为什么不要句柄

**spec §3 自己声明该形状不是最终契约**：草案原文写明"该代码仅定义所需操作形状，不是最终可
直接编译的 Trait（Handle 所有权 … 通过 ADR 决定）"。因此无状态 API 不是对规格的偏离，而是
规格明确留给 ADR 的决定。

技术理由：

1. **ADR-003 §9 的句柄草图在 D+ 下无法兑现**：那里写"句柄含 FileId + 打开的 root 快照"，
   但 D+ 的历史树不可读（ADR-003 §4.1），没有可读取的快照。硬做只能退化成"句柄只记 FileId"。
2. **退化后的句柄不提供任何路径式 API 给不了的保证**：FileId 在改名后不变、在删除后才失效，
   而这两点路径式 API 同样能表达。
3. **无 TOCTOU**：路径式 + 每操作一事务意味着"解析路径"与"变更/读取"在**同一个事务内**完成，
   不存在"检查后使用"的窗口。带句柄的实现反而要额外论证这一点。

### 3.2 与规格的偏差，以及推迟了什么

| 项 | 说明 |
| --- | --- |
| 偏差 | spec §3 草案的 `open`/`Handle` 形状未采用；以 `Fs` + `&Path` 取代。依据见 §3.1 第 1 点与 spec §3 自身的声明。 |
| **推迟，不是关闭** | P3 的 WinFsp 前端**必须**有句柄语义。届时需要回答：① 句柄在 `rename` 后是否仍指向同一对象（身份钉扎）；② 句柄在目标被 `unlink` 后的行为（delete-pending）；③ 与 WinFsp 句柄生命周期/关闭回调的映射；④ 句柄与 share mode、文件锁的交互。这四条是 P3 设计时的**问题清单**，不在 P1 解决。 |
| 实现路径 | P3 可在 `fs-api` 之上加一层句柄表（`FileId` → 引用计数），不需要改本 ADR 的操作语义。 |

## 4. 读写语义（A2–A6）

### 4.1 `read_at(path, offset, out) -> usize`

- 返回**实际读到的字节数**（短读允许），`offset >= size` 时返回 `0`。
- 不跨越已写结尾返回填充字节：文件大小 `size` 是硬边界，读 `size` 之后不返回零。
- `out` 大于剩余长度时只填剩余部分，返回值告诉调用方实际长度。

### 4.2 `write_at(path, offset, bytes) -> usize`

- 返回**实际写入字节数**（短写允许）；P1 的实现总是全写（内存内操作），但签名允许短写，
  P2 接入真实流时长文件可能短写。
- 写起点 `offset > size` 时**自动扩展并补零**（稀疏空洞以零填充，不是错误）。
- 写导致 `size` 变化时更新 `size` 与 `modified_ns`。

### 4.3 `truncate(path, size)`

- `size < 当前`：丢弃尾部；`size > 当前`：补零扩展。FileId 不变。
- 是独立的变更操作，**发布新 root**。

### 4.4 `create(path)`

- 创建**空文件**（`size = 0`、`content_root = None`）。已存在 → `AlreadyExists`。
- 父目录不存在 → `NotFound`；父是文件 → `NotADirectory`。
- 补上了 T3 留下的缺口：T3 的 `file` 表有 `Kind::File`，但没有任何操作能创建它。

### 4.5 `flush(path)`

**P1 的 `flush` 是显式 no-op。** 每次写操作自身就是一个事务（ADR-003 §6），P1 不存在
"缓冲未刷"状态，因此没有可 flush 的东西。该方法存在的唯一理由是满足 API 形状要求，其文档
必须写明"当前无操作，不提供额外持久化保证"——不得暗示它有 `fsync` 语义。

## 5. 文件内容路径（A7–A9）

### 5.1 小文件：单个对象

`size < 64 KiB` 的文件，内容整体存为一个 `Role::Bytes` 对象，其 ObjectId 即
`file.content_root`。读取 = `Store::get` + `Object::decode` + 切片。

### 5.2 大文件：固定块 + extent 行

`size >= 64 KiB` 的文件切成 **64 KiB** 固定块（最后一块可变长），每块一个 `Bytes` 对象，
extent 记录在 `file_extent`：

```sql
CREATE TABLE file_extent (
    namespace TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    logical_start INTEGER NOT NULL,
    length INTEGER NOT NULL,
    object_id TEXT NOT NULL,
    source_offset INTEGER NOT NULL,
    PRIMARY KEY (namespace, file_id, logical_start)
) STRICT;
CREATE INDEX file_extent_by_file ON file_extent (namespace, file_id, logical_start);
```

`read_at` 的算法：按 `[offset, offset+len)` 与 extent 行求交，对每个相交区间只 `get`
对应块对象并切片拷贝。**绝不加载整个文件**——这是 spec P1.4 的硬性要求，并由 `read_at`
的放大计数（§6）钉住。

为什么 64 KiB：2 的幂、与 P2 的 CDC 目标（16 KiB）**刻意不同**，避免读者混淆两种分块；同时
1 GiB 文件 ≈ 16,384 行，SQLite 索引下随机读可接受（调研 §4.4）。

块尺寸冻结进 `namespace_policy`（键 `block_bytes`），打开时校验——与 CDC profile 冻结参数
是同一先例。

### 5.3 大小文件的边界

`size` 恰好 64 KiB 时用块路径还是整文件路径 **由 §5.4 的写入过程决定，不由读取猜测**：
读路径先看 `content_root` 是否为空、再看有没有 extent 行，两者互斥。这避免了"按 size 推断
编码方式"在 `truncate` 后失配。

### 5.4 写入路径定序（A9，Root 审查 Q3）

```text
write_at(path, offset, bytes):
  1. 读 file.size / content_root（shape_of，不读 extent 行）
  2. 块级：只查询 [touch_from, touch_to) 区间内的 extent 行（extents_in_range）
  3. 对每个受影响块：读出旧块 → 修改 → Store::put(Bytes, 新块)   ← 对象先发布
  4. BEGIN IMMEDIATE
       更新 file.size / modified_ns
       只 upsert 受影响块的 extent 行（replace_extents），其余行留在原地
       publish_tx("write")
     COMMIT
```

**写路径必须是 O(受影响块)，不是 O(文件)。** 这一条在 T4 实现期间被实测抓出两次：

| 版本 | 每块写入代价 | 256 MiB 总耗时 | 问题 |
| --- | --- | --- | --- |
| 初版：整文件读-改-写 | 5.9 → 52.3 ms/块 | **> 10 分钟（跑不完）** | 每次 `write_at` materialize 整文件，O(n²) |
| 第二版：只改受影响块，但遍历全部 extent | 3.7 → 10.8 ms/块 | 61 s | `content_state` 每次读全部 extent 行，仍 O(n²) |
| **第三版：区间查询 + 只 upsert 受影响行** | **1.46 → 1.55 ms/块（恒定）** | **6.4 s** | — |

1 GiB（16,384 块）在第三版下 30.9 s，随机 4 KiB 读仍只触及 1 个块。
前两版都会让 spec §6 的"创建 10 万小文件"与"1 GiB 文件"验收不可达——**这是 ADR-003 §4.1
拒绝方案 A 时同一个算术错误的又一次出现**：只要路径上有一步是 O(文件)，循环调用就是 O(n²)。

**不变量**（等同 Root 审查的两条方向性结论，ADR-003 §"四问四答"）：

- **命名空间绝不悬空引用**：引用只在其对象已 `put` 成功后才写入，且写入与 root 发布同事务。
  事务失败 → 引用从未提交 → 永远成立。
- **对象可以孤儿**：第 3 步成功而第 4 步失败 → store 里有未被引用的对象。无害（内容寻址天然
  幂等，后续同内容 put 直接复用），回收归 P2.11 GC。

反方向（悬空引用）**不存在**，这是定序的直接推论。

## 6. 读放大计数（A10）

`Fs` 暴露一个只读计数：

```rust
pub struct ReadStats { pub object_gets: u64, /* … */ }
impl Fs { pub fn read_stats(&self) -> ReadStats; }
```

每次 `read_at` 内部实际调用的 `Store::get` 次数累加进去（原子计数）。

**这是生产指标，不是测试钩子**：spec §5.6 要求 "read amplification" 指标，本计数是它的最小
实现，T5 的基准直接复用。因此**不加 feature 门**，也不需要 dev-dependency 自引用。

测试用它断言"读 4 KiB 至多触发 2 个块对象的 get"——因为一次 4 KiB 读最多跨 **2** 个
64 KiB 块（起点在块尾部时跨到下一块）。若将来块尺寸变化，该阈值的推导随之变化，测试注释
里写明这一点。

## 7. 闭包校验（A11）

`Fs::verify()` = `Store::verify()` 的对象校验 **+** 命名空间→对象闭包校验：

1. 遍历 `file` 表中所有非空 `content_root`，逐个 `Store::get`（同时验证内容身份）；
2. 遍历 `file_extent` 中所有 `object_id`，同上；
3. 引用**重复时不重复读取**（同一 ObjectId 只验一次）；
4. 失败项**逐条报告**，不静默跳过，不修复。

这满足 Root 审查 Q3 的补充修正：spec §4 "发布 Root 前验证可达依赖" 由**put-先于-引用**的
定序（无需逐操作验证依赖，否则是 O(树)/op）+ **闭包校验**共同满足。

## 8. Store 之外的写入者不存在

`fs-api` 依赖 `fs-namespace` 与 `cas-core`；它通过 `fs-namespace` 使用 `Store`，
**不绕过**它直接碰 `objects/` 或 SQLite（`docs/architecture.md` §2）。`fs-cli` 依赖 `fs-api`，
不含任何业务逻辑。

## 9. 版本与兼容

`fs-namespace` schema **1 → 2**（新增 `file_extent` 表与 `block_bytes` policy 键）。

- 按 crate 治理（ADR-003 §4.2）：`namespace_policy.schema_version = 2`；
- **不提供 1→2 迁移**，旧库被 DDL 自校验以 `SchemaMismatch` 拒绝（P1 零部署）；
- 这是"schema 演进第二个先例"（第一个是 T3 的 1→1 内部形状调整，未发布）。

## 10. 未实现特性（明确清单）

| 特性 | 计划阶段 | 说明 |
| --- | --- | --- |
| 句柄 / `open` 返回 Handle | P3 | 见 §3.2 的四条问题清单 |
| 真正的 `fsync` 语义 | P1 无 | `flush` 是 no-op（§4.5） |
| 硬链接 / 符号链接 | P1 外 | 无 |
| 递归删除 `rm -r` | 未定 | CLI 的 `rm` 只删文件与**空**目录 |
| 稀疏文件（不占块的洞） | 未定 | 扩展补零是实写零块 |
| CDC / 变长分块 | P2 | 现为固定 64 KiB |
| delta / 压缩 / pack | P2 | 无 |
| GC | P2.11 | 孤儿不回收 |
| 文件锁 / mmap | P3 或外 | 无 |
| 并发写同一文件的冲突检测 | P2 | 后写生效（ADR-003 §7） |
| WinFsp | P3 | 无 |

## 11. 正确性风险与已知限制

1. **64 KiB 块对大文件写入不经济**：改写一个字节也要 `put` 整个 64 KiB 块（内容寻址会去重
   未变块,但受影响块必然重写）。CDC 归 P2。
2. **写路径是块级读-改-写**：只读取与重写受影响的块（§5.4），代价 O(受影响块)。并发同文件写是
   "后写生效"，不会产生交错损坏，但可能丢失一方的更新（P1 语义，ADR-003 §7）。
3. **闭包校验是 O(引用数)**：大 Store 上线性，无增量模式。
4. **`flush` no-op** 可能让从 POSIX 语义来的调用方误以为有持久化保证；文档已明示，但这是
   真实的语义陷阱。
5. **`block_bytes` 冻结但无迁移**：改块尺寸等同于换格式，需要新版本。
6. **无稀疏表示**：`truncate` 扩大与跨越 EOF 的写都会实写零块，大空洞浪费空间。

## 12. 测试清单

| 类别 | 代表性测试 |
| --- | --- |
| 部分读 / 短读 / EOF | `read_at_returns_at_most_the_requested_length`、`read_at_is_short_near_eof_and_zero_past_it`、`read_at_does_not_return_bytes_past_eof` |
| 部分写 / 跨 EOF 扩展补零 | `write_at_past_eof_zero_fills_the_gap`、`write_at_overwrites_in_place` |
| truncate 扩大/缩小 | `truncate_shrinks_and_grows_without_changing_identity` |
| create 冲突与父缺失 | `create_makes_an_empty_file`、`create_refuses_an_existing_name`、`create_requires_an_existing_parent` |
| flush 是 no-op | `flush_is_a_no_op_on_a_file` |
| 大文件有界 read_at（放大计数） | `large_file_read_is_bounded`、`a_read_straddling_a_boundary_fetches_two_blocks`、`reading_one_byte_from_either_end_fetches_one_block` |
| 写路径有界（O(1)/块） | `per_block_write_cost_does_not_grow_with_file_size` |
| 边界与去重 | `a_file_at_the_block_boundary_round_trips`、`identical_blocks_share_one_object` |
| 闭包校验（坏引用检出） | `a_dangling_reference_is_reported`、`extents_are_verified_too`、`identical_contents_are_counted_once` |
| schema 治理（v1 拒绝、policy 键、DDL） | `a_version_1_store_is_refused`、`a_removed_policy_key_reports_the_qualified_key_name`、`every_recorded_ddl_table_exists` |
| CLI 全命令 | `tests/cli.rs`（16 个） |
| 1 GiB 确认 | `a_gi_b_file_answers_a_random_read_from_one_block`（`#[ignore]`，T5 跑） |

具体测试名在提交说明中与每条声明配对列出。

## 13. 下一步

- **T5**：规模与性能，复用 `read_stats`；用真实 1 GiB 复测块路径。
- **P2**：CDC、delta、快照（纯对象图，零表依赖）、GC；Branch CAS 令牌 = 快照根。
- **P3**：句柄层（§3.2 的四条问题清单）、WinFsp 映射。
