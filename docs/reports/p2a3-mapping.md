# P2a-3 只读映射分析：现有存储调用 → TreeStore 操作面

- 日期：2026-10-23
- 用途：card P2a-3 要求"TreeStore trait 的操作面覆盖现有 operations.rs / content.rs 的全部存储需求"，
  以及验收标准"grep `conn().execute\|conn().query` 在这两个文件中为空"。本文是重构前的枚举，
  重构后逐条核销。

## 1. 现状：14 处直接 SQL

`crates/fs-namespace/src/operations.rs`：

| # | 行 | 语句 | 语义 | 映射到 |
| --- | --- | --- | --- | --- |
| 1 | 38 | `INSERT INTO file(...)` | 新建目录 inode | `create_inode(Kind::Directory)` |
| 2 | 73 | `SELECT COUNT(*) FROM directory_entry WHERE parent_id = ?` | 目录是否为空 | `read_dir(dir, None, 1)` 判空 |
| 3 | 189 | 同上（rename 的 R6） | 目标目录是否为空 | `read_dir(target, None, 1)` |
| 4 | 316 | `INSERT INTO directory_entry(...)` | 绑定名字 | `bind(parent, name, child)` |
| 5 | 330 | `DELETE FROM directory_entry(...)` | 解绑名字 | `unbind(parent, name)` |

`crates/fs-namespace/src/content.rs`：

| # | 行 | 语句 | 语义 | 映射到 |
| --- | --- | --- | --- | --- |
| 6 | 70 | `UPDATE file SET ...`（create_file 置 content_root） | 建文件 inode | `create_inode(Kind::File)` |
| 7 | — | `SELECT kind, size, content_root FROM file WHERE file_id` | 读 inode | `inode(id)` |
| 8 | — | `SELECT ... FROM file_extent WHERE file_id AND logical_start BETWEEN` | 区间 extent | `extents_in_range(id, from, to)` |
| 9 | — | `DELETE FROM file_extent WHERE file_id` | 替换 extent 前清空 | `extent_replace` / `drop_inode` |
| 10 | — | `INSERT INTO file_extent(...)` | 写 extent | `extent_replace` |
| 11 | 405–478 | 三条内部一致性查询（unreachable inodes / entries without inode / extents without inode） | `verify` 用 | **见 §3：不进 trait** |
| 12 | 484 | 三表联合查引用对象 | `referenced_objects` | **见 §3：不进 trait** |
| 13 | — | `UPDATE file SET size, content_root, modified_ns` | 改 inode | `create_inode` 的继任：`update_inode` |
| 14 | — | `UPDATE file SET content_root = NULL` | 清内容 | 同上 |

`crates/fs-namespace/src/namespace.rs`：14 处 `conn()` 使用（`with_write_tx`、`publish_tx`、
`root_record(s)`、`head_root`、`root_count`、`connection_for_tests`）。这些是**后端的**职责，
随表后端迁到 `SqlTree`。

## 2. 裁决的 trait 操作面 vs 现有需求

architect 给的操作面：

```rust
trait TreeRead  { lookup / inode / read_dir }
trait TreeWrite { bind / unbind / create_inode / drop_inode }
```

**缺口一：没有"改 inode"**。`content.rs` 需要改 size、content_root、modified_ns（写文件、
truncate、形态切换都要）。`create_inode` 只能新建。

→ 需要 `update_inode(&mut self, id: FileId, record: InodeView) -> Result<()>`。

**缺口二：没有 extent 操作**。`content.rs` 的 extent 读写是内容路径的核心。

→ 需要 `extents_in_range`（读，在 `TreeRead`）与 `extent_replace`（写，在 `TreeWrite`）。

**缺口三：`inode` 返回的 `InodeView` 内容未定义**。现有 `Stat`/`ContentState` 需要
kind、size、created_ns、modified_ns、content_root（`Option<ObjectId>`）。

→ `InodeView` 必须含这五项。

