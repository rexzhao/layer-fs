# ADR-006：Workspace、Overlay、flush 与 Branch/Commit

- 状态：**草案（Proposed），分叉已决**——architect 起草，作为 P2a-2 卡的设计输入；owner 已于
  2026-10-23 确认分叉 F1–F7 全部按建议执行（经 manager 转达，§2.1）。worker 落地核对后转 Accepted
- 日期：2026-10-23
- 起草：architect
- 依据：spec §2 P2.1–P2.4、P2.8–P2.11、§4、§5、§6 P2；owner P2 决定六条（workspace-only、overlay
  不做每操作落盘、CLI 显式风格、P2a/P2b 两波、快照 = 纯 CAS 对象图、无乐观并发 + 全量物化）；
  Root 审查 Q2/Q3/Q4；ADR-001 D9/D10；ADR-002 §5/§8；ADR-003 §3/§4.3；ADR-004 §3；ADR-005
- 修订：本 ADR **修订 ADR-001 D10**（§11，F2）与 **ADR-002 §5**（§12，批量发布，F7）；**取代**
  ADR-003 §4（表存储与 D+ root 链）与 ADR-004 §3/§5（直写路径）。ADR-003 §3 的违反形式枚举与
  ADR-004 §4 的读写语义**保留**，换到 workspace 视图上执行

---

## 1. 背景与问题

P1 只有一棵可变的"当前树"，每个操作直接改表、发一条 root 记录。owner 已定 P2 终态
**只有 Workspace 一条写路径**：变更进 overlay，snapshot/commit 才产生不可变根。需要回答：

1. overlay 存在哪里、如何隔离、如何独占；
2. 写入什么时候持久——flush 的精确合约；
3. 视图 `base ⊕ overlay` 的读取与路径语义如何组合，且**只有一份**路径规则实现；
4. snapshot/commit 如何把视图物化成 ADR-005 对象图，跨两个 SQLite 文件如何保证发布原子性；
5. branch/commit 的表、CAS 语义、reset/revert/restore 的区别（spec P2.9）；
6. 并发模型与 Busy；
7. 物化的写盘成本；
8. fs-api 与 CLI 的形状；
9. P1 表与直写路径如何退场。

## 2. 决策摘要

| # | 决策 |
| --- | --- |
| W1 | 每个 workspace 一个独立 overlay 文件 `workspaces/<id>/overlay.sqlite`；主库只放低频全局表 |
| W2 | **单写者**：overlay 以 `locking_mode=EXCLUSIVE` 打开，第二个打开者得到 `WorkspaceInUse` **[F5 已确认]** |
| W3 | 每个变更操作 = overlay 上的**一个不 fsync 的事务**（WAL + `synchronous=NORMAL`）；`flush` = 一次 `synchronous=FULL` 的提交，使此前全部事务持久 **[F4 已确认]** |
| W4 | 崩溃合约：恢复后的状态是操作序列的一个**前缀**，且包含最后一次成功 `flush` 之前的全部操作 |
| W5 | overlay 按 **serial** 键控（不按路径）；目录改名只写两条名字行；落回 base 只按 serial 进行 |
| W6 | 路径规则只有一份：`fs-namespace` 的规则改写为针对 `TreeStore` trait 的算法；overlay 实现该 trait |
| W7 | snapshot = 全量物化（ADR-005）**[F6 已确认]** + `pending_root` 意图标记 + 主库单事务 + overlay 重置 |
| W8 | commit 的 branch head 更新带期望值（期望 = workspace 的 `base_commit`）；不符 → `BranchMoved` **[F1 已确认]** |
| W9 | `revert` 只撤销 head **[F3 已确认]**；`reset` 移动指针；`restore` 改 workspace 内容 |
| W10 | 主库短事务有界等待锁（5 s），修订 D10 **[F2 已确认]** |
| W11 | cas-store 增加批量发布：对象文件逐个 durable，索引分块提交 **[F7 已确认]** |
| W12 | serial 由主库 `scope_allocator` 按 1024 一段预留，全 scope 唯一、永不复用 |
| W13 | 直写路径与 P1 命名空间表在 P2a 末删除；新 schema 拒绝 P1 Store（不迁移） |

### 2.1 owner 决策记录（2026-10-23 14:02，经 manager 转达，mailbox `manager/l2vye0vx8b`）

| 分叉 | owner 结论 | 落在本 ADR |
| --- | --- | --- |
| F1 同一 branch 先后 commit | **(b)**：后者得到类型化 `BranchMoved{expected, actual}`，workspace 原样保留；`--force` 显式覆盖并在 `branch_log` 记录被取代的 head | W8、§10.2、§10.3 |
| F2 Busy 处理 | **主库短事务有界等待 5 s**，作为对 ADR-001 D10 的显式修订；超时仍返回 `Busy` | W10、§11 |
| F3 revert 范围 | **只撤销 head**；任意提交的 revert 待有 merge 时再议 | W9、§10.2 |
| F4 flush 粒度 | **workspace 级**；`flush(path)` 等价于 `flush()` | W3、§8 |
| F5 同一 workspace 多进程写 | **单写者**；snapshot 期间本 workspace 写入阻塞 | W2、§9.1 |
| F6 物化方式 | **全量物化**（未变部分按 ADR-005 S3 不写盘）；增量以后做，格式不变 | W7、§9.2 |
| F7 物化写盘成本 | **先做 cas-store 批量发布并实测**；不达标再在 P2b 引入 pack | W11、§12 |

