# ADR-003：命名空间 schema、路径语义与 root 发布事务

- 状态：已接受（Accepted），适用于 P1 / Store schema 版本 2
- 日期：2026-10-22
- 决策者：manager（任务卡 T3；spec §1 P1-3/P1-5/P1-6、§3 数据模型与 API 草案、§4、§6 P1 矩阵；ADR-001 §5；ADR-002；`docs/architecture.md` §1–§3）
- 实施者：T3（本 ADR 与 `crates/fs-namespace` 同步提交）
- 相关：[ADR-001 对象编码](ADR-001-object-encoding.md)、[ADR-002 Store 布局](ADR-002-store-layout.md)

本 ADR 记录命名空间的**持久化 schema、路径语义、事务模型与并发语义**。标注"已实现"的条目
有测试覆盖；"决策"条目是 T4/P2 必须遵守但本卡未实现的契约；"未实现"清单里的特性一律不存在。

---

## 1. 背景与问题

ADR-001 定死了对象身份，ADR-002 定死了对象与索引的落盘。两者都只回答"一个内容对象怎么存"，
不回答：

1. 目录、文件名、文件身份（FileId）怎么表示与持久化；
2. 路径如何解析，哪些路径必须被拒绝；
3. 一次 `mkdir`/`rename`/`unlink` 如何原子地产生一个"新的、不可变的文件系统视图"；
4. 旧视图如何保证不被后续写入破坏；
5. 并发下同路径写入的语义；
6. FileId 如何分配且不跨命名空间碰撞。

spec §3 给出概念模型，spec §11 提出开放决策（大小写、事务粒度）；`docs/architecture.md` 规定
`fs-namespace` 拥有 FileId、目录条目、root 发布事务与路径操作，**禁止知道**对象物理布局。

## 2. 决策摘要

| # | 决策 | 状态 |
| --- | --- | --- |
| N1 | 扩表到同一 `store.sqlite`，`fs-namespace` 开**第二条连接** | 已实现 |
| N2 | 版本**按 crate 治理**：`namespace_policy.schema_version = 1`；`user_version` 由 `cas-store` 治理，本 crate 不使用 | 已实现 |
| N3 | 表：`namespace_policy`、`namespace`、`file_id_allocator`、`file`、`directory_entry`、`root` | 已实现 |
| N4 | FileId = (namespace, serial)，serial 由分配器单调发放，**永不复用** | 已实现 |
| N5 | 每次命名空间操作 = 一个事务：改行 + 发布 root | 已实现 |
| N6 | root = D+：当前树可变 + `root` 表 append-only（owner 确认）；P1 无历史树视图 | 已实现 |
| N7 | 路径语义：绝对、以 `/` 分隔、区分大小写、拒绝 `.`/`..`/空段/重复分隔符/NUL | 已实现 |
| N8 | 同路径并发 = 后写生效，无 CAS/冲突错误（P2 引入 Branch 时升级） | 已实现 |
| N9 | `fs-namespace` 只经 `cas-store` 的 `Store::put/get` 写对象 | 已实现 |
| N10 | `mkdir` 对已存在目标严格报 `AlreadyExists` | 已实现 |
| N11 | `rmdir` 删空目录，非空报 `DirectoryNotEmpty` | 已实现 |
| N12 | 文件内容读写（`create`/`read_at`/`write_at`/`truncate`） | 决策（T4） |

## 3. 违反形式枚举（先枚举，再写校验）

T2 的 `objects/` 校验曾经补漏三轮（名字 → 位置 → 目录），根本原因是"写完检查再想还能怎么坏"。
本节先把**所有违反形式**列全，再据此设计校验与测试。这是 manager 指定的方法论，也是本 ADR 的
组织方式。

### 3.1 路径无效的全部形式

| # | 形式 | 例子 | 结果 |
| --- | --- | --- | --- |
| P1 | 不以 `/` 开头 | `a/b` | `InvalidPath::NotAbsolute` |
| P2 | 含空段（`//`、结尾 `/`） | `/a//b`、`/a/` | `InvalidPath::EmptySegment` |
| P3 | 含 `.` 段 | `/a/./b` | `InvalidPath::DotSegment` |
| P4 | 含 `..` 段 | `/a/../b` | `InvalidPath::DotDotSegment` |
| P5 | 名字超过 255 字节 | 256 字节名 | `InvalidPath::NameTooLong` |
| P6 | 名字为空（`""` 作为段） | — | 同 P2（空段） |
| P7 | 名字含 NUL 或 `/` | `a\0b` | `InvalidPath::ForbiddenByte` |
| P8 | 路径深度超过上限（64） | 65 层 | `InvalidPath::TooDeep` |
| P9 | 非 UTF-8 | 无效字节序列 | `InvalidPath::NotUtf8` |

说明：

- 根路径 `/` 是**合法**的，代表根目录，单独处理（它没有段）。
- `.`/`..` 一律拒绝而非解析：P1 不提供"规范化"，调用方给出什么就是什么。这避免了
  "`/a/../b` 与 `/b` 是同一个文件吗"这类跨平台歧义（Windows 前端 P3 自行处理）。