**缺口四：serial 分配**。`mkdir`/`create_file` 需要新 serial（`alloc_serial`）。
architect 方案是 `create_inode(kind)` 内部分配并返回 `FileId`，覆盖此需求。

## 3. 明确不进 trait 的四项（必须记录理由）

| 项 | 为什么不在 trait 里 |
| --- | --- |
| `unreachable_inodes` / `entries_without_inode` / `extents_without_inode` | 这三条是**表结构**的诊断查询。overlay 后端没有"file 表"，孤儿页的形态不同（whiteout vs 悬空），表后端的 SQL 语义无法泛化。它们应作为各后端自己的 `verify` 特有能力，或由 `TreeRead::read_dir` 遍历重建。 |
| `referenced_objects` | 闭包校验需要枚举全部引用，是 `verify` 的需求而非路径规则的需求；overlay 后端的引用集合来自不同结构。 |
| `publish_tx` / `head_root` / `root_record(s)` | 裁决已明确：D+ root 是**表后端的实现细节**，从 `Namespace::backend()` 取。 |
| 文件内容 `put`（对象写） | 裁决已明确：内容操作不进 TreeStore（跨连接会 Busy）。 |

这四项在重构后**保留在 `SqlTree` 上**，`Namespace` 通过类型约束只在这些方法需要时才可见。

## 4. 最终形状（architect 裁决后，已实现）

architect 的裁决（ADR-006 §7.1 @ 6afbfb3）与本文 §2 的提议**四项全部调整**，最终实现如下：

```rust
pub trait TreeRead {
    fn lookup(&self, parent: FileId, name: &[u8]) -> NamespaceResult<Option<FileId>>;
    fn inode(&self, id: FileId) -> NamespaceResult<Option<InodeView>>;
    fn read_dir(&self, dir: FileId, after: Option<&[u8]>, limit: usize)
        -> NamespaceResult<Vec<(Vec<u8>, FileId)>>;
    fn is_dir_empty(&self, dir: FileId) -> NamespaceResult<bool>;
}

pub trait TreeWrite: TreeRead {
    fn bind(&mut self, parent: FileId, name: &[u8], child: FileId) -> NamespaceResult<()>;
    fn unbind(&mut self, parent: FileId, name: &[u8]) -> NamespaceResult<()>;
    fn create_inode(&mut self, kind: Kind) -> NamespaceResult<FileId>;
    fn drop_inode(&mut self, id: FileId) -> NamespaceResult<()>;
}

pub trait TreeStore {
    type Read<'t>: TreeRead;
    type Write<'t>: TreeWrite;
    fn read<T>(&self, body: impl FnOnce(&Self::Read<'_>) -> NamespaceResult<T>) -> NamespaceResult<T>;
    fn write<T>(&self, op: OpLabel, body: impl FnOnce(&mut Self::Write<'_>) -> NamespaceResult<T>)
        -> NamespaceResult<T>;
}
```

与本文 §2 提议的差异：

| # | 我的提议 | 裁决 | 理由 |
| --- | --- | --- | --- |
| 1 | `update_inode` 进 `TreeWrite` | **不进 trait**，作 `SqlTree` 固有方法 | overlay 的内容是"页 + base_limit"，没有 `content_root`；进 trait 会逼它模拟 P1 表 |
| 2 | extent 两方法进 trait | **不进 trait**，同上 | 同上。且规则层（R1–R12）从不读内容 |
| 3 | `InodeView` 含 `content_root` | **去掉**，只留 id/kind/size/两个时间 | 同上；`stat` 在边缘由 `SqlTree::content_root_of` 补 |
| 4 | `is_dir_empty` 可选（性能偏好） | **进 `TreeRead`** | 理由比我说的更强：overlay 上 `read_dir(dir,None,1)` 不是 O(1)——要归并 base + overlay 行 + 跳过 whiteout |

