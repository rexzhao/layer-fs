# P2a-6：ADR-006 一致性核对（fs-workspace）

对象：`crates/fs-workspace`（新建），以及对 `crates/fs-overlay` 的接缝修改。
依据：`docs/adr/ADR-006-workspace-overlay.md`（Accepted）。逐节核对，**每条都注明是否有测试钉住**。

> 说明：任务卡里引用的 ADR 小节号与 ADR 正文存在系统性偏移（卡 §2.1→ADR §4，卡 §5→§9.2，
> 卡 §6→§9+§10，卡 §10→§10.3，卡 §11→§11.1），本报告一律按 **ADR 正文小节号** 组织。

---

## §4 主库表

七张表按 DDL 建在 `store.sqlite` 里，`workspace_policy` 记录 `schema_version=3` + `ddl` 原文 +
`snapshot_profile` + `overlay_page_bytes` + `main_busy_timeout_ms`，打开时**逐项读回校验**
（`main_db.rs::validate`），任一不符即 `SchemaMismatch`。

- 表名、列名、`STRICT`、主键与 ADR §4 的 DDL 草案逐字一致，只有两项差异：
  1. `workspace` 的主键是 `name`（ADR 原文也是 `name`），`id` 是 `UNIQUE`；
  2. ADR 的 `commit`/`branch`/`snapshot` 是 SQL 关键字或易歧义，DDL 里加引号——存储形态不变。
- `commit` 不是 CAS 对象（ADR §4 末段）：确实只是一行表记录，role 闭集未动。
- 钉住：`init_creates_the_default_branch_and_one_commit`、`a_store_without_workspace_tables_reports_not_initialised`、
  `every_workspace_reserves_a_serial_segment`（后者的"schema 版本不符"路径由 `main_db` 单测以外的方式覆盖：
  见"未验证"一节）。

## §5 Overlay 文件（fs-overlay 的接缝改动）

本卡改了 P2a-4 已合入的三处，都是卡里点名的接缝：

| 改动 | 理由 |
| --- | --- |
| `OverlayOptions` 增加 `scope_id: [u8; 32]`，创建时写入 `ov_meta.scope_id`，重开时校验 | P2a-4 自己随机生成 scope，会让同一谱系的两个 workspace 用不同 scope，物化出的 root 必然不同；ADR §6 要求 scope 由主库 `scope_allocator` 拥有 |
| `serial_end` 在重开时按"取更大值"扩展 | workspace 每次打开向主库预留一段（ADR §6），重开必须能延续 |
| `initialize` 在 `base_root` 与参数不符时，**仅当** `pending_root` 等于参数才放行，并且内存中的 base 以 `ov_meta` 为准 | 这是 §9.3 第二行的可恢复状态；否则 `open` 会在恢复之前就报 `SchemaMismatch`，两阶段协议无法完成 |

新增（`fs-overlay/src/publish.rs`，约 190 行）：`read_tree`（物化器的读作用域，TX3）、
`workspace_id`/`set_workspace_id`、`pending_root`/`set_pending_root`（`FULL` 提交）、
`adopt_base`（§9.1 第 4 步，单事务清三表 + 换 base）、`serial_range`/`extend_serials`、`is_dirty`。

## §6 FileId 与 scope

- `scope_allocator` 每行 `(scope_id, next_serial)`；`init` 新建 scope（`next_serial=1`，0 是根目录）。
- workspace 预留段：创建时 `reserve_segment`（1024），**每次打开**再预留一段并扩展
  `ov_meta.serial_end`。预留是主库短事务，正是 §11.1 有界等待存在的理由。
- **低水位补段**：`WorkspaceStore::write` 在开启 overlay 事务**之前**检查剩余 serial，
  少于 `SERIAL_LOW_WATER`（256）时向主库再预留一段。这是本实现相对 ADR 的一处工程选择：
  ADR 只说"用尽时预留"，而"用尽"没有可挂钩的位置（分配发生在事务内部），所以在事务外提前补。
  代价与 ADR §6 的结论一致——浪费的段无害，serial 不复用。
- 被拒绝的操作也消耗 serial（分配在规则判定之前）⇒ 与 ADR 一致。
- 钉住：`every_workspace_reserves_a_serial_segment`（每次创建推进 1024）、
  `a_rejected_rule_leaves_the_view_unchanged`。

## §7.1 TreeStore

`OverlayStore` 已实现 trait；本卡新增 `WorkspaceStore`（`tree.rs`）仅做两件事：把
`Arc<OverlayStore>` 交给按值取 store 的 `Rules`，以及在 `write` 前补 serial 段。
规则（R1–R12）**一行未改**。这也验证了 P2a-3 的抽象是对的。