与 owner 既有决定的关系：F1 不与 Q4"不加乐观锁"冲突——Q4 的原语境是文件级写入；branch head 的
期望值检查是 spec §4 明文要求（"不静默覆盖"），令牌 = 快照根（Root 审查 Q2）。

> **相对于设计信件（3yinqic7xe §2.5）的修订（manager 已批准，`manager/zma4rr53ik`）**：信件曾设计"内存层 + 隐式 flush 的内存预算"。
> 本 ADR 改为 W3——每个操作写进 overlay 库、但不 fsync。理由：
> ① 少一层（读路径只有 overlay 库 + base 两层，没有"内存 / 已 flush / base"三层合成）；
> ② 内存上界自动成立（SQLite 页缓存），不需要预算与隐式 flush；
> ③ 与 owner 决定一致——不做每操作**落盘**（无 fsync），崩溃丢失未 flush 的部分仍被允许（进程
> 崩溃时实际上通常不丢，见 W4 的"至少"）；
> ④ [LFS] 同样是每操作一个 overlay 事务（它连 flush 都不提供）。
> 代价：每操作多一次 SQLite 事务提交（无 fsync，几十微秒级），P2a-8 实测。

## 3. 模块与依赖

```text
fs-cli ─► fs-api ─► fs-workspace ─┬─► fs-overlay ─┬─► fs-namespace（Path、规则、TreeStore trait、类型）
                                  │               └─► fs-snapshot
                                  └─► fs-snapshot ─► cas-store ─► cas-core
```

| crate | 拥有 | 禁止知道 |
| --- | --- | --- |
| fs-namespace | `Path`、名字规则、ADR-003 §3 全部违反形式、`TreeStore` trait、`FileId`/`Kind` | 任何存储：表、overlay、对象 |
| fs-snapshot | ADR-005 编解码、读取器、`PageSink`、`verify --deep` | overlay、workspace、路径规则 |
| fs-overlay | overlay 文件 schema 与 pragma、视图合成（overlay ⊕ base 读取器）、`TreeStore` 实现、flush | 主库表、branch/commit、发布协议 |
| fs-workspace | 主库 workspace/branch/commit/snapshot/scope 表与其 policy、workspace 生命周期、物化器、发布与恢复协议 | 页编码细节（经 fs-snapshot）、对象文件布局（经 cas-store） |
| fs-api | 对外 `Repo` / `Workspace` / `ReadView` | 上述一切内部类型 |

主库表的 schema 所有权从 fs-namespace **移交** fs-workspace（`workspace_policy` + DDL 逐字自校验，
沿用 ADR-003 §4.2 的按 crate 治理）。cas-store 的表不变。

## 4. 主库表（store.sqlite，fs-workspace 拥有）

```sql
CREATE TABLE workspace_policy (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL) STRICT;
  -- schema_version=3, ddl=<本 DDL 原文>, snapshot_profile=<ADR-005 profile_id>, overlay_page_bytes=4096,
  -- main_busy_timeout_ms=5000（§11.1）
CREATE TABLE scope_allocator (
    scope_id TEXT NOT NULL PRIMARY KEY,      -- 32 B hex
    next_serial INTEGER NOT NULL             -- 下一个未预留的 serial（≥1；0 = 根目录）
) STRICT;
CREATE TABLE "commit" (
    commit_id TEXT NOT NULL PRIMARY KEY,     -- §10.1
    root_oid TEXT NOT NULL,
    parent_commit TEXT,                      -- NULL 仅限 init 的首个提交
    created_ns INTEGER NOT NULL,
    message TEXT NOT NULL
) STRICT;
CREATE TABLE branch (
    name TEXT NOT NULL PRIMARY KEY,          -- 字节规则同文件名，另禁 '/'
    scope_id TEXT NOT NULL,
    head_commit TEXT NOT NULL
) STRICT;
CREATE TABLE branch_log (                    -- append-only：每次 head 移动一行（reflog，GC 根）
    seq INTEGER PRIMARY KEY,
    branch TEXT NOT NULL,
    old_commit TEXT,
    new_commit TEXT NOT NULL,
    op TEXT NOT NULL,                        -- init / commit / commit-force / reset / revert
    created_ns INTEGER NOT NULL
) STRICT;
CREATE TABLE workspace (
    name TEXT NOT NULL PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,                 -- 目录名；随机 128 bit hex，名字可复用而目录不复用
    branch TEXT NOT NULL,
    base_commit TEXT NOT NULL,               -- commit 的 parent；W8 的期望值
    base_root TEXT NOT NULL,                 -- 视图的 base；snapshot/restore 后可 ≠ root(base_commit)
    state TEXT NOT NULL,                     -- creating / ready
    created_ns INTEGER NOT NULL
) STRICT;
CREATE TABLE snapshot (                      -- 不移动 branch 的 snapshot 产物（GC 根）
    root_oid TEXT NOT NULL,
    workspace TEXT NOT NULL,
    created_ns INTEGER NOT NULL,
    PRIMARY KEY (root_oid, workspace, created_ns)
) STRICT;
```