- 上限 255 字节对齐常见文件系统单段限制；深度上限 64 是**显式**准入，非静默截断。

### 3.2 目录与文件类型冲突的全部组合

设目标路径已存在时，按目标类型分：

| # | 操作 | 目标存在且为目录 | 目标存在且为文件 | 目标不存在 |
| --- | --- | --- | --- | --- |
| T1 | `mkdir` | `AlreadyExists` | `AlreadyExists` | 创建 |
| T2 | `stat` | 返回目录记录 | 返回文件记录 | `NotFound` |
| T3 | `read_dir` | 列出条目 | `NotADirectory` | `NotFound` |
| T4 | `unlink` | `IsADirectory`（删目录用 `rmdir`） | 删除成功 | `NotFound` |
| T5 | `rmdir` | 空 → 删除；非空 → `DirectoryNotEmpty` | `NotADirectory` | `NotFound` |
| T6 | `rename` | 见 §3.3 | 见 §3.3 | 见 §3.3 |

父路径不存在的所有操作 → `NotFound`（父）。父路径存在但为文件 → `NotADirectory`（父）。

> **T1 的裁决记录**：初稿曾把 `mkdir` 对已存在目录定为幂等成功（对齐 `mkdir -p` 直觉）。
> manager 推翻了它：这是文件系统原语层，不是 shell 便利层——POSIX `mkdir(2)` 返回 `EEXIST`、
> Win32 `CreateDirectory` 返回 `ERROR_ALREADY_EXISTS`、Rust `create_dir` 与 Node `fs.mkdir`
> 同此，**所有先例都报错**。幂等成功会静默吞掉拼写错误与权限问题等真实异常；
> "确保存在"的语义归上层封装（T4 的 `fs-api`/CLI 可提供）。

> **T5 的裁决记录**：`rmdir` 本不在 spec §2 P1-5 的 API 清单里，初稿把它记为缺口。manager
> 裁定加入 T3：`rmdir` 是命名空间操作，架构上归 `fs-namespace`（`fs-api` 只组合不实现）；
> spec §1 的最终目标写明"创建目录、文件、随机读写、**删除**、重命名、列目录"，P1-5 清单漏列
> 属规格笔误。根目录不可删（`IsRoot`）。

### 3.3 rename 的全部冲突组合

| # | from | to | 结果 |
| --- | --- | --- | --- |
| R1 | 不存在 | — | `NotFound { from }` |
| R2 | 存在 | `to` 不存在 | 成功 |
| R3 | 存在 | `to` 存在且为**文件**，`from` 为文件 | 覆盖 `to`（P1 语义） |
| R4 | 存在 | `to` 存在且为**目录**，`from` 为文件 | `AlreadyExists`（不覆盖目录） |
| R5 | 目录 | `to` 存在且为文件 | `AlreadyExists`（不覆盖文件） |
| R6 | 目录 | `to` 存在且为**非空目录** | `DirectoryNotEmpty` |
| R7 | 目录 | `to` 存在且为**空目录** | 覆盖（删除 `to`，移入 `from`） |
| R8 | — | `to` 与 `from` **完全相同**（逐字节） | no-op 成功，**不发布新 root** |
| R9 | 目录 | `to` 在 `from` **子树内** | `InvalidRename::IntoOwnSubtree` |
| R10 | — | `to` 的父不存在 | `NotFound { to_parent }` |
| R11 | — | `to` 的父是文件 | `NotADirectory { to_parent }` |
| R12 | `from` 的父是文件 | — | `NotADirectory { from_parent }` |

R8 选择"成功但不发布新 root"而非报错：重命名到自己是最常见的一类惰性操作（脚本重复执行），
报错会逼调用方先 `stat`。这与 POSIX `rename(a, a)` 的 "perform no other action" 一致。
但它**不得**产生新 root，否则每次惰性调用都会污染历史。

**R8 只适用于逐字节相同的路径。** 核心区分大小写（§3.4 C1），因此 `/A → /a` 是**真实重命名**，
必须走 R2/R3 而不是 R8 的自重命名捷径——若误判为 self，`/A` 会既没改名又报成功。测试必须
显式覆盖这一条（见 §12 `rename_case_only_change_is_a_real_rename`）。

### 3.4 大小写相关

| # | 情形 | 结果 |
| --- | --- | --- |
| C1 | 同目录下 `A` 与 `a` 并存 | **允许**（ADR-001 D8：核心区分大小写） |
| C2 | `rename("/A", "/a")` 当 `a` 不存在 | 成功，`A` 变成 `a` |
| C3 | `rename("/A", "/a")` 当 `a` 已存在 | 按 §3.3 处理（文件覆盖文件 R3） |

C1 是核心语义；Windows 前端（P3）必须在映射时**先检测**这种冲突再决定行为，不得让上层出现
"两个都能打开"的不确定状态（ADR-001 §5.2）。