**我错在哪**：把"实现需要"当成了"契约需要"。内容方法唯一的调用方是 `fs-api` 直写路径，
它们随表后端在 P2a-7 一起消失，本就不该出现在跨后端的契约里。

## 4.1 R9 不需要 `parent_of`

本文 §2 曾提议加 `TreeRead::parent_of(id)`，理由是原实现用
`SELECT parent_id WHERE child_id` 做 O(树深) 的反向查询，而 trait 的向下接口做不到。

**architect 否掉了**，改用 ADR-006 §7.3 的做法：`rename` 在同一 write body 内**从根逐段 lookup
目标路径**，任一前缀 serial 等于被移动目录即拒绝。O(路径深度)，只用 `lookup`，overlay 也能做。

实现为 `path_passes_through`。这比 `parent_of` 好在两处：overlay 的 base 快照 inode 没有 parent
字段，反向索引建不出来；而逐段 lookup 同时把 R9 判定与 bind/unbind 放进同一事务（TX2），
交叉 rename 成环（K4）因此结构性消失。

## 5. 验收核对方式（含一处与实际冲突的说明）

原卡写：

```sh
grep -nE "conn\(\)\.(execute|query)" crates/fs-namespace/src/operations.rs content.rs   # 必须为空
cargo test --workspace                                                                  # 279 既有测试不改一字
```

**第一条与 architect 的裁决冲突**，执行结果如下：

| 文件 | 结果 |
| --- | --- |
| `operations.rs` | **已删除**（规则全部搬到 `rules.rs`，走 trait）→ 满足 |
| `content.rs` | **仍有 SQL**——按裁决它是表后端的固有实现，SQL 就是它的实现方式 |

所以准确的说法是：**规则层零 SQL，内容层按裁决保留为 `SqlTree` 固有方法**。
卡的原文写于裁决之前，manager 已确认按裁决执行。

第二条成立：**279 个既有测试一字未改**（现在连同新增共 313 通过）。

## 6. 风险（事后核对）

1. **GAT 的生命周期** —— 确实卡住了，但**不是**我预判的原因（`where Self: 'a`）。
   真实原因是视图同时持有 `Transaction<'a>` 与 `name: &'a str` 两个无关借用，
   以及规则层缺 `use TreeRead`。详见 `p2a3-gat-probes.md`。
2. **`Namespace::open` 的签名** —— 预料正确。`Namespace` 保持具体类型、内部持
   `Rules<SqlTree>`，`fs-api` **零改动**。
3. **测试构造代码** —— 预料正确。`connection_for_tests` 移到 `SqlTree`，签名从
   `&Connection` 变为 `Ref<'_, Connection>`；`inject::plant` 改为接受
   `impl Deref<Target = Connection>`，所以 5 处既有调用点**一字未改**。

## 7. 重构中引入的三个缺陷（都被既有测试抓到）

| # | 缺陷 | 症状 |
| --- | --- | --- |
| 1 | `initialize` 把 `root_id = 0` 写死 | 第二个 namespace 撞 `UNIQUE constraint failed: root.root_id` |
| 2 | `append_root` 写 `head_root = head + 1` 而非 `root_id` | 多 namespace 时 root 链指向别的 namespace 的行，报 "missing parent" |
| 3 | `validate_root_chain` 用两条独立查询比对行数 | 并发提交落在两条查询之间时差值变成 **-1**，报 "−1 of 9 rows" |

第 3 条尤其说明问题：两条 `query_row` 不是一个原子读（TX3 要求的正是这个）。
已改为**单条语句**返回两个计数。

另有一处**被放大**的既有缺陷：`write_at` 与 `unlink` 竞争留孤儿 extent 行。基线 0/25，
加上 `BEGIN DEFERRED` 读事务后变成 11/25——读事务把"确认存在"到"写 extent"的窗口拉长了。
已按 manager 批准加 rows-affected 检查（`UPDATE file` 匹配 0 行 → `NotFound`），
修复后 0/20。