`commit` 不是 CAS 对象：role 是闭集（ADR-001 §4.2），新增 role 要升 `version`，不值得。

## 5. Overlay 文件（fs-overlay 拥有）

### 5.1 位置与 pragma

`<store>/workspaces/<id>/overlay.sqlite`。路径由 fs-workspace 从 `Layout` 派生后交给 fs-overlay
（fs-overlay 不拼 Store 路径，同 ADR-003 §4.2.1 先例）。

`journal_mode=WAL`、`synchronous=NORMAL`（flush 时临时 `FULL`）、`locking_mode=EXCLUSIVE`、
`busy_timeout=0`（独占，不存在竞争，W2）、`foreign_keys=OFF`。打开后**读回校验**（[LFS] 先例），
不符即拒。

### 5.2 Schema

```sql
CREATE TABLE ov_meta (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL) STRICT;
  -- schema_version, ddl, workspace_id, base_root, scope_id, serial_next, serial_end, pending_root
CREATE TABLE ov_inode (
    serial INTEGER NOT NULL PRIMARY KEY,
    origin INTEGER NOT NULL,        -- 0 = 来自 base（覆盖其记录），1 = 本 workspace 新建
    live INTEGER NOT NULL,          -- 0 = 已删除的 base inode（墓碑）；新建 inode 删除时直接删行
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    created_ns INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL,
    base_limit INTEGER NOT NULL,    -- base 内容可见的上界：[0, base_limit)；新建文件 = 0
    entries INTEGER NOT NULL        -- 目录：视图中的子项数（rmdir 判空 O(1)）；文件 = 0
) STRICT;
CREATE TABLE ov_entry (
    parent INTEGER NOT NULL,
    name BLOB NOT NULL,
    child INTEGER,                  -- NULL = whiteout
    PRIMARY KEY (parent, name)
) STRICT;
CREATE TABLE ov_page (
    serial INTEGER NOT NULL,
    page_index INTEGER NOT NULL,
    data BLOB NOT NULL,             -- ≤ page_bytes；短于页长的部分读作零
    PRIMARY KEY (serial, page_index)
) STRICT;
```

## 6. FileId 与 scope（ADR-005 S8）

- `init` 生成 scope（随机 32 B），写进 `scope_allocator`；`branch create --from` 继承来源的 scope，
  所以同一谱系的所有 workspace 共享 scope；
- workspace 的 `ov_meta` 持有预留段 `[serial_next, serial_end)`；用尽时用一个主库短事务预留下一段
  （1024 个）。崩溃浪费未用的段——serial 不复用（ADR-003 §4.3），浪费无害；
- 被拒绝的操作也消耗 serial（分配在规则判定之前或之后都可，不回收）；
- base 中的文件在 workspace 中保持原 serial。

## 7. 视图与操作语义

### 7.1 TreeStore trait（fs-namespace）

```rust
pub trait TreeStore {
    fn lookup(&self, parent: FileId, name: &Name) -> Result<Option<FileId>>;
    fn inode(&self, id: FileId) -> Result<Option<InodeView>>;          // kind, size, times, entries
    fn read_dir(&self, dir: FileId, after: Option<&Name>, limit: usize) -> Result<Vec<(Name, FileId)>>;
    fn bind(&mut self, parent: FileId, name: &Name, child: FileId) -> Result<()>;
    fn unbind(&mut self, parent: FileId, name: &Name) -> Result<()>;
    fn create_inode(&mut self, kind: Kind) -> Result<FileId>;
    fn drop_inode(&mut self, id: FileId) -> Result<()>;                // A1：连同内容一并丢弃
    // 文件内容操作（read_at / write_at / truncate）由实现者提供，规则层只做类型与存在性检查
}
```

规则层（R1–R12、T1–T5、§3.1 路径无效形式）**原样迁移**，P2a-3 先让 P1 表后端实现该 trait、
251 个测试全绿，证明迁移不改语义；overlay 再实现同一 trait。

**trait 契约测试**（两个后端都必须通过，P2-0a/0b 的复现测试转入此处）：

| # | 契约 |
| --- | --- |
| K1 | unlink 后，被删 inode 的元数据与内容全部不可达（A1） |
| K2 | rename 覆盖文件后，被覆盖 inode 同 K1（A1） |
| K3 | **可线性化**：同一 workspace 上并发调用的变更操作，结果等于它们按某个串行顺序逐个执行的结果；每个操作对外是原子的（不存在"半个 write_at"可见）。规则判定、读取与写入在同一锁/事务内完成，无检查-使用窗口（B1）。见 §7.4 |
| K4 | 交叉目录 rename 不能形成环（B1，R9） |
| K5 | 同路径 create 恰有一个成功者（P2 首批的 create_file 对称用例） |
| K6 | truncate 到块/页边界不产生多余对象或页（KI-1 的对应物） |