### 3.5 root 与事务的违反形式

| # | 情形 | 结果 |
| --- | --- | --- |
| X1 | 操作中途失败（语句错误） | 事务回滚，**旧 root 完好且是当前 root** |
| X2 | 读取旧 root 记录 | 永远成功（旧 root 不可变） |
| X3 | 读取不存在的 root_id | `RootNotFound` |
| X4 | 命名空间不存在 | `NamespaceNotFound` |

## 4. 持久化 schema

同一 `store.sqlite`，由 `fs-namespace` 在**自己的连接**上创建与校验（§4.2.1）。新增表：

```sql
CREATE TABLE namespace_policy (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE namespace (
    name TEXT NOT NULL PRIMARY KEY,
    head_root INTEGER NOT NULL
) STRICT;

CREATE TABLE file_id_allocator (
    namespace TEXT NOT NULL PRIMARY KEY,
    next_serial INTEGER NOT NULL
) STRICT;

CREATE TABLE file (
    namespace TEXT NOT NULL,
    file_id INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    content_root TEXT,
    created_ns INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL,
    PRIMARY KEY (namespace, file_id)
) STRICT;

CREATE TABLE directory_entry (
    namespace TEXT NOT NULL,
    parent_id INTEGER NOT NULL,
    name BLOB NOT NULL,
    child_id INTEGER NOT NULL,
    PRIMARY KEY (namespace, parent_id, name)
) STRICT;

CREATE TABLE root (
    root_id INTEGER PRIMARY KEY,
    namespace TEXT NOT NULL,
    parent_root_id INTEGER,
    op TEXT NOT NULL,
    created_ns INTEGER NOT NULL
) STRICT;

CREATE INDEX root_by_namespace ON root (namespace, root_id);

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

> 本节代码块与 `crates/fs-namespace/src/schema.rs` 的 `DDL` 常量**逐字一致**，那是打开时逐字
> 比对的权威副本。QA 复验指出本节曾停留在 T3 首版形状（`file`/`directory_entry` 无
> `namespace` 列、`root` 无 `namespace` 列与索引）——F-T3-3 改了实现与 §4.1，却没回头同步
> 这里。**同一类"实现前进、文档未跟随"的缺陷**，已修正。

**这些表刻意不带 `root_id` 列。** 初稿按方案 A（整表按 root 复制）设计了 `(root_id, ...)`
复合主键；A 已被判定不可行（§4.1），而 B/C/D+ 都不需要把每行绑定到 root。因此这里采用
**方案无关的中性形态**：当前树是单份可变的行集合。若将来选 C（对象化目录页），行会迁出
SQLite 成为 CAS 对象，届时走 schema 版本 2 + 新 ADR。

`root` 表的 `op` 列记录发布原因（`mkdir`/`rmdir`/`rename`/`unlink`/`create`），是 append-only
发布记录的一部分；D+ 若还需 `parent` 链字段，在裁决后追加（属同版本的附加列，P1 零部署，
可直接改 DDL）。

### 4.1 root 的实现方式：D+（owner 已确认）

T3 卡的措辞"root 不可变：写前 root 的完整视图在写后逐项不变"存在歧义，已升级负责人裁决。
候选方案与代价：

| 方案 | 每次操作代价 | 读取代价 | 判定 |
| --- | --- | --- | --- |
| A 按 root 整表复制 | O(树大小) | O(1) | **不可行** |
| B 按 root 差异链 | O(变更) | O(链长)，需压缩/GC | P2 级复杂度 |
| C 写时复制目录页（对象化） | O(深度) | O(深度) | 正确终态，P2 级工作量 |
| **D+ 当前树可变 + root 记录 append-only** | **O(1)** | O(1) | **已采纳（owner 确认）** |

**A 不可行的关键算术**（manager 指出，我初稿低估了）：spec §6 要求创建 10 万小文件，A 意味着
第 k 次操作写 k 行，总量 O(n²) ≈ 50 亿行写入，直接撞穿 2 分钟测试红线。它不是"慢"，是做不到。

#### D+ 的三点语义（owner 确认）

1. **P1 的 root 是发布点 / 审计链**：`root` 表 append-only，每行
   `(root_id 位置性 serial, parent_root_id, op, created_ns)`。不接受内容哈希身份——每操作
   O(1) 是 D+ 的全部意义。保证是：**append-only、原子发布、崩溃恢复**。
2. **P1 局限（如实记录）**：
   - **不提供历史树视图**——只有当前树可读；"旧 root 的完整视图读取"按 spec P2.8 明文划归
     Snapshot（P2）。若 P1 就要求旧树完整可读，P2.8 的 Snapshot 交付就冗余了。
   - **每操作写放大 O(1)**（一条 root 记录），不是 A 的 O(树大小)。
3. **P2 演进路径**：对象式 CoW 命名空间——目录页/inode 表成为 CAS 对象（ADR-001 已预留
   role 5–8），届时另立 ADR。

#### 多命名空间语义（F-T3-3）

`root`、`file`、`directory_entry` 三张表**跨命名空间共享**，每行带 `namespace` 列：

| 概念 | 作用域 | 说明 |
| --- | --- | --- |
| `namespace.head_root` | 每命名空间 | 当前发布指针；不同命名空间的链互相不可见 |
| `file.file_id` | 每命名空间 | `PRIMARY KEY (namespace, file_id)`：两个命名空间都有 serial 0 的根 |
| `directory_entry` | 每命名空间 | `PRIMARY KEY (namespace, parent_id, name)` |
| `root.root_id` | **全局** | 全局 surrogate key，仅保证行唯一；不是"某命名空间的第几个 root" |
| `root.parent_root_id` | 每命名空间 | 链**按命名空间闭合**：沿 parent 回溯必达该命名空间的 `NULL` 头 |

所有读取入口（`head_root` / `root_record` / `root_count` / 每个操作的每次查询）都以
`namespace` 限定，因此一个命名空间既看不到别的命名空间的条目，也数不到它们的 root。

> **这是实现期间发现的缺陷**：初版把 `file_id` 作为全局主键、`root_id` 用全局 `MAX+1`，
> 结果第二个命名空间的初始化与第一个的 serial 0 相撞（`UNIQUE constraint failed`）。
> `namespaces_keep_separate_root_chains` 测试抓到了它。多命名空间本身虽超出 T3 卡面，
> 但 `Namespace::open` 按名字取命名空间已是 API 形状，文档必须显式化而不是靠读者推断。

#### 发布规则

| 情形 | 是否追加 root 记录 |
| --- | --- |
| `mkdir` / `rmdir` / `rename`（真实改名）/ `unlink` / `create` / `write_at` / `truncate` | **是**，恰好一行 |
| 只读操作（`stat` / `read_dir` / `open`） | 否 |
| 已裁决的 no-op（`rename` 逐字节同一路径，R8） | 否 |
| 首个 root（空树初始化） | 是，链的头（`parent_root_id = NULL`） |

#### 读取面（保持最小）

操作 API 一律读写**当前树**，不暴露"按 root 读"（那是 P2 Snapshot 的门）。为测试与 T5 保留
最小只读访问器：

- `head_root() -> Option<u64>` —— 当前 root serial；
- `root_record(root_id) -> Option<RootRecord>` —— 按 serial 查单条发布记录。

两者都**不**提供历史树内容。

#### 实现形态

"发布 root"实现为单一内部函数 `publish_tx(...)`，只在操作事务内被调用一次。P2 改为 C 时只替换
它，其余代码与测试不受影响。

### 4.2 schema 自校验与版本策略：按 crate 治理

**每个 crate 拥有自己的 policy 表与 DDL 自校验**，同一模式并行存在：

| crate | 版本载体 | DDL 自校验 |
| --- | --- | --- |
| `cas-store` | `store_policy.schema_version` | `schema_definition.ddl` 逐字比对（ADR-002 §4.1） |
| `fs-namespace` | `namespace_policy.schema_version`（当前 = 1） | `namespace_policy.ddl` 逐字比对 |

两 crate **互不校验对方的表**。

**`PRAGMA user_version` 由 `cas-store` 治理，`fs-namespace` 不使用它。** `cas-store` 在建库时
把它设为 1，并在每次打开时按自己的版本策略校验（ADR-002 §4.2）；`fs-namespace` 既不读写也不
校验它，只用自己的 `namespace_policy` 表。这两个版本载体互不相干，任一 crate 的版本演进都不
影响对方。

> **为什么不用 `user_version` 承载版本**（manager 裁决，我初稿曾这么设计）：
> `user_version` 是**全库一个**。若 `cas-store` 严格校验它，而 `fs-namespace` 装表后把它 bump 到 2，
> `cas-store` 再打开就会拒绝自己的 Store——除非它知道 namespace 的 DDL 版本，这就把两个 crate
> 焊死了。按 crate 治理消除了这个耦合：任一 crate 的版本演进只影响它自己的表。

`namespace_policy` 表：

```sql
CREATE TABLE namespace_policy (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
```

其中 `schema_version = 1`、`ddl = <本 crate 的建表 SQL 原文>`。

兼容性声明（spec §8-3）：**P1 不承诺跨 schema 版本的 Store 兼容**。任一侧版本不符都显式拒绝，
调用方重建（P1 零部署，无迁移包袱）。

### 4.2.1 双连接与"跨连接无事务"（文档化不变量）

`fs-namespace` **不开自己的数据库文件**，而是对 `cas-store` 的同一个 `store.sqlite` 开**第二条
连接**：

```text
         store.sqlite  (WAL)
            ▲        ▲
            │        │
  cas-store 连接   fs-namespace 连接
  对象文件 + 索引   命名空间表
```

- **路径所有权**：`fs-namespace` 不得自己拼 `store.sqlite` 文件名（文件名知识归 `cas-store`）。
  `cas-store` 增加最小访问器 `Store::metadata_path()`，这是本次唯一的 API 增量。
- **跨连接无事务**是明确的不变量：对象的 `put/get` 走 `cas-store` 的连接，命名空间行与 root
  发布走 `fs-namespace` 的连接，**两者不共享事务**。崩溃时的最坏情况是"对象已发布但无人引用"
  ——即孤儿（ADR-002 §10 允许保留），绝不是"root 指向不存在的对象"（§6 的定序保证）。
- **并发**：两条连接 + WAL 的并发由 SQLite 保证；`busy_timeout = 0` 的 `Busy` 语义不变，
  一次操作不重试。
- **错误**：`fs-namespace` 定义自己的类型化错误（`NamespaceError`），必要时包装保留
  `StoreError` 的语义，不复用其变体。
- **打开模式镜像**：`Namespace::open(&Store, OpenMode::{Create, ReadWrite, ReadOnly})`——
  建表/校验/只读语义与 `Store::open` 同构。两个 crate 的自检 plumbing 平行存在是解耦的代价，
  可接受；不抽公共工具 crate（YAGNI）。

### 4.3 FileId 分配

- `FileId` = `(namespace, serial)`，`serial` 是 `file_id_allocator.next_serial` 单调递增发放的
  `u64`（ADR-001 D9：身份须带 namespace/scope 防碰撞）。
- 发放与使用在**同一事务**内：分配器 `UPDATE ... RETURNING` 或先读后写，随操作一起提交。
- **serial 永不复用**：删除文件不归还 serial。这保证"已删除文件的 FileId 不会指向新文件"，
  避免陈旧引用（将来的快照/日志）指向错误对象。
- 分配器是**每命名空间**一行，不同命名空间的 serial 从 1 开始，靠 namespace 前缀区分。

## 5. 语义细节

### 5.1 路径与名称

- 名称是**原始 UTF-8 字节**（`directory_entry.name` 为 `BLOB`），比较逐字节、区分大小写。
- 不提供规范化：调用方给的 `A` 与 `a` 是两个名字（§3.4 C1）。
- 根目录的 FileId 是**固定的** `(namespace, 0)`——serial 0 保留给根，分配器从 1 开始。

### 5.2 时间戳

`created_ns` / `modified_ns` 为 Unix 纪元纳秒（`i64`）。P1 只用它们做展示与 `stat`，**不参与
任何身份或冲突判定**。写入时取当前系统时间；时钟回拨不报错，也不回退已记录的值（取
`max(now, existing)`）。

### 5.3 `mkdir` 的严格语义

对**已存在的目录或文件**一律报 `AlreadyExists`（§3.2 T1）。这是文件系统原语层的语义，
与 `mkdir(2)`/`CreateDirectory`/`create_dir` 一致（见 §3.2 的裁决记录）。需要"目录不存在才
创建、存在则忽略"的调用方，由上层（T4 的 `fs-api`/CLI）封装，核心不提供。

### 5.4 删除语义

- `unlink` 只删**文件**；对目录报 `IsADirectory`（删目录用 `rmdir`）。
- `rmdir` 只删**空目录**：非空报 `DirectoryNotEmpty`，根目录报 `IsRoot`，对文件报
  `NotADirectory`，不存在报 `NotFound`（§3.2 T5）。
- 删除**不回收 FileId 的 serial**（§4.3：永不复用）。

### 5.5 root 读取（D+）

- 当前状态由 `namespace.head_root` 给出。
- `head_root()` 与 `root_record(root_id)` 是**唯一**的只读访问面（§4.1"读取面"）：
  前者给当前 serial，后者给单条发布记录。
- **P1 不提供历史树内容读取**（D+ 的明确局限，§4.1 第 2 点）；完整快照归 P2。
- root 记录一经写入**不再修改**（append-only），`namespace.head_root` 是唯一可变的指针。

## 6. 事务模型

每次命名空间操作：

```text
with_write_tx:  BEGIN IMMEDIATE
  1. 解析并校验路径（纯计算；校验失败 → 不开事务直接返回）
  2. 读取所需行（当前 head 与父/目标条目）
  3. 计算新状态（新 FileId）
  4. 写入变更行
  5. publish_tx(op)   ← 追加一行 root 记录并推进 head_root
COMMIT
```

**任何失败都不暴露部分更新**：步骤 2–5 全在一个 `BEGIN IMMEDIATE` 事务里；`body` 返回错误
时事务被 drop，rusqlite 自动 `ROLLBACK`，旧 root 仍是当前 root（§3.5 X1）。

步骤 1 的纯校验（路径合法性、父目录存在、目标是否存在）在**开事务之前**完成，这是刻意的：
无效输入不应占用写锁。

**`with_write_tx` 是所有变更操作的唯一入口**，四个操作（`mkdir`/`rmdir`/`unlink`/`rename`）
全部经它执行；只读操作（`stat`/`read_dir`）与 no-op 路径不开启事务。

**`publish_tx` 是唯一接缝**：D+ 下它只追加一行 root 记录并推进 head；P2 改 C 时只替换这一步，
路径校验、行读写、操作语义、测试都不随之变化。

> **这是实现期间被 review 抓到的缺陷（F-T3-1）**：初版四个变更操作各自执行多条 autocommit
> 语句——`mkdir` 实际是 6 条独立事务。ADR 写了"单事务"、`publish_tx` 文档写了 "inside its
> transaction"，但代码里**没有那个事务**。表现是文档与代码的矛盾，本质与 T2 的"测试副本"同类：
> 声明先行，实现未跟上。修复引入 `with_write_tx`，并由 §12.2 的失败注入测试钉住。

**root 发布与对象写入的边界**：若一次操作需要新对象（T4 的 `write_at`），对象**先**经
`Store::put` 发布（ADR-002 的序列），**再**在命名空间事务里引用它的 ObjectId。这样崩溃时
最坏是多一个未引用的对象（孤儿，ADR-002 §10 允许），而不是一个指向不存在对象的 root。

## 7. 并发语义

- 所有命名空间操作经 SQLite 单写者串行化（`BEGIN IMMEDIATE`，`busy_timeout = 0`）。
- **同路径并发 = 后写生效**，无 CAS、无冲突错误。显式冲突检测留给 P2 的 Branch
  （`commit_branch` 的 expected_head 语义）。
- 不同路径的并发操作互不干扰（不同行），但会串行化（单写者）。
- **P2 引入 Branch 时，本语义升级为 CAS**：届时 `head_root` 的推进要带期望值，移动即报
  `HeadMoved`。本 ADR 记录这一演进方向，P1 不实现。

## 8. 模块边界（`docs/architecture.md` §2）

`fs-namespace` 拥有：FileId、目录条目、root 发布事务、路径操作。
**禁止知道**：对象物理布局（`objects/ab/cd`）、CLI、Windows 语义。

因此：

- 写对象只经 `Store::put`，取对象只经 `Store::get`；
- 不引入平台 API（`fs-namespace` 必须零平台依赖，连 `cfg(windows)` 都不该出现）；
- 不实现第二份对象编码（复用 `cas_core::Object`）；
- 文件 ≤999 行、`lib.rs`/`mod.rs` ≤200 行。

## 9. T4 的 API 语义草图（决策，未实现）

`fs-api` 的 `VirtualFileSystem` 落地时须明确下列问题，本 ADR 先给结论供 T4 对照：

| 问题 | 结论 |
| --- | --- |
| 部分读 | `read_at` 允许短读（返回实际字节数），EOF 时返回 0 |
| 部分写 | `write_at` 允许短写，返回实际写入字节数；调用方负责重试剩余部分 |
| 跨越 EOF 的写 | 自动扩展文件（补零），不是错误 |
| **句柄所有权** | **P1 不用句柄**：`fs-api` 以路径寻址、无状态（ADR-004 §3）。初稿写的"句柄含打开时 root 快照"在 D+ 下不可兑现——历史树不可读——已由 ADR-004 修订 |
| 关闭后使用 | 不适用（无句柄）。P3 的 WinFsp 层需要句柄，问题清单见 ADR-004 §3.2 |
| `fsync` | P1 **不提供**显式 flush；每次写操作即一次事务 |

最后一条特别说明：spec §3 草案含 `flush`，但 P1 每写一事务，没有"缓冲未刷"状态，因此
`flush` 是 no-op。T4 暴露它时必须如实标注为"当前无操作"，不得暗示它有持久化含义
（ADR-004 §4.5 的措辞即为此）。

## 10. 未实现特性（明确清单）

| 特性 | 计划阶段 | 说明 |
| --- | --- | --- |
| 文件内容读写 `create`/`read_at`/`write_at`/`truncate` | T4 | 本卡只定 schema 与 `content_root` 链接 |
| CLI 命令面 | T4 | `fs-cli` 仍为占位 |
| 硬链接 | P1 外 | 本卡不实现，`file` 表无 link 计数 |
| 符号链接 | P1 外 | 本卡不实现，`kind` 只取目录/文件 |
| 递归删除（`rm -r` 语义） | T4 决定 | T3 只提供空目录 `rmdir`（§5.4） |
| 历史 root 的完整树重放 | 待 owner 裁决 | §4.1；若采纳 D+，归 P2 Snapshot |
| Branch / Commit / Snapshot | P2 | 本卡只有线性发布记录 |
| 同路径 CAS 冲突检测 | P2 | 见 §7 |
| WinFsp / Windows 语义 | P3 | 无 |
| 权限位 / ACL / 所有者 | P3 | `file` 表只存 kind/size/content/times |

### 10.1 与规格的偏差

| 偏差 | 理由 |
| --- | --- |
| **增加了 `rmdir`** | spec §2 P1-5 的 API 清单未列它，但 spec §1 的最终目标写明"创建目录、文件、随机读写、**删除**、重命名、列目录"。`rmdir` 是命名空间操作，架构上归 `fs-namespace`（`fs-api` 只组合不实现），因此由 manager 裁定加入 T3。清单漏列视为规格笔误。 |
| **`mkdir` 严格报 `AlreadyExists`** | 与 spec 无冲突；spec 未定义该情形，本 ADR 按文件系统原语层惯例定案（§3.2）。 |

## 11. 正确性风险与已知限制

1. **P1 的 root 不承载内容身份**（§4.1，D+ 已由 owner 确认）：root 记录是发布点标记，
   不含内容身份；目录条目与 file 行属"当前树"，被后续操作覆盖。历史树视图归 P2 Snapshot。
   （初版本节曾写"实现方式未定、待 owner 裁决"，是 D+ 确认后未同步的陈旧表述，已修正。）
2. **serial 永不复用**意味着长时间运行的 Store 会持续消耗 serial；`u64` 在实践中足够。
3. **时间戳用系统时钟**，可被外部调整；P1 不用它做任何判定。
4. **root 记录无限增长**：没有 GC（P2.11）。每次操作新增一条发布记录。
5. **并发只测到线程与单机跨进程**，不测分布式。
6. **两个版本载体互不校验**：`cas-store` 的 `user_version` 与 `fs-namespace` 的
   `namespace_policy.schema_version` 各管各的（§4.2）。一方演进不会因为另一方而失败，但
   也意味着一方可能被旧二进制打开而另一方被拒绝——两个 crate 的拒绝各自独立生效。
7. **无递归删除**：T3 只有空目录 `rmdir`，删非空目录需要调用方逐层清理。

## 12. 测试清单（对照验收标准）

先按 §3 的枚举写测试，再补事务与并发。

| 类别 | 覆盖（§3 编号） | 代表性测试名 |
| --- | --- | --- |
| 路径无效 | P1–P9 每种形式各一例 | `rejects_*_path` 系列 |
| 类型冲突 | T1–T6 | `mkdir_*`、`unlink_*`、`rmdir_*` |
| rename 组合 | R1–R12 | `rename_*` 系列 |
| 大小写 | C1–C3 | `case_sensitive_names_coexist`、`rename_case_only_change_is_a_real_rename` |
| root 与事务 | X1–X4 | `failure_before_the_mutation_leaves_everything_unchanged`、`failure_after_the_mutation_rolls_the_rows_back`、`failure_rolls_back_every_mutating_operation`、`failed_operation_leaves_published_roots_intact`、`root_record_of_an_unknown_serial_is_absent`、`namespaces_keep_separate_root_chains`（逐条映射见 §12.2） |
| 并发 | 线程级不同路径、同路径；跨进程至少一例 | `concurrent_*` |

### 12.1 T5 / QA 的 root 验收钩子（D+ 专有）

| # | 断言 | 说明 |
| --- | --- | --- |
| D1 | 写前已存在的 root 记录在写后**逐字节不变** | append-only：`(root_id, parent_root_id, op, created_ns)` 四列全等 |
| D2 | 事务中途崩溃 → **无新 root 记录且树不变** | 强制杀进程后重开，`head_root` 与失败前相同 |
| D3 | root 链 `parent_root_id` **完整无断裂** | 从 head 沿 parent 回溯必达 `NULL` 头，且每条 parent 都存在 |
| D4 | 变更型操作恰好追加**一行** root 记录 | 计数差 == 1 |
| D5 | no-op（rename 自身）与只读操作**不追加** | 计数差 == 0 |

### 12.2 事务原子性失败注入（F-T3-2）

确定性故障注入，编译在 `failure-injection` feature 下（默认关闭，dev-dependency 自引用使
`cargo test` 启用）。三种故障都通过**真实 SQL 失败**制造，不是模拟标志：

| 故障 | 制造方式 | 落在事务的哪个阶段 |
| --- | --- | --- |
| `BeforeMutation` | 删除 `file_id_allocator` 行 | `mkdir` 事务体第一条语句 |
| `AfterMutation` | 把分配器指向已占用的 serial | 目录条目已写入、file 行插入时 |
| `BeforePublication` | 在 `root` 上建 `BEFORE INSERT` 触发器 `RAISE(ABORT)` | 所有四个操作的最后一步，行变更已完成 |

对应测试（`tests/atomicity.rs`）与 §3.5 的映射：

| §3.5 | 测试 |
| --- | --- |
| X1 行变更前失败 | `failure_before_the_mutation_leaves_everything_unchanged` |
| X1 行变更后失败（回滚已写行） | `failure_after_the_mutation_rolls_the_rows_back` |
| X1 四个操作统一回滚 | `failure_rolls_back_every_mutating_operation` |
| X2 已发布 root 不变 | `failed_operation_leaves_published_roots_intact` |
| X3 未知 serial 无记录 | `root_record_of_an_unknown_serial_is_absent` |
| X4 命名空间隔离 | `namespaces_keep_separate_root_chains` |

每个失败用例都断言三件事：**树逐项不变**（名称、类型、FileId 全等）、**root 计数不变**、
**head 不变**。

**变异证据**：把 `with_write_tx` 改回"不开事务"（即 F-T3-1 的原缺陷），
`failure_after_the_mutation_rolls_the_rows_back` 与
`failure_rolls_back_every_mutating_operation` 立即失败。这两个测试是真在测原子性，不是恒真。

具体测试名在提交说明中与每条声明配对列出（本卡新格式要求）。

## 13. Root 数据模型与事务协议（四问四答）

> 来源：`docs/research/2026-10-22-root-model-review.md`（owner 插入审查，2026-10-22）。
> 本节把审查结论固化进 ADR——此前这些内容散落在各节的"局限"里，读者拼不出来。

### Q1：目录条目、inode、文件内容引用如何对应一个确定 Root？

**内容意义上不对应（D+ 的既定语义）。** Root 记录是发布点标记
（`root_id`/`parent_root_id`/`op`/`created_ns`），**不含任何内容身份**；目录条目与 `file` 行
是可变共享表，属于"当前树"而非任何 root；`file.content_root → ObjectId` 是不可变的内容身份，
但"哪个 root 拥有哪些 content_root"的映射只存在于当前表。root N 与树状态的对应是**纯时序性**
的（op N 之后、op N+1 之前），且被后续操作覆盖。

spec §3 的 `FilesystemRoot { inode_table_root: ObjectId }` 描述的是**内容根形态**——即 P2
Snapshot 的目标形态，不是 P1 的。

### Q2：新版本产生后，旧 Root 如何在不依赖当前表状态的情况下读取？

**P1 不能。** root 记录本身可独立读取（append-only 行 + parent 链），但其**树状态不可读**
——已被可变表覆盖。这是 D+ 的既定局限（§4.1），不是缺陷。

**P2 必须满足的三条约束**（本次审查确立，现在前置）：

1. **Snapshot = 把当前树物化为纯 CAS 对象图**：目录页（role 5/6）、inode 表（role 7）、根对象
   （role 8，ADR-001 已预留），旧快照经对象根读取，**零表依赖**。这是 LayerFS
   overlay→commit 模式的等价物。
2. **快照编码参数冻结进 Store Policy**（页尺寸、每条目数等），同 CDC profile 的先例。
3. **P2 Branch 的 CAS 令牌是快照根（内容身份），不是 D+ 的 op 链 head**。op 链是**审计序列**，
   不是版本语义。此点现在写明，防 P2 误用。

### Q3：对象已写入 CAS，但 Namespace 事务失败时如何处理？

**对象成为 store 级孤儿**：有索引行、无命名空间引用。处理：**容忍**——只读、不删，内容寻址
天然幂等（后续同内容 `put` 直接复用）。回收归 P2.11 GC（根集合 = 各命名空间当前树 + 快照根）。

**两条方向性不变量**：

- 命名空间**绝不悬空引用**（失败方整体回滚，引用从未提交）——**永远成立**；
- 对象**可以孤儿**（无害、可复用）——**可能成立**。

反方向（悬空引用）**不存在**。

**闭包校验**替代逐操作依赖验证：`Fs::verify` 遍历全部 `content_root` 与 extent 的
`object_id`，确认存在于 store（ADR-004 §7）。逐操作验证在 D+ 下是 O(树)/op，不可接受；
spec §4 的"发布 Root 前验证依赖"由 **put-先于-引用的定序 + 闭包校验**共同满足。

### Q4：两个进程同时更新 Namespace，如何保证只有符合预期版本的更新成功？

**P1 不保证"符合预期版本"——这是刻意的。** 并发防护分三层：

| 层 | 机制 | 效果 |
| --- | --- | --- |
| ① | `BEGIN IMMEDIATE` 事务串行化 | 两个写者排队，不交错 |
| ② | 同路径**创建**的 PK 约束 + 事务隔离 | 第二方见类型化 `AlreadyExists` |
| ③ | 同路径**写** = 后写生效 | 与文件系统常态一致 |

**没有 CAS（预期版本检查）。** spec §4 明文把 CAS 归 P2 Branch Head，且 P2 的令牌是**快照根**
（Q2 第 3 条），不是 D+ 的 op-head。

**开放选项（呈 owner，manager 建议不加）**：P1 若希望乐观并发，可让 `fs-api` 写操作带
`expect_head` 参数，不符报类型化冲突（namespace 层事务内校验 `head_root`，约 20 行）。但 P1
的消费方（单机 CLI、T5 基准）用不到，且 P2 的正确令牌是快照根而非 op-head——**现在加会引入
错误的版本语义**。

## 14. 下一步

- **T4**：`fs-api` + CLI（ADR-004）。已实现；`file_extent` 表使本 crate 的
  `namespace_policy.schema_version` 从 1 升到 2（第二个版本演进先例）。
- **T5**：规模与性能，复用 ADR-004 §6 的读取放大计数；用真实 1 GiB 复测块路径。
- **P2**：Snapshot（Q2 的三条约束）、Branch（CAS 令牌 = 快照根）、GC（Q3 的根集合）。
