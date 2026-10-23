# P2a 验收矩阵（A1–A8）与 DoD 判定

依据 `docs/process/p2a8-card.md`、ADR-006 §18、spec §6 P2。每个 A 行给出**测试名**或**实测证据**；
三个在验收过程中发现的缺陷见 `docs/reports/p2a-baseline.md` §8。

---

## 一、验收矩阵

### A1 — 10 个 workspace 共用同一 base 并发乱序写，互不影响 ✅

**测试**：`crates/fs-api/tests/acceptance.rs::a1_ten_workspaces_do_not_interfere`
（`--ignored`，重场景，约 14 s）

- 10 个 branch 从同一 base commit 分出，10 个 workspace 各建 200 个文件；
- `Barrier` 同时释放后按 `(step*7 + index*13) % 200` 的**不同顺序**写，内容每 workspace 不同；
- 断言：每个 branch 都前进到自己的 commit；`read_view` 逐文件核对**每个 workspace 只有自己的字节**；
  10 个 workspace 都注册在册。

**结果**：通过（修复 §8.3 的 `BEGIN DEFERRED` 缺陷后）；连续 3 次运行全通过。

### A2 — flush 后重启数据仍在 ✅

**测试**：`acceptance.rs::a2_a_flushed_change_survives_a_reopen`

写 → `flush` → `close` → 丢弃 Repo → 重新 `Repo::open` → 目录、文件内容、dirty 状态全部核对。
与 P2a-4 的 I4 进程级崩溃测试互补：那条证明"被杀之后的前缀"，这条证明"正常重启后的完整性"。

### A3 — 历史 `read_view` 零表依赖 ✅

**测试**：`acceptance.rs::a3_history_is_readable_without_any_row`

提交两版内容后 `workspace_discard`（删主库行 + 删 overlay 文件 + 删目录），断言
`workspaces()` 为空，随后 `read_view(第一版)` 仍能读回**第一版**内容、列目录、`stat`。
另在 `workspace_api.rs` 有一份同类断言。

### A4 — BranchMoved → 类型化 → 原样保留 → rebind / --force ✅

**测试**：`acceptance.rs::a4_a_moved_branch_is_refused_and_can_be_rebound`

- 两个 workspace 同 branch 同 base，前者提交成功，后者得到
  `BranchMoved { branch, expected: base, actual: 前者 }`；
- 失败后：branch head 未变、后者 base_commit 未变、它的内容仍在；
- **出路 1**：`branch_create("side", from = base)` + `rebind` + 提交成功；
- **出路 3**：branch 被 `reset` 到旧提交后，一个 base 在新 head 上的 workspace 提交得到冲突，
  `--force` 覆盖成功，且新提交 parent = workspace 的 base（不是被取代的 head）。

### A5 — 两个 workspace 并发 snapshot 各 1000 文件均成功 ✅

**测试**：`acceptance.rs::a5_two_publications_share_the_main_database`
（常规回归内，约 3 s）

两个线程各持 1 000 个文件，`Barrier` 同时发布，两者都成功且 commit 不同；
测试打印实测耗时作为 §11.1 的余量证据（本机约 0.3–0.8 s，对 5 s 上限有一个数量级余量）。

### A6 — 同一 workspace 内可线性化 ✅

**测试**：`acceptance.rs::a6_operations_in_one_workspace_are_linearizable`

rename 之后旧名字立即消失、新名字内容正确；被拒绝的操作（mkdir 同名）不改变视图
（`read_dir` 前后一致）。K1–K6 的完整契约在 `fs-overlay/tests/contract.rs`（16 条）。

### A7 — 物化器从根可达（I2）✅

**测试**：`acceptance.rs::a7_an_unreachable_inode_never_reaches_a_snapshot`

建 `/kept/f` 与 `/dropped/g`，删掉 `/dropped` 整棵子树后提交；断言快照里只有 `kept`、
`/dropped` 查不到，且**深校验（`verify(true)`）的 inode 计数 = 4**（genesis 根 + 3 个可达 inode）。
被删除 inode 的 overlay 行仍在，但它不参与物化——这正是 I2 的"不扫表"含义。

### A8 — schema v3 拒绝 v1/v2（含 `create=true`）✅

**测试**：`acceptance.rs::a8_an_older_schema_is_refused_even_with_create`
（另见 `verify.rs::a_previous_schema_is_named_and_refused`）

构造"真实 cas-store + P1 的 `namespace_policy`"的库：`Repo::open` 返回
`SchemaMismatch { found: "schema 1/2 (namespace_policy); rebuild the store" }`，
`Repo::create` **同样拒绝**（不会在旧库上写第二套 schema）。