### 7.2 读取

- **元数据**：`ov_inode` 有行 → 用它（`live=0` → 不存在，**不**落回 base）；否则查 base 读取器；
- **lookup(parent, name)**：`ov_entry` 有行 → child 或 whiteout；否则若 parent 是 base 来源的目录
  → 查 base 目录树；新建目录没有 base 内容，不落回（按 serial 键控的直接结果，不需要 opaque 标记）；
- **read_dir**：`ov_entry`（按 name）与 base 目录树（按 name）有序归并，overlay 胜，whiteout 遮蔽；
  以 `after` 名字为游标分页（窗口 64）；
- **read_at(serial, offset, len)**：对每个 4 KiB 页：有 `ov_page` → 用页（页内短于页长部分为零）；
  否则若字节位置 < `base_limit` → 从 base extent 读（只取覆盖区间的块，有界）；否则零。最后按
  `size` 截断。读取量 O(len)，不加载整文件（spec P2.4）。

### 7.3 写入

- **write_at**：对触及的每个页：页不存在时**拷入**该页的当前视图字节（至多首尾两页需要读 base，
  有界）→ 覆盖写入范围 → 存页；更新 `size = max(size, end)`、`modified_ns`。base 来源的 inode 首次
  修改时插入 `ov_inode(origin=0, base_limit=原 size)`；
- **truncate(new)**：删除 `page_index ≥ ⌈new/page⌉` 的页、把边界页的尾部截掉；
  `base_limit = min(base_limit, new)`；`size = new`。之后再扩展读到的都是零（被截掉的 base 字节因
  `base_limit` 不再可见）——P1 的 KI-1 类边界问题在此结构中不存在，由 K6 钉住；
- **unlink / rename 覆盖（K1/K2）**：名字：若 base 父目录确实绑定该名字 → 写 whiteout，否则删
  `ov_entry` 行。inode：base 来源 → `live=0` 墓碑 + 删其全部页；新建 → 删 `ov_inode` 行 + 页；
- **rename**：源名字按上一条处理（不动 inode），目标名字 bind 到同一 serial。目录改名只写两条名字行，
  子树不动；
- **R9（不得移入自身子树）**：在同一事务内，从根逐段解析目标父路径，任一前缀的 serial 等于被移动目录
  即拒绝。O(路径深度 ≤ 64)，不需要 parent 指针。物化器再校验一次拓扑（ADR-005 V3）。

每个变更操作是 overlay 上的一个事务；规则判定、读取与写入都在该事务内（K3）。workspace 句柄内部一把
互斥锁串行化同进程多线程调用。

### 7.4 同一 workspace 内的并发语义（K3 的精确表述）

- **只有一个句柄**：一个进程对一个 workspace 只能打开一个 `Workspace`（overlay 独占锁对同进程的第二条
  连接同样生效 → `WorkspaceInUse`）。多线程共享它：`Workspace: Send + Sync`，调用方用 `Arc` 共享；
- **串行化点**：句柄互斥锁在操作开始时获取、在 overlay 事务提交后释放。overlay 没有内存层（§2 的
  修订），所以不存在"写入一部分进内存、一部分进库"的中间状态——一个操作要么整体提交，要么整体没发生；
- **由此得出的可观察语义**：
  1. 不同块 / 不同区间的并发 `write_at`：两者都保留，`size` = 两者结尾与旧 size 的最大值；
  2. 重叠区间的并发 `write_at`：结果等于"先 A 后 B"或"先 B 后 A"之一，**重叠区间内不出现两者字节
     交错**；
  3. `write_at` 与 `truncate` 并发：结果等于两种串行顺序之一（先截后写：文件长回写入结尾，被截区间
     读零；先写后截：写入被截断）；
  4. `write_at` 与 `unlink` / `rename` 并发：按串行顺序之一；`unlink` 在先时 `write_at` 得到
     `NotFound`，不会给已删 inode 留下页（K1）；
  5. 交叉目录 `rename` 并发：至多一个成功，另一个按 R9 被拒（K4）；
- **不承诺的**：哪一个顺序胜出（无公平性、无"按调用时间"的顺序）；跨 workspace 的任何顺序（不同
  workspace 之间完全隔离，彼此不可见，直到 commit 后另一方 restore 或新建）。

**契约测试的写法**：断言"结果 ∈ {所有串行顺序的结果}"，而不是断言某一个顺序。第 1 条的两种顺序
结果相同，所以直接断言两者都存活即可——`k3_concurrent_writes_to_different_blocks_keep_both` 的断言
**不过强**，是 overlay 后端的准入门槛；表后端上它失败正是 B1 的复现（保持 `#[ignore]` 并注明原因）。
第 2–5 条各需一个同样形式的测试。

