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
| N2 | 版本**按 crate 治理**：`namespace_policy.schema_version = 1`；`user_version` 保持 0 | 已实现 |
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
    file_id INTEGER PRIMARY KEY,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    content_root TEXT,
    created_ns INTEGER NOT NULL,
    modified_ns INTEGER NOT NULL
) STRICT;

CREATE TABLE directory_entry (
    parent_id INTEGER NOT NULL,
    name BLOB NOT NULL,
    child_id INTEGER NOT NULL,
    PRIMARY KEY (parent_id, name)
) STRICT;

CREATE TABLE root (
    root_id INTEGER PRIMARY KEY,
    parent_root_id INTEGER,
    op TEXT NOT NULL,
    created_ns INTEGER NOT NULL
) STRICT;
```

**这两张表刻意不带 `root_id` 列。** 初稿按方案 A（整表按 root 复制）设计了 `(root_id, ...)`
复合主键；A 已被判定不可行（§4.1），而 B/C/D+ 都不需要把每行绑定到 root。因此这里采用
**方案无关的中性形态**：当前树是单份可变的行集合。若 owner 最终选 C（对象化目录页），
行会迁出 SQLite 成为 CAS 对象，届时走 schema 版本 3 + 新 ADR。

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

**`PRAGMA user_version` 保持 0，不使用。** 保留给未来可能的全局格式断裂（例如整个文件不再是
SQLite）。

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
BEGIN IMMEDIATE
  1. 解析并校验路径（纯计算，校验失败 → 回滚）
  2. 读取所需行（当前 head 与父/目标条目）
  3. 计算新状态（新 FileId）
  4. 写入变更行
  5. publish_tx(op)   ← 追加一行 root 记录并推进 head_root
COMMIT
```

**任何失败都不暴露部分更新**：步骤 1–5 全在一个事务里；异常路径直接 `ROLLBACK`。

**`publish_tx` 是唯一接缝**：D+ 下它只追加一行 root 记录并推进 head；P2 改 C 时只替换这一步，
路径校验、行读写、操作语义、测试都不随之变化。

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
| 句柄所有权 | 句柄含 `FileId` + 打开的 root 快照；句柄在 root 变更后仍读**打开时**的视图 |
| 关闭后使用 | `Handle` 消费式关闭，关闭后使用是编译期错误（所有权），不是运行时错误 |
| `fsync` | P1 **不提供**显式 flush；每次写操作即一次事务 |

最后一条特别说明：spec §3 草案含 `flush`，但 P1 每写一事务，没有"缓冲未刷"状态，因此
`flush` 是 no-op。T4 若要暴露它，必须如实标注为"当前无操作"，不得暗示它有持久化含义。

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

1. **root 不可变性的实现方式未定**（§4.1）：A 已被判定不可行，D+/B/C 待 owner 裁决。在裁决前
   `publish` 接缝按 D+ 的最小形态实现；若改为 C，该步会被整体替换。
2. **serial 永不复用**意味着长时间运行的 Store 会持续消耗 serial；`u64` 在实践中足够。
3. **时间戳用系统时钟**，可被外部调整；P1 不用它做任何判定。
4. **root 记录无限增长**：没有 GC（P2.11）。每次操作新增一条发布记录。
5. **并发只测到线程与单机跨进程**，不测分布式。
6. **`user_version` 严格相等**意味着本 ADR 一旦实施，T2 时代的 Store 全部不可打开——这是
   刻意的（§4.2）。
7. **无递归删除**：T3 只有空目录 `rmdir`，删非空目录需要调用方逐层清理。

## 12. 测试清单（对照验收标准）

先按 §3 的枚举写测试，再补事务与并发。

| 类别 | 覆盖（§3 编号） | 代表性测试名 |
| --- | --- | --- |
| 路径无效 | P1–P9 每种形式各一例 | `rejects_*_path` 系列 |
| 类型冲突 | T1–T6 | `mkdir_*`、`unlink_*`、`rmdir_*` |
| rename 组合 | R1–R12 | `rename_*` 系列 |
| 大小写 | C1–C3 | `case_sensitive_names_coexist`、`rename_case_only_change_is_a_real_rename` |
| root 与事务 | X1–X4 | `failed_operation_leaves_previous_state` |
| 并发 | 线程级不同路径、同路径；跨进程至少一例 | `concurrent_*` |

### 12.1 T5 / QA 的 root 验收钩子（D+ 专有）

| # | 断言 | 说明 |
| --- | --- | --- |
| D1 | 写前已存在的 root 记录在写后**逐字节不变** | append-only：`(root_id, parent_root_id, op, created_ns)` 四列全等 |
| D2 | 事务中途崩溃 → **无新 root 记录且树不变** | 强制杀进程后重开，`head_root` 与失败前相同 |
| D3 | root 链 `parent_root_id` **完整无断裂** | 从 head 沿 parent 回溯必达 `NULL` 头，且每条 parent 都存在 |
| D4 | 变更型操作恰好追加**一行** root 记录 | 计数差 == 1 |
| D5 | no-op（rename 自身）与只读操作**不追加** | 计数差 == 0 |

具体测试名在提交说明中与每条声明配对列出（本卡新格式要求）。

## 13. 下一步

- **待 owner**：§4.1 的 root 实现方式。裁决后替换 `publish` 接缝并补对应测试。
- **T4**：`fs-api` + CLI，实现 §9 的语义草图与文件内容操作。
- **T5**：规模与性能，用真实数据验证所选 root 方案。