## §9.1 snapshot / commit 协议

实现在 `publish.rs`，四步与 ADR 逐条对应：

| 步 | 代码 | 说明 |
| --- | --- | --- |
| 0 | `materialise` 开头 `overlay.flush()` | 要物化的内容先落盘 |
| 1 | `read_tree(collect)` → `encode` | 对象先发布，尚无任何行引用（I5） |
| 2 | `set_pending_root(Some(R))` | `FULL` 提交，意图本身必须持久 |
| 3 | `record_snapshot` / `record_commit`，单个 `BEGIN` 事务 | **唯一发布点** |
| 4 | `adopt_base(R)` | 清三表 + 换 base，`FULL` |

- `commit` 在物化后先比 `R == root(base_commit)`，相等返回 `NothingToCommit`，**不执行 2–4 步**
  （overlay 与主库都不变；新对象若有则成孤儿）——与 §9.1 第 1 步括号里的要求一致。
- `record_commit` 在同一事务里读 `branch.head_commit`、插 commit、移 head、写 `branch_log`、
  更新 `workspace.base_commit/base_root`；`CHECK` 失败则整事务回滚（§10.3 表格第 1 行）。
- 钉住：`commit_moves_the_branch_and_grows_history`、`nothing_to_commit_when_the_view_equals_the_base`、
  `two_snapshots_without_change_produce_the_same_root`、`a_crash_after_the_publication_completes_it_on_reopen`。

## §9.2 物化器

- **从根可达遍历**（I2）：`collect` 从 serial 0 开始按目录列举向下走，从不查表决定"有哪些文件"；
  inode 表按 serial 排序后编码，所以键集 = 可达集（ADR-005 V4 按构造成立）。
  可达 serial 无 inode ⇒ `Inconsistent` 报错，而不是静默丢子树。
- **两趟**：第一趟在**一个**读作用域内记录结构与子项；第二趟读内容并发对象。分开的原因是
  overlay 的内容 API 自己会开作用域，在遍历作用域里调用就是 TX4 重入。
- 内容按 **64 KiB 块**逐个读取（`read_block`），峰值内存是一块而非一个文件。
- **未优化（如实标注）**：未变目录**重新编码**而不是复用 base 的目录树 oid；未变块**重新读取**
  而不是复用 base extent。内容寻址让结果相同、也不多写对象（`AlreadyPresent`），但遍历成本照付。
  ADR §9.2 把这两条写成优化目标，本卡按卡的预算条款标为"已知未优化"，不宣称已实现。
- 文件形态严格按 ADR-005 §3.2 归一：`size==0` 或全零 → `Empty`；`size < block` → `Blob`；
  否则 `Extents` 且全零块省略。
- 钉住：`two_snapshots_without_change_produce_the_same_root`（同一视图两次物化同 root）、
  `a_committed_tree_is_readable_after_the_workspace_is_gone`（编码结果可被 `SnapshotReader` 读）。
  回归：这一条最初暴露了 `Empty` 目录/文件被当成非法快照的问题（P2a-4 期间已修）。

## §9.3 恢复

| pending_root | 主库 base_root | 本实现动作 | 测试 |
| --- | --- | --- | --- |
| NULL | — | 无 | `workspace_lifecycle_matrix` |
| R | == R | `adopt_base(R)`（补第 4 步） | `a_crash_after_the_publication_completes_it_on_reopen` |
| R | ≠ R | `set_pending_root(None)`，overlay 原样 | `a_crash_after_the_intent_clears_it_and_keeps_the_work` |

- `state = creating`：删除该 workspace 目录、重建空 overlay、置 `ready`
  （`a_workspace_stuck_in_creating_is_rebuilt_when_opened`）。
- `Repo::open` 清扫 `workspaces/` 下无主库行的目录（discard 中崩溃）
  （`an_orphan_workspace_directory_is_swept_on_open`、`a_crash_during_discard_is_swept_on_reopen`）。
- **实现细节需要留意**：恢复时 `OverlayStore::open` 先按 `ov_meta.base_root` 建立内存 base，
  再由 `adopt_base` 推到主库的 row 值。这样"文件始终自洽"，主库那一行是唯一可能超前的一方，
  而 `adopt_base` 是闭合它的唯一动作。ADR 没规定这个顺序，但它是让 `open` 不出现
  "内存 base 与文件不符"窗口所必需的。

## §9.4 崩溃注入点

用**真实进程 abort**（无常规展开、无 `Drop`、无 SQLite close、无 checkpoint），子进程由
`tests/crash.rs` 以 `--exact crash_child --ignored` 派生。环境变量 `WCAS_WORKSPACE_CRASH_AT`
命名注入点，未设置时函数只做一次环境变量查询。