---

## 二、DoD 判定

| # | 完成定义 | 判定 | 证据 |
| --- | --- | --- | --- |
| D1 | 多 Workspace 并发可用 | **满足** | A1（10 路并发发布）、A5（两路 1000 文件）、A6（工作区内线性化）；`fs-overlay` 的 `ten_workspaces_over_one_base_stay_isolated` 与 K3 并发用例 |
| D2 | flush 语义明确 | **满足** | A2 + P2a-4 的 I4（`fs-overlay/tests/crash.rs`）+ P2a-6 的六个进程级崩溃注入点（`fs-workspace/tests/crash.rs`）；报告 §3 给出 flush 成本与脏页数量无关的实测 |
| D3 | 历史零表依赖可读 | **满足** | A3（discard 后仍可读）+ `workspace_api.rs` 同类断言；`read_view` 路径只依赖 store + 快照根 |
| D4 | Branch/Commit CAS 语义完整 | **满足** | A4（冲突 / rebind / force / displaced 记录）+ `branch_moved_is_typed_and_force_records_the_displaced_head` + reset/revert/restore 各自的历史形态测试 |
| D5 | 直写路径已退役 | **满足** | `docs/reports/p2a7-retirement.md`：`Fs`、表后端、D+ root 链全部删除；`fs-namespace` 无 SQLite 依赖；165 个测试的逐文件删除说明 |
| D6 | 性能报告可复现（R1–R5） | **满足** | `docs/reports/p2a-baseline.md`：每个场景 3 次原始 run、载荷特征、盘与目录、复现命令；工具是仓库内的 `cargo run --example baseline` |

### 不满足 / 留给后续的项（如实标注）

| 项 | 状态 | 去向 |
| --- | --- | --- |
| §18-1 参考模型随机序列（内存模型对照随机操作序列） | **未做** | 未在 P2a-8 卡范围内；建议 P2b 前的加固卡，或 QA 交叉验证时补 |
| 增量物化（未变目录复用 base oid、未变块复用 extent） | **未做**（KI-8） | 报告 §4.1 量化了代价：同一棵树第二次快照仍需 2.3 s 全量遍历，但只写 5 446 字节 |
| 并发 snapshot 的"最长单次锁等待"精确度量 | **未做**，只有上界 | A5 记录整次发布耗时；§8.3 证明等待路径现在真的走 busy handler。要精确到"单次锁等待"需要在发布事务内埋点，属 P2b 可观测性 |
| 批量发布（§12）、GC/CDC/pack | **未做** | P2b（KI-7 处置里记了 fsync 持锁的纪律） |
| WinFsp / 原生 FS 对照 | **未做** | P3 |
| KI-2b（高盘负载下 S2 创建预算偶发失败） | **仍存在** | 与 P1 报告一致，属环境限制 |

---

## 三、本卡修复的三个缺陷

三条都是 P2a-6 的缺陷，都在 P2a-8 的"量级/并发/边界"场景下暴露（详见 baseline 报告 §8）：

| 缺陷 | 触发场景 | 修复 | 回归测试 |
| --- | --- | --- | --- |
| 一个 workspace 只能新建 1 024 个文件 | flush 场景 N=10 000 | 内容操作经 `fs_workspace::Workspace` 统一补 serial 段 | `a_workspace_can_create_more_files_than_one_serial_segment` |
| 全零小文件被编码成 `Empty`（快照非法） | 任一 `size < 64 KiB` 且全零的文件 | 按 ADR-005 §3.2 改为 Blob | 由 `verify(true)` 与 snapshot 场景覆盖；`fs-snapshot` 的解码器本身在拒绝 |
| 十路并发发布 2/10 提前 `Busy` | A1 | 发布事务改 `BEGIN IMMEDIATE` | `a_publication_waits_for_the_write_lane_before_reading_the_head` |

另外顺手修了 `fs-snapshot` 的 `lookup`：**名字排在目录分支首键之前**时，叶子层返回
`Ok(None)`，分支层却报"malformed"。同一个查询在两个层级给出不同答案，导致"查一个不存在的
名字"被当成快照损坏。现在两层都返回 `Ok(None)`。

---

## 四、四件套

| 项 | 结果 |
| --- | --- |
| build | 0 warning / 0 error |
| clippy | 0 |
| fmt | clean |
| doc | 0 |
| test | **230 passed / 0 failed / 3 ignored** |
| 红线 | 仅 `crates/cas-store/{durability.rs, object.rs}`（本卡未改 fsync 路径） |

`--ignored` 的 3 条：`fs-overlay` 的崩溃子进程、`fs-workspace` 的崩溃子进程、`fs-api` 的 A1。