## 8. flush 合约 **[F4 已确认]**

| 调用 | 保证 |
| --- | --- |
| 任一变更操作返回 | 对本 workspace 的后续读取可见；**不保证持久** |
| `flush()` 返回 | 此前全部已返回的操作**持久**（一次 FULL 提交，fsync WAL） |
| `flush(path)` | 与 `flush()` 等价（workspace 级）。理由：create/rename 与数据之间有顺序依赖，只持久化一个文件会产生"数据在、创建不在"的状态，违反 W4 |
| 崩溃后打开 | 状态 = 操作序列的一个前缀 ⊇ 最后一次成功 flush 前的全部操作（W4） |

- 进程崩溃：已提交的 overlay 事务都在 WAL 中，通常一个都不丢；**不对外承诺**这一点，只承诺 W4；
- 掉电：依赖 SQLite WAL + NORMAL 的文档化语义（不损坏、可能回退最近的未 sync 事务，按 WAL 顺序
  得到前缀）。**测试只能覆盖进程崩溃**；掉电保证明确标为"依据 SQLite 文档，未经本项目测试"
  （spec §0-5）；
- CLI 每条命令退出前 flush（一条命令一个进程）；
- spec P2.10 的"flush 与后台 Seal 分离"：flush 不产生任何 CAS 对象；Seal 是 P2b。

## 9. snapshot / commit 协议（W7）

### 9.1 步骤

```text
snapshot(ws) / commit(ws, msg):
  0. 取 workspace 锁（本 workspace 的写操作在此期间阻塞 [F5 已确认]）；flush
  1. 物化（§9.2），得到根 R；commit 时若 R == root(base_commit) → 返回 NothingToCommit，
     不执行 2–4 步（overlay 与主库都不变；R 的新对象若有则成为孤儿）
  2. overlay 事务：ov_meta.pending_root = R                                  （FULL）
  3. 主库单事务（BEGIN IMMEDIATE）：
       snapshot：INSERT snapshot 行；UPDATE workspace SET base_root = R
       commit  ：读 branch.head_commit，≠ workspace.base_commit → BranchMoved（W8）
                 INSERT commit；UPDATE branch head；INSERT branch_log；
                 UPDATE workspace SET base_commit = C, base_root = R
  4. overlay 事务：清空 ov_inode / ov_entry / ov_page；base_root = R；pending_root = NULL（FULL）
```

不变量 I5：对象（第 1 步）先于引用它们的主库行（第 3 步）；第 3 步是唯一的发布点；第 4 步之前
overlay 内容不丢。

### 9.2 物化器（fs-workspace）

- **从根可达遍历**（设计不变量 I2）：从 serial 0 深度优先，按 §7.2 的视图规则列目录；**不扫描**
  `ov_inode` 或任何表来决定"有哪些文件"。inode 表由遍历结果按 serial 排序后编码（ADR-005 V4 按
  构造成立）；
- **目录树**：目录既无 `ov_inode` 行、也无 `ov_entry` 行时，直接复用 base 的目录树 oid（目录页
  只含 `name → serial`，子 inode 的变化不影响它，ADR-005 S2）；否则按视图归并的有序条目重新编码
  （未变页按 S3 得到同一 oid）；
- **文件内容**：无 `ov_inode` 行 → 复用 base 记录原样；否则按 64 KiB 块：与任何 `ov_page`、
  `base_limit` 边界、`size` 边界都不相交的块复用 base extent；其余块合成字节后 put；最后按 ADR-005
  §3.2 规范化规则选 Empty / Blob / Extents；
- 页与块经 `PageSink` 交给批量发布（§12）。

### 9.3 恢复（打开 workspace 时）

| overlay `pending_root` | 主库 `workspace.base_root` | 含义 | 动作 |
| --- | --- | --- | --- |
| NULL | — | 无进行中的发布 | 无 |
| R | == R | 第 3 步已提交 | 补做第 4 步 |
| R | ≠ R | 第 3 步未提交 | 清 `pending_root`，overlay 原样保留；R 的对象成为孤儿（容忍，GC 归 P2b） |

`workspace.state = creating`（创建中崩溃）→ 重建空 overlay 文件并置 `ready`。Repo 打开时，
`workspaces/` 下没有对应主库行的目录（discard 中崩溃）→ 删除。

### 9.4 崩溃注入点（spec §4 末条）

overlay 操作事务前后；flush 前后；物化中途（任意 put 之后）；第 2 步前后；第 3 步前后；第 4 步前后；
discard 删行后删文件前。每个点的期望结果由 §9.3 表给出，并对照参考模型逐字节核对视图。

## 10. Branch 与历史操作

### 10.1 commit id

`commit_id = hex(BLAKE3("wcas/commit/v1\0" || root_oid || parent_or_32_zero || created_ns(BE i64) ||
message_len(BE u32) || message))`。git 式，CLI 友好；含时间，所以同树同父的两次提交 id 不同。

### 10.2 操作