| ADR §9.4 注入点 | 覆盖 | 测试 |
| --- | --- | --- |
| 物化中途（任意 put 之后） | ✅ `during-materialise` | `a_crash_during_materialisation_publishes_nothing` |
| 第 2 步之前 | ✅ `after-materialise` | `a_crash_before_the_intent_leaves_the_branch_and_the_work` |
| 第 2 步之后、第 3 步之前 | ✅ `after-intent` | `a_crash_after_the_intent_clears_it_and_keeps_the_work` |
| 第 3 步之后、第 4 步之前 | ✅ `after-publish` | `a_crash_after_the_publication_completes_it_on_reopen`、`..._snapshot_...` |
| discard 删行后删文件前 | ✅ `after-discard-row` | `a_crash_during_discard_is_swept_on_reopen` |
| overlay 操作事务前后 | ⚠️ 未单独注入 | P2a-4 的 I4 已覆盖 overlay 自身事务（`fs-overlay/tests/crash.rs`）；本卡的注入点都在其外层 |
| flush 前后 | ⚠️ 未单独注入 | 同上；`flush` 的持久性由 P2a-4 的 I4 钉住 |

**这些测试证明的是恢复协议**（重开后必落在 §9.3 的某一行，且视图与参考模型逐字节一致），
**不是** fsync / 掉电语义：被 abort 的进程仍留下操作系统的页缓存（ADR §8、§17.3）。

## §10.1 commit id

`hex(BLAKE3("wcas/commit/v1\0" || root || parent_or_32_zero || created_ns(BE i64) || len(BE u32) || message))`，
与 ADR 逐字节对应；含时间，所以同树同父两次提交 id 不同。

## §10.2 操作

| 操作 | 状态 | 测试 |
| --- | --- | --- |
| `init` | ✅ 建 scope、空根、首个提交、`main` | `init_creates_the_default_branch_and_one_commit` |
| `branch create <b> [--from]` | ✅ 名字冲突 `AlreadyExists`，继承 scope | `lifecycle_failures_are_typed`、`rebind_...` |
| `commit` | ✅ 期望值检查 | `commit_moves_the_branch_and_grows_history` |
| `commit --force` | ✅ parent = `base_commit`，`branch_log` 记 `commit-force` + displaced | `branch_moved_is_typed_and_force_records_the_displaced_head` |
| `reset` | ✅ 只移指针，旧 head 进 `branch_log`，`--expect` 不符则 `BranchMoved` | `reset_moves_and_revert_grows` |
| `revert` | ✅ 新提交，root = head 父提交的 root（**只撤销 head**） | 同上 |
| `restore <ws> <commit> [--discard]` | ✅ `base_commit` 不变；dirty 且无 `--discard` → `WorkspaceDirty` | `restore_changes_the_view_but_not_the_base_commit` |
| `log <b>` | ✅ 沿 parent 链 | 多个测试 |
| `workspace rebind` | ✅ 只允许 head == `base_commit` | `rebind_lets_the_loser_publish_on_another_branch` |

`revert` 的 message 固定为 `"revert"`（ADR 未规定文本）。

## §10.3 BranchMoved

判定在**第 3 步事务内**：`branch.head_commit != workspace.base_commit` → `BranchMoved{branch,
expected, actual}`，事务回滚。失败后逐项对照 §10.3 表格：

| 对象 | 本实现 | 断言位置 |
| --- | --- | --- |
| branch / commit / branch_log | 事务回滚，不变 | `branch_moved_...`（head 仍是赢家的） |
| overlay | 不变；第 2 步的 `pending_root` 在同一调用内清回 NULL | 同一测试（`is_dirty()` 仍真、内容完整） |
| workspace 行 | 不变（`base_commit` 仍是旧 head） | 同一测试（`base_commit() == base`） |
| 已物化对象 | 留作孤儿；再次物化时 `AlreadyPresent` 复用 | 未单独断言（见"未验证"） |

三条出路都提供：`workspace rebind`（✅ 测试）、`restore` 重做（✅ `restore` 测试 + 人工重做）、
`commit --force`（✅ 测试）。

## §11.1 Busy 有界等待

- 主库连接（`MainDb`）`busy_timeout = 5000`，且 5000 写进 `workspace_policy.main_busy_timeout_ms`
  并在打开时读回校验。
- 超时后仍返回**类型化** `Busy{waited_ms}`（`WorkspaceError::from(rusqlite::Error)` 把
  `DatabaseBusy`/`DatabaseLocked` 归纳成它），不重试、不退避、不翻译成成功。
