# P2a-4：ADR-006 一致性核对

- 日期：2026-10-23
- 分支：`p2a/overlay`
- 用途：卡面要求"ADR-006 的 overlay 相关节（§2.2/§2.5/§5.3/§7.4）与实现一致，不一致处如实记录"
- 方法：逐节读条文 → 指出实现位置 → 标注「一致 / 有差异 / 未实现」

---

## §2.5 overlay 库（WAL + NORMAL，不 fsync）

| 条文 | 实现 | 结论 |
| --- | --- | --- |
| 每个变更操作一个不 fsync 的 overlay 事务 | `store_impl.rs` 的 `TreeStore::write`：每个 body 一个 `transaction()`，pragma `synchronous=NORMAL` | 一致 |
| 无内存层，读路径只有 overlay 库 + base 两层 | `view.rs`：先 `ov_*` 表、miss 则 base，没有第三层 | 一致 |
| flush = 一次 FULL 提交 | `store_impl.rs` 的 `flush()`：`synchronous=FULL` → 写 `flush_seq` 的一次真实提交 → `wal_checkpoint(FULL)` → 恢复 `NORMAL` | 一致 |

**一处实现细节值得说明**：`flush` 为什么要写一个 `flush_seq`？因为 `BEGIN;COMMIT` 若没有任何写入，SQLite 不会产生新的 WAL 帧，也就不会把此前已提交的帧推到盘上。ADR 只写"一次 FULL 提交"，没说提交什么；实现必须制造一次真实提交才能兑现语义。

## §2.2 overlay schema

| 条文 | 实现 | 结论 |
| --- | --- | --- |
| `ov_meta(key, value)`，键含 schema_version / ddl / workspace_id / base_root / scope_id / serial_next / serial_end / pending_root | `schema.rs` DDL + `store.rs::initialize` 写入全部 8 个键 | 一致（`pending_root` 与 `workspace_id` 由 P2a-6 使用，现在写空串） |
| `ov_inode` 含 `origin`(0=base/1=新建)、`live`(0=墓碑)、`kind`、`size`、`created_ns`、`modified_ns`、`base_limit`、`entries` | 同 | 一致 |
| `ov_entry(parent, name, child)`，`child IS NULL` = whiteout | `write_view.rs::unbind_impl`：只存在于 base 的名字被删时写 NULL 行 | 一致 |
| `ov_page(serial, page_index, data)`，短于页长读作零 | `content.rs::page_bytes` + `read_at_impl` 的零填充 | 一致 |

**差异 1（`entries` 的维护时机）**：ADR 写"目录：视图中的子项数（rmdir 判空 O(1)）"。实现里这个计数在**第一次触碰该目录时**才建立（此时遍历一次 base 目录），此后增量维护。
- 影响：`rmdir`/R6 在**已触碰**的目录上是 O(1)，与 ADR 一致；
- 代价：首次触碰是 O(base 目录大小)。10 万条目的 base 目录会付这一次。
- 原因：新建 workspace 时不可能预先知道 base 里每个目录的条目数，除非全量遍历——那与"不复制 base"冲突。

**差异 2（`serial_end` 的来源）**：ADR §6 说 serial 段由主库 `scope_allocator` 按 1024 预留；实现里 `serial_first`/`serial_end` 是 `OverlayOptions` 的**入参**，用尽时返回 `InvalidRecord` 错误。
- 原因：`scope_allocator` 属于 fs-workspace（P2a-6），本卡不接 workspace 生命周期。
- 影响：段用尽不会自动续借，会报错而不是静默与 base 冲突。P2a-6 接管时把入参换成真实的预留即可。

## §5.3 overlay 内容表示（页 + base_limit，不是 extent）

| 条文 | 实现 | 结论 |
| --- | --- | --- |
| 4 KiB 页；`read_at` 对每页：有 `ov_page` 用页，否则位置 < `base_limit` 读 base，否则零 | `content.rs::read_at_impl` | 一致 |
| `write_at`：页不存在时拷入当前视图字节（首尾两页读 base）→ 覆盖 → 存页 | `write_at_impl`：每个被触及的页先materialise 当前视图再覆盖 | 一致 |
| 首次修改 base inode 时插入 `ov_inode(origin=0, base_limit=原 size)` | `content.rs::touch_inode` | 一致 |
| `truncate`：删 `page_index ≥ ⌈new/page⌉`、截尾、`base_limit = min(base_limit, new)` | `truncate_impl` | 一致 |
| K6：截到页边界不产生多余页 | 同上：`boundary = new/page`，`page_index ≥ boundary` 全删；对齐时边界页正好从 new 开始，因此一并删除 | 一致（有 `k6_truncate_to_a_page_boundary_creates_no_page`） |

**补充（ADR 未写但实现必须做）**：`read_at` 必须处理 base 文件的**三种形态**（`Empty` / `Blob` / `Extents`）。ADR-005 §3.2 规定小于一块的文件是单个 Blob，只处理 extent 会让所有小文件读不出来。初版实现漏了，被 `a_base_file_reads_through_the_overlay` 抓到。