| 操作 | 语义 | 期望值检查 |
| --- | --- | --- |
| `init` | 建 scope、空根（只有根目录）、首个提交、branch `main` | — |
| `branch create <b> [--from <b\|commit>]` | 新 branch 指向来源提交，继承 scope | 名字已存在 → `AlreadyExists` |
| `commit` | §9.1 | head == `workspace.base_commit`，否则 `BranchMoved{expected, actual}` **[F1 已确认]** |
| `commit --force` | 不做期望值检查，覆盖 head；`branch_log` 记录被取代的 head **[F1 已确认]** | — |
| `reset <b> <commit> [--expect <c>]` | 只移动 branch 指针，不产生提交；旧 head 留在 `branch_log` | 给了 `--expect` 就检查 |
| `revert <b>` | 新提交：root = head 父提交的 root，parent = head。**只撤销 head** **[F3 已确认]** | 期望 = 读到的 head |
| `restore <ws> <commit> [--discard]` | 把 workspace 的 `base_root` 换成该提交的根；`base_commit` **不变**，所以随后 commit 产生一个"把内容恢复成该提交"的新提交。overlay 非空且无 `--discard` → `WorkspaceDirty` | — |
| `log <b>` | 沿 parent 链列提交 | — |

spec P2.9 要求的 revert 与 reset 区别：revert **生成**逆向提交（历史增长）；reset **移动**引用
（历史不增长，旧提交只经 `branch_log` 可达）。

### 10.3 `BranchMoved` 语义（F1，owner 已确认 (b)）

**判定**：第 3 步主库事务内读到 `branch.head_commit ≠ workspace.base_commit` → 返回
`BranchMoved { branch, expected: base_commit, actual: head_commit }`，事务回滚。

**失败后的状态（精确失败保管，ADR-001 D10 第 2 条）**：

| 对象 | 状态 |
| --- | --- |
| branch / commit / branch_log | 不变（事务回滚） |
| overlay | 不变；第 2 步写入的 `pending_root` 在同一调用内清回 NULL（overlay 事务）；若此时崩溃，§9.3 第三行处理，结果相同 |
| workspace 行 | 不变（`base_commit` 仍是旧 head，下一次 commit 仍会得到同一错误，直到用户处理） |
| 已物化的对象 | 留作孤儿（无害；同一视图再次物化时 `AlreadyPresent` 复用） |

**用户出路**（P2 不做 merge）：

1. `branch create <b2> --from <base_commit>` 后把 workspace 改挂 `b2` 再提交（P2a-6 提供
   `workspace rebind <w> <b2>`，只允许改挂到 head == `base_commit` 的 branch）；
2. `restore` 到新 head 后手工重做改动；
3. `commit --force`：跳过期望值检查。新提交的 parent = `workspace.base_commit`（不是被取代的 head），
   head 被覆盖；`branch_log` 追加一行 `op = commit-force`，`old_commit` = 被取代的 head——被取代
   的提交仍经 `branch_log` 可达（GC 根，§15），不丢数据，只是不在 branch 的 parent 链上。

**决策依据（owner 于 2026-10-23 14:02 确认 (b) 前的对照，留档）**：

| | (a) 后者覆盖 | **(b) 后者 `BranchMoved`（已采纳）** |
| --- | --- | --- |
| 结果 | 前者的提交 C1 从 branch 历史消失，只在 `branch_log` 留痕 | 后者失败，overlay 原样保留 |
| 用户出路 | 无感知（静默丢失） | 另建 branch、restore 重做，或 `--force` 显式覆盖 |
| 规格 | 违反 spec §4"不静默覆盖" | 符合 spec §4 与 Root 审查 Q2（令牌 = 快照根） |
| 与 owner Q4 决定 | Q4 原语境是文件级写入；branch head 不在其范围 | 同左 |
| 先例 | — | [LFS] `HeadMoved` + force/displaced |

## 11. 并发与 Busy **[F2 已确认]**

- workspace 内：单写者（W2）；同进程多线程经句柄互斥锁串行；
- workspace 之间：完全并发；共享的只有主库（短事务：serial 预留、发布、分配）与 cas-store 索引；
- **新的竞争来源**：两个 workspace 同时 snapshot，都在批量发布，会争 cas-store 索引的写锁。
  在 `busy_timeout=0` 下，大快照几乎必然中途 Busy。

### 11.1 对 ADR-001 D10 的修订（F2，owner 已确认）

**修订条文**：D10 第 1 条"不做 busy handler"增加一个例外——**主库（store.sqlite）上的连接**
（cas-store 一条、fs-workspace 一条）设置 `busy_timeout = 5000 ms`。