- overlay 连接保持 `busy_timeout = 0`（独占，W2）。
- 钉住：`a_held_main_database_lock_ends_in_a_typed_busy`——第二个连接持有主库写锁，
  `branch_create` 等待约 5 s 后返回 `Busy`，且**锁释放后同一操作成功**（失败未留残迹）。

**未做（与 ADR 有偏离，已提请 manager 裁决）**：§11.1 要求**两条**主库连接都是 5000 ms，
其中一条是 cas-store 的连接；ADR-002 §8 的既有测试 `a_cross_process_writer_reports_busy_without_retrying`
**断言**该连接 `busy_timeout = 0`。这是两个 ADR 的直接冲突，且影响 §11.1 自己列出的测试
（"两个 workspace 并发 snapshot 各 1000 文件，均成功"）——本卡不动 cas-store，记为 KI-7。

## §12 批量发布

未实现（`PageSink` 目前直接写 store）。§12 明确属于 P2a-5，卡里列为"不做"。
物化器只依赖 `PageSink`，接批量发布时不需要改本 crate。

## §14 版本与退场

- 主库 `schema_version = 3`；发现 P1 的 `namespace_policy`（schema 1/2）→ 表不存在于本 crate 的
  DDL 校验路径，实际表现为 `workspace_policy` 缺失 → `NotInitialised`。ADR 期望的是
  `SchemaMismatch` 提示重建；本实现的错误类型是 `NotInitialised`（可读、可操作，但不是 ADR 字面）。
- overlay `schema_version = 1`，按 overlay 自校验（P2a-4 已有）。
- P2a-7 的删除项（fs-api 直写路径、表后端、D+ root 链）未动。

## §15 留给 P2b 的 GC 根

本卡记录的 GC 根：`branch` 的 head 沿 parent 链、`branch_log` 全部行（含 displaced）、
`snapshot` 表、每个 workspace 的 `base_commit`/`base_root`、`ov_meta.pending_root`。
**不删除任何东西**。`snapshot` 表只增不减（无保留策略），与 §17.4 一致。

## §18 测试清单（P2a-6 部分）

| §18 项 | 状态 |
| --- | --- |
| 1 参考模型随机序列 | ❌ 未做（P2a-8） |
| 2 K1–K6 trait 契约 | P2a-4 已在两个后端跑；本卡新增 `a_rejected_rule_leaves_the_view_unchanged` |
| 3 10 个 workspace 同 base 并发 | ❌ 未做（P2a-8；P2a-4 的 `ten_workspaces_over_one_base_stay_isolated` 覆盖隔离，不覆盖并发） |
| 4 W4 崩溃合约 §9.4 逐点 | ✅ 见 §9.4 表（overlay 事务前后/flush 前后沿用 P2a-4） |
| 5 历史可读、零表依赖 | ✅ `a_committed_tree_is_readable_after_the_workspace_is_gone` |
| 6 F1 | ✅ `branch_moved_is_typed_and_force_records_the_displaced_head` |
| 7 reset / revert / restore 历史形态 | ✅ `reset_moves_and_revert_grows`、`restore_...` |
| 8 NothingToCommit | ✅ |
| 9 workspace 独占 | ✅ `workspace_lifecycle_matrix`（同进程第二个句柄） |
| 10 Busy | ✅ 有界等待 + 超时路径；"两 workspace 并发 snapshot 均成功"受 KI-7 阻断 |
| 11 性能 | ❌ 未做（P2a-8） |

---

## 汇总

**逐条一致**：§4（表 + policy）、§6（scope 与预留）、§9.1/§9.2/§9.3、§10.1、§10.2、§10.3、§15。

**有意的偏离（3 处，理由见上）**：

1. §9.2 的两条复用优化**未实现**（未变目录 oid、未变块 extent），按卡的预算条款标注"已知未优化"；
2. §11.1 只对 fs-workspace 的一条连接生效，cas-store 那条仍是 0 ms——与 ADR-002 §8 的既有测试冲突，
   记为 KI-7 待裁决；
3. §14 的 P1 schema 检测报 `NotInitialised` 而非 `SchemaMismatch`。

**未验证**：§10.3 的"已物化对象留作孤儿并被 `AlreadyPresent` 复用"没有单独断言；
§18 的参考模型随机序列、10-workspace 并发、性能数字属 P2a-8。

**本卡顺带修复的既有缺陷**：`fs-overlay::truncate` 在非页对齐尺寸下会删掉承载存活字节的页
（数据丢失，5 字节文件截断后读回全零）。已修 + 回归测试
`k6_truncate_inside_a_page_keeps_the_surviving_bytes`，详见 KI-6。