## §7.4 同一 workspace 内的并发语义

| 条文 | 实现 | 结论 |
| --- | --- | --- |
| 一个进程对一个 workspace 只能打开一个句柄，第二个得到 `WorkspaceInUse` | `store.rs`：`locking_mode=EXCLUSIVE`；第二个 opener 在 pragma 阶段失败 | 一致（有 `a_second_handle_is_refused`） |
| `Workspace: Send + Sync`，多线程用 `Arc` 共享 | `OverlayStore` 用 `Mutex<Connection>` + `Mutex<Store>`；`the_handle_is_send_and_sync` 断言 | 一致 |
| 句柄互斥锁在操作开始获取、事务提交后释放 | 每个作用域 `let _lock = self.lock()` | 一致 |
| 每个操作整体提交或整体没发生 | 一个操作一个 overlay 事务 | 一致 |
| ①不同区间并发写：两者都保留，size 取最大 | `k3_concurrent_writes_to_different_pages_keep_both` | 一致 |
| ③`write_at` 与 `truncate` 并发：结果等于两种串行顺序之一 | 互斥锁使二者串行；顺序不确定但语义等价 | 一致（未单独写测试，K6 覆盖了 truncate 的形态） |
| ④`write_at` 与 `unlink` 并发：unlink 在先时 `write_at` 得 `NotFound`，不留页 | `k3c_a_write_that_loses_to_unlink_leaves_nothing` | 一致 |
| ⑤交叉目录 rename 并发：至多一个成功 | `k4_a_directory_cannot_move_into_its_own_subtree` 覆盖单线程判定；并发版属 K3d，未单独在 overlay 上跑 | **部分**：互斥锁使两个 rename 串行，第二个必然看到第一个的结果并被 R9 拒；但没有写并发测试来证明 |

**未实现 / 未验证**：§7.4 第②条（重叠区间并发写不出现字节交错）没有单独测试。互斥锁使两个写完全串行，交错在结构上不可能，但"结构上不可能"不是实测。

## §8 flush 合约

| 条文 | 实现 | 结论 |
| --- | --- | --- |
| 任一变更返回：对本 workspace 可见，不保证持久 | 每操作一个事务，`NORMAL` | 一致 |
| `flush()` 返回：此前操作持久 | `flush()` | 一致（进程崩溃下无法区分，见下） |
| `flush(path)` 等价于 `flush()` | **未提供** `flush(path)` 重载 | **差异**：只提供 `flush()`。workspace 级语义使按路径的版本没有意义，ADR 自己也说二者等价；调用方拿不到按路径的入口是缺口，不是语义分歧 |
| 崩溃后状态 = 前缀 ⊇ 最后 flush | `i4_a_kill_after_flush_keeps_everything_flushed`、`i4_a_kill_without_flush_reopens_to_a_prefix` | 一致（仅进程崩溃） |

**必须说清的一点**：进程被 kill 时，SQLite WAL 里已提交的事务**通常一个都不丢**，ADR 自己也这么写。所以"kill 后 flush 的数据还在"这个测试**不能证明 fsync 生效**——它能证明的是：恢复可用、状态是前缀、没有半应用。掉电语义按 ADR 是"依据 SQLite 文档，未经本项目测试"。我没有把这条测试包装成比它更强的东西。

## §9.4 崩溃注入点

| 注入点 | 覆盖 |
| --- | --- |
| overlay 操作事务前后 | `i4_a_kill_without_flush_reopens_to_a_prefix`（在事务之后 kill） |
| flush 前后 | 两个 I4 测试 |
| 物化中途 / §2 步前後 / §3 步前後 / §4 步前後 / discard | **未覆盖**：这些属于 snapshot/commit 协议，P2a-6 |

## §5.1 pragma

`journal_mode=WAL` / `synchronous=NORMAL` / `locking_mode=EXCLUSIVE` / `busy_timeout=0` / `foreign_keys=OFF`，打开后**读回校验** `journal_mode`（ADR 写"读回校验，不符即拒"）。其余 pragma 未逐项读回——`synchronous` 与 `locking_mode` 可读回，但 SQLite 对它们的返回值语义在连接级别，本次只校验了最关键的 journal_mode。**这是一处偏弱的实现**。

---

## 汇总

| 类别 | 数量 |
| --- | --- |
| 完全一致 | §2.5（3/3）、§2.2（4/4 结构）、§5.3（5/5）、§7.4（6/9 明列条目） |
| 有差异且已记录 | `entries` 维护时机、`serial_end` 来源、缺 `flush(path)`、pragma 只校验一项 |
| 未实现（属 P2a-6） | §9 全部、§9.4 大部分 |
| 未验证 | §7.4 第②条、§7.4 第⑤条的并发形态 |