| 项 | 修订后 |
| --- | --- |
| 适用连接 | 仅主库的两条连接；overlay 连接保持 `busy_timeout = 0`（独占，W2，不存在竞争） |
| 性质 | **等锁**：同一条语句在 SQLite 内部等待写锁释放。不是"操作失败后再执行一次"，不重放任何已执行的步骤 |
| 超时 | 仍返回类型化 `Busy`（附等待上限），调用方看到的错误类型与 P1 相同 |
| 不变的部分 | D10 第 1 条其余内容（不自动重试、不退避重试、不做错误驱动的替代路径）、第 2–5 条全部不变；不把竞争错误翻译成成功 |
| 参数 | 5000 ms 写入 `workspace_policy`（`main_busy_timeout_ms`），打开时读回校验 |

**修订理由**：P1 的 `busy_timeout = 0` 建立在"任意写竞争都应该立即暴露"的前提上。workspace 独占
之后，主库上的竞争只剩跨 workspace 的发布与 serial 预留——短事务、正确使用下也必然发生。保持 0 会让
正确的并发使用随机失败（大快照几乎必然失败），而等待一个短事务结束不改变任何操作的语义。

**测试**：两个 workspace 并发 snapshot 各 1000 个文件，均成功；人为持有主库写锁 > 5 s 时，发布方
在约 5 s 后得到 `Busy`（断言等待时长有界，而不是立即失败或无限等待）。

- **对 ADR-002 §8 的修订（同一次 F2 裁决）**：ADR-002 §8 的“`busy_timeout` 保持 0”对**默认调用方**
  不变。主库上的两条连接（cas-store 的索引连接、fs-workspace 的元数据连接）通过
  `Store::open_with(..., StoreOptions { busy_timeout })` 显式选择有界等待，两者取**同一个参数源**
  （`workspace_policy.main_busy_timeout_ms`），因此不会出现两份配置。ADR-002 §8 的原文与其既有测试
  （`a_cross_process_writer_reports_busy_without_retrying`）一字不改。
- 替代方案（未采纳）：[LFS] 式进程内 FIFO 准入 + 跨进程目录锁 + 发布通道——复杂度高，P2a 不需要。

## 12. 批量发布（ADR-002 §5 修订）**[F7 已确认]**

> **已由 ADR-008 §5 取代（2026-10-23）**：P2a 实测回答了 F7 的"批量是否达标"（每对象一次文件 fsync
> 是瓶颈），本节的"对象文件逐个 durable + 索引分块提交"不再实现，直接改为 pack 批量发布。
> 衔接说明见 ADR-008 §2.1。下文保留为历史记录。

```rust
let mut batch = store.batch();             // PageSink 的实现之一
let oid = batch.put(role, value)?;         // tmp → fsync → rename；不写索引
batch.commit()?;                           // fsync 涉及的前缀目录（合并）；索引行分块提交
```

- 一批内重复对象在内存中去重；已在索引中的对象只查索引（`AlreadyPresent`，不写文件）；
- 索引按块提交（例如每 4096 行一个事务），**块按 put 顺序**提交，所以子对象的索引行不晚于父对象
  （"已发布对象只引用已发布对象"在对象图内成立）；限制单事务大小与锁持有时间；
- 崩溃：已 rename 未入索引的文件是 ADR-002 §5.2 已有的孤儿形态（读者忽略、后续同内容 put 复用），
  **恢复协议不变**；
- 预期收益：省去每对象一次索引事务的 fsync；每个对象文件的 fsync（Windows：FlushFileBuffers）仍在。
  10 万新小文件的首次快照实测由 P2a-5/P2a-8 给出，**不预先宣称**倍数；不达标 → P2b pack。

## 13. 对外 API（fs-api）与 CLI

```rust
Repo::open(store_dir, OpenMode) -> Repo
  .init() / .branch_create(name, from) / .branches() / .log(branch)
  .reset(branch, to, expect: Option<CommitId>) / .revert(branch)
  .workspace_create(name, branch) / .workspace_open(name) -> Workspace / .workspace_list()
  .workspace_discard(name) / .workspace_rebind(name, branch)   // rebind：§10.3 出路 1
  .view(commit_or_root) -> ReadView                      // read_at / stat / read_dir；零表依赖
  .verify() / .verify_deep(root) / .stats()
Workspace
  .read_at / .write_at / .truncate / .create / .mkdir / .remove / .rename / .stat / .read_dir
                                                          // 签名与 ADR-004 的 P1 Fs 一致
  .flush() / .status() / .snapshot() -> RootId / .commit(msg, force) -> CommitId
  .restore(commit, discard) / .close()
```

- 保持无状态、路径式（ADR-004 §3.1 的理由仍成立）；句柄、delete-pending、share mode 仍归 P3；
- 不提供"自动 workspace + 立即 snapshot"的直写便利层（manager 与 architect 一致：组合语义只是多一条
  可走的路径）。

CLI（owner 定的 git 式）：

```text
init
branch [list | create <b> [--from <b|commit>]]
workspace [create <w> --branch <b> | list | status <w> | discard <w> | rebind <w> <b>]
--workspace <w> put|get|ls|stat|mkdir|rm|mv ...      # 每条命令退出前 flush
commit --workspace <w> -m <msg> [--force]
snapshot --workspace <w>
log <b> ; reset <b> <commit> [--expect <c>] ; revert <b>
restore --workspace <w> <commit> [--discard]
ls|get|stat --at <commit|root> <path>                # 读历史（I1 的验收入口）
verify [--deep <commit|root>] ; stats
```

## 14. 版本与退场

- 主库：`workspace_policy.schema_version = 3`。打开时发现 `namespace_policy`（P1 schema 1/2）→
  `SchemaMismatch`，提示重建；**不迁移**（P1 零部署，manager 已确认）；
- cas-store schema 不变（批量发布不改表）；
- overlay：`ov_meta.schema_version = 1`，按 overlay 自校验；以后升级 overlay schema 时要求 workspace
  先 snapshot/discard，不迁移；
- P2a-7 删除：fs-api 直写路径（`Fs`）、fs-namespace 的 SQLite 表后端与 `db.rs`/`schema.rs`/
  `content.rs`、D+ root 链与其访问器（`head_root`/`root_record`/`root_records`）。对应测试迁移为
  trait 契约测试或删除（逐个说明）。

## 15. 留给 P2b 的接口

- **GC 根集合** = 每个 branch 的 head 沿 parent 链的全部根 ∪ `branch_log` 中出现过的提交及其祖先
  ∪ `snapshot` 表 ∪ 每个 workspace 的 `base_root`/`pending_root`/`root(base_commit)`。P2a 记录
  全部这些，**不删除任何东西**；保留策略（例如 `snapshot` 与 `branch_log` 的过期）由 P2b 定；
- **Seal**：`ov_page` 以后可增加"引用已封存 chunk"的行类型（[LFS] `fs_data_ref` 类似），属于 overlay
  schema 升级；
- **CDC**：只替换物化器中"块 → 对象"的切块步骤（ADR-005 S10）。

## 16. 未实现特性（明确清单）

| 特性 | 去向 |
| --- | --- |
| snapshot 期间不阻塞写（[LFS] 式 generation 捕获） | P2b 以后 |
| 增量物化 | 以后（格式已支持） |
| merge / 三方合并 / 任意提交的 revert | 未计划于 P2 |
| 同一 workspace 多进程写 | 不做（W2） |
| Seal、CDC、GC、pack | P2b |
| 掉电持久性的测试证明 | 无法在本项目测试中构造，依据 SQLite 文档（§8） |
| 句柄、delete-pending、文件锁 | P3 |

## 17. 正确性风险与已知限制

1. **全量物化** O(可达 inode 数) 的遍历与 inode 表重编码，每次 snapshot 都付；写盘只发生在变化部分。
   千万级文件时内存（inode 记录排序）需要外部排序——P2a 不处理，P2a-8 报告 10 万级实测；
2. **大 overlay**：1 GiB 改写在 snapshot 前全部以 4 KiB 页存在 overlay 库里（约 1 GiB + 页开销）；
3. **W4 的前缀合约依赖 SQLite**：WAL + NORMAL 的掉电语义未经本项目测试（§8）；
4. **anonymous snapshot 无保留上限**：`snapshot` 表只增不减，直到 P2b 定保留策略；
5. **F1 选 (b) 时的用户体验**：没有 merge，冲突后只能另建 branch、restore 后重做或 force。

## 18. 测试清单（P2a-4 / P2a-6 / P2a-8 验收）

1. **参考模型随机序列**（spec §6 P2 第 5 条）：内存参考模型（路径 → 字节/目录）对照 workspace，
   随机 create/mkdir/write_at/truncate/rename/unlink/rmdir 序列，每步逐字节核对 stat/read_dir/read_at；
   序列中随机插入 flush、snapshot、commit、崩溃重开；
2. **K1–K6** trait 契约，表后端与 overlay 后端都跑；
3. **10 个 workspace 同 base**（spec §6 P2 第 1 条）：并发乱序写/截断/删除，互不影响；
4. **W4 崩溃合约**：§9.4 每个注入点 × 参考模型核对"前缀 ⊇ 最后 flush"；
5. **历史可读**：commit 后继续修改，`--at` 读旧提交逐字节等于提交时的视图；**删除全部 overlay 文件
   与 workspace 行后旧提交仍可读**（零表依赖，I1）；
6. **F1**：两个 workspace 同 head 先后 commit → 第二个 `BranchMoved`，overlay 不变；`--force` 后
   `branch_log` 有被取代的 head；
7. **reset / revert / restore** 各自的历史形态（revert 增长、reset 不增长、restore 不动 branch）；
8. **NothingToCommit**；
9. **workspace 独占**：第二个进程打开同一 workspace → `WorkspaceInUse`；
10. **Busy**：两个 workspace 并发 snapshot 不因锁失败（F2 = 有界等待时）；超时路径返回类型化 `Busy`；
11. **性能**（spec §8-6，三次重复报 P50，注明载荷特征）：write_at / flush 的 P50/P95/P99；snapshot
    耗时对树规模（1k/10k/100k 文件）与变更量（1 文件 / 1% / 100%）；每次 snapshot 新增对象数与字节数；
    dirty_bytes 与写放大。
