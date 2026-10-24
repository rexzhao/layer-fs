# 已知问题（T5 期间发现，按约束只记录不修复）

本文件只收录**已确认、有复现、不修**的问题及其理由。每条的复现都由在树测试钉住，
所以"修好了"必须同步改测试，不能悄悄漂移。

---

## KI-1：truncate 到精确块边界会多发一个零长度对象（**P2-0a 已修复**）

**发现于**：T5，收尾 QA 覆盖率建议 C4 时。

**复现**：写一个三块文件，然后 `truncate` 到恰好一块：

```rust
fs.write_at("/f", 0, &content)?;           // 3 个不同块 → 3 个对象
fs.truncate("/f", BLOCK_BYTES as u64)?;    // 期望：不发新对象
```

**实际**：Store 多出一个**零长度**的 `Bytes` 对象。

**成因**：`Fs::truncate` 用 `size / BLOCK_BYTES` 定位边界块。当 `size` 恰好是块大小的整数倍
时，该商指向一个不存在的块（文件正好在边界结束），于是 `block_len == 0`，但分支仍调用了
`store.put(Role::Bytes, &[])`，并插入一条零长度 extent 行、删除该位置及之后的行。

**影响**：只浪费空间，不丢数据（文件读回正确）。内容寻址把这类空对象折叠成同一个 id，所以
无论调用多少次，Store 里最多累积一个——这也是它一直没被发现的原因：直到有测试在调用前后
数对象。

**为什么不在 T5 修**：T5 卡的约束是"性能数字只记录不优化"。manager 已将其预登记为 **P2 首批
候选修复项**。

**修复方向**（任选，都在 `Fs::truncate` 的边界块分支，几行）：

- `block_len == 0` 时跳过发布，只从 `boundary_start` 起删行；
- 或在进入边界块分支之前把"整数倍"单列出来处理。

**钉住它的测试**：`c4_truncating_to_a_block_boundary_keeps_surviving_bytes` 断言当前行为
（对象数 +1），并在注释里指向本条。修好实现时**必须**同步更新该断言。

---

## KI-2：每次操作一次 fsync，小文件吞吐由提交延迟主导

**发现于**：T5，S2 验收场景。

**现象**（F: 盘，release，每项 10 次重复取 P50）：

| 磁盘 | `create` | `write_at` | 一对（create+write） |
| --- | --- | --- | --- |
| F: | ~0.5 ms | ~0.5 ms | **0.93 ms**（P95 1.03） |
| C: | — | — | **2.76 ms**（P95 2.86） |

写 10 个字节要 0.5–3 毫秒。10 万文件 = 20 万次强制落盘，因此在小文件场景里，吞吐由
**提交次数 × fsync 延迟**决定，而不是由 crate 内的算法决定——对象发布与索引写入在这个成本
面前可忽略。

**这不是缺陷**：`synchronous=FULL` 是 ADR-002 §9 与 ADR-003 §7 为崩溃安全做的**刻意**选择。
记录它是为了让后续读者知道这个数字的成因，而不是把"10 万文件 90 秒/900 秒"误读成实现问题。

**后续方向**（P2 级，需单独立卡，不在 T5 范围）：

- **批量/组提交**：一次事务创建 N 个文件，把 N 次 fsync 变成 1 次。代价是改变"每操作一事务"
  的语义，需重新论证崩溃一致性。
- **分层持久化**：元数据与对象采用不同的 `synchronous` 级别。代价是崩溃后的一致性格局变复杂
  （可能提交了引用但没提交对象，或反之）。
- 先例：LayerFS 的"短持有发布道"（short-held publication lane）把多个小写操作的发布合并到一次
  提交里，同时不长时间持锁。

**测量它的方式**：`cargo run -p fs-api --release --example probe_tx -- <root>` 直接给出每操作
延迟与 10 万文件的预测耗时。

---

## KI-2b：S2 的创建预算在高盘负载下可能偶发失败

**性质**：测试预算的余量问题，不是代码问题。QA 交叉验证时实测两次：681.65 s（超预算）与
551.5 s（达标）——同一条代码，同一台机器。

**现象**：10 万文件的完整验收在不同运行时落在 **546–682 s** 区间，而预算声明为 600 s，
余量只有 8–13%。参考盘的提交延迟方差约 5 倍（KI-2），这个方差会直接体现为总耗时波动。

**判定规则**：该断言失败时，**先确认机器是否忙碌**（并发构建、索引器、拷贝等），重跑一次
再判断是否为回归。**不要**因单次失败就当成性能退化——§2.2 的分段数据显示成本不随文件数单调
增长，波动来自磁盘而非代码。

**为什么保留紧预算而不是放宽**：如果放到 900 s，一个真实的 1.5 倍退化就会被吞掉。保留 600 s
意味着它的失败是有信息量的，代价是需要人工区分"磁盘忙"与"真退化"——这个区分成本是可接受的，
因为该断言只在完整验收时跑，不在常规回归里。

**已在测试内注明**：`s2_create_budget_secs` 的文档注释写明了这条规则。

---

## KI-3：T4 的 1 GiB 数字曾标注错误（已更正，留档防再犯）

**性质**：已修复的**测量标签**错误，不是代码缺陷。留档是因为它说明了收尾清单第 4 条的必要性。

T4 卡里的 1 GiB 测量（约 31 秒）写的是 16,384 个**内容完全相同**的块，被内容寻址折叠成
**1 个对象**，所以那 31 秒**几乎没做对象写入**。它被当作"1 GiB 写入实测"报告给了 manager 与
QA——标签是错的。

T5 用互异块重测：**192 秒，~5 MiB/s**，相差 6 倍。

**原因**：写那个测试时只关心"随机读触及几个块"，块的内容随手用了常量，没意识到这让写入测量
变成了去重测量。

**处理**：

- 基线报告以 192 s 为真实写入基线，31 s 标注为"全去重上界"；
- `gb.rs` 的模块文档与测试名已改为明示去重语义
  （`a_one_gibibyte_file_of_identical_blocks_dedups_to_one_object_and_reads_one_block`），
  防止下一个读者再把它当写吞吐；
- 沉淀为收尾清单第 4 条：**每个性能数字旁必须注明它的载荷特征**（同内容块 vs 互异块、对齐
  vs 非对齐、冷盘 vs 热盘）。

## KI-4：快照编码器的输入驻留是 O(N)，不是 ADR-005 §8 要求的 O(N/64)

- 发现于：P2a-1（2026-10-23）
- 状态：**未修**，记录待定
- 影响：内存占用随条目总数线性增长；不改变任何已发布的字节

**事实**：ADR-005 §8 写明编码器"内存 O(N/64)（只缓存各层未满页），1 GiB 文件的 extent 流与
10 万 inode 流都不需要整份驻留"。实际实现中，`encode_directory` / `encode_inode_table` /
`encode_extents` 都先把输入收集进 `Vec<Entry>`，再交给分页器，因此输入驻留是 **O(N)**。

**为什么不影响正确性**：已发布的对象字节与流式实现完全相同——`build_tree` 只按顺序消费条目，
收集与否不改变切页结果（`the_root_id_is_independent_of_insertion_order` 与
`re_encoding_the_same_tree_yields_the_same_objects` 均通过）。这是**内存行为**偏差，不是格式偏差。

**实测规模**（`docs/reports/p2a-snapshot-bench.md` §3）：

| 树 | 条目 | 条目缓冲估算 |
| --- | --- | --- |
| inode 表 | 200 000 | ≈ 13 MB |
| 单目录 | 100 000 | ≈ 4 MB |
| 1 GiB extent | 16 384 | ≈ 0.9 MB |

在 P2a 的规模下不构成压力。

**为什么不顺手修**：改成流式需要分页器接受迭代器并在跨页时保留"上一页首键"，
`build_tree` 的签名变化会波及三个调用点与全部编码测试。这是一个**独立的、可验证的改动**，
混在 P2a-1 里会让"格式实现是否正确"与"内存是否流式"两件事难以分开评审。

**建议**：作为独立小卡处理，判据是"同样的输入产出同样的根 id，同时峰值驻留不再随 N 线性增长"。
在那之前，ADR-005 §8 的这句话应视为**目标而非现状**——本文即该声明。

## KI-5：B1-3（write 与 unlink 竞争留孤儿 extent）**能复现**，波 0 的"未复现"结论作废

- 发现于：P2a-3（2026-10-23）
- 状态：**已修**（rows-affected 检查）

**此前记录**：`p2-wave0-attribution.md` §4 与波 0 报告写"B1-3 未复现（A1 级联修复后该机制已消失）"。
那是 40 轮驱动的结论——按实测概率约 5%，40 轮全过有约 13% 的可能，属于运气，不是不存在。

**实际机制**：`write_at` 先解析路径（确认文件存在），再在一个事务里
`UPDATE file SET size...` 然后插入 `file_extent` 行。若 `unlink` 在这两步之间提交，
`UPDATE` 匹配 **0 行且不报错**，随后插入的 extent 行就指向一个不存在的 inode——
`verify` 报 `ExtentWithoutInode`。

**量化**（`cargo test -p fs-api --test b1_races`，20 轮）：

| 版本 | 失败 |
| --- | --- |
| 基线（P2a-3 之前） | 1 / 20 |
| 表后端重构后（加了 `BEGIN DEFERRED` 读事务） | **11 / 25** |
| 加 rows-affected 检查后 | **0 / 20** |

读事务是 TX3 的要求（`read` 内多次读取看到同一状态），它本身没错；代价是把"确认存在"到
"写 extent"的窗口拉长了，于是把原本罕见的竞态放大了一个数量级。

**修复**：`write_file_row` 检查 `execute` 的返回值，为 0 时返回 `NotFound`，事务回滚。
符合 ADR-004 §4 对"写到不存在的路径"的承诺。

**与 B1-1 的区别**（manager 裁决不修 B1-1）：B1-1 是**丢失写入**（后写覆盖先写的块），
属语义选择；B1-3 是**数据完整性**（孤儿行被 `verify` 报损坏），表里出现自相矛盾的状态。
前者可以等 overlay 后端结构性消除，后者会让 `verify` 永远报错，必须修。

---

## KI-6：`truncate` 到非页对齐尺寸会丢掉存活的字节（**P2a-6 已修复**）

**发现于**：P2a-6，写 workspace 的测试辅助函数时（`write` 后 `truncate` 到更短的内容，
读回全零）。

**复现**：写 10 字节，然后 `truncate` 到 5：

```rust
overlay.write_at("/f", 0, b"0123456789")?;
overlay.truncate("/f", 5)?;
assert_eq!(overlay.read_at("/f", 0, 5)?, b"01234");   // 实际：[0,0,0,0,0]
```

**成因**：`truncate_impl` 用 `boundary = new_size / PAGE_BYTES` 作为"从此页往后删除"的起点。
向下取整对**页对齐**尺寸是对的（那一页正好从新结尾开始），对**非对齐**尺寸是错的：承载最后
存活字节的那页从 `new_size` 之前开始，被一起删掉了。

**影响**：数据丢失，且 `size` 仍是 5，所以读回是静默的零，`verify` 也看不出来。

**为什么 P2a-4 没发现**：当时只有"truncate 到页边界"的 K6 测试（`new_size` 为 4096 的倍数），
恰好落在向下取整也正确的那一侧。

**修复**：`boundary = new_size.div_ceil(PAGE_BYTES)`（向上取整：只删真正从新结尾**或之后**开始的页）。

**钉住它的测试**：`crates/fs-overlay/tests/contract.rs::k6_truncate_inside_a_page_keeps_the_surviving_bytes`
（断言截断后 5 字节内容仍在，且页数不变）。

---

## KI-7：ADR-006 §11.1 与 ADR-002 §8 对 cas-store 连接的 `busy_timeout` 直接冲突

**发现于**：P2a-6，写"主库锁被持有 > 5 s"的 Busy 测试时。第一次用 `commit` 触发，得到的是
`WorkspaceError::Store(StoreError::Busy)` 而不是 `WorkspaceError::Busy`——因为物化阶段先碰到
cas-store 的连接。

**冲突**：

| 来源 | 要求 |
| --- | --- |
| ADR-006 §11.1（F2 修订） | 主库上的**两条**连接（cas-store 一条、fs-workspace 一条）`busy_timeout = 5000` |
| ADR-002 §8 + `crates/cas-store/tests/crash.rs::a_cross_process_writer_reports_busy_without_retrying` | cas-store 连接 `busy_timeout = 0`，竞争立即暴露 |

**后果**：ADR-006 §11.1 自己列出的验收测试（"两个 workspace 并发 snapshot 各 1000 个文件，
均成功"）在 cas-store 保持 0 ms 的情况下**不可能通过**——大快照在 `put` 阶段就会随机 Busy。

**为什么 P2a-6 不修**：改 cas-store 的 pragma 会让上面那个已合入的测试失败，属于 ADR 级裁决，
不是 worker 能单方面决定的。本卡只保证 **fs-workspace 自己的连接**有界等待，并有测试钉住
（`a_held_main_database_lock_ends_in_a_typed_busy`，走纯主库写路径）。

**建议**：要么 ADR-002 §8 给主库连接开同样的例外（并同步改那个测试的断言），要么 §11.1 的
并发 snapshot 验收测试改为接受"重试/重排"策略。需要 manager/architect 裁决。

---

## KI-8：物化器未复用 base 的目录树 oid 与 extent（已知未优化）

**现象**：每次 `snapshot` / `commit` 都**全量**遍历并从字节重新编码：未变的目录重新编码
（结果 oid 相同，靠内容寻址不多写对象），未变的文件块重新读取并重新 `put`
（`AlreadyPresent`，同样不多写）。正确性不受影响，成本是 `O(可达 inode 数)` 的读 + 编码。

**为什么这样交付**：ADR-006 §9.2 明确把"未变目录复用 base oid / 未变块复用 base extent"
写成优化；P2a-6 任务卡给了预算条款（"物化器优化可标'已知未优化'，如实报告"）。
本卡按此标注，不宣称已实现。

**已做的**：内容按 64 KiB 块**逐块**读取（峰值内存一块，不是一个文件）；两趟分离使遍历在
一个读作用域内完成、内容读取不重入（TX4）。

**去向**：P2a-8 度量（1k/10k/100k 文件、变更 1 文件/1%/100% 的 snapshot 耗时与新增对象数），
再决定是否需要增量物化。
---

## KI-7 处置：**按 ADR-006 §11.1 修订关闭**（P2a-6 补充提交）

architect 裁决方案 (c)——**参数化，不改默认**：

- `cas-store` 新增 `StoreOptions { busy_timeout }` 与 `Store::open_with(root, mode, options)`；
  `Store::open` 保持 `busy_timeout = 0`（ADR-002 §8 原语义），既有测试
  `a_cross_process_writer_reports_busy_without_retrying` **一字未改**。
- `fs-workspace` 从 `workspace_policy.main_busy_timeout_ms` 读出这一个数，同时用于**自己的元数据
  连接**与**发对象用的 cas-store 连接**（`open_with`），所以不会出现两份配置。
- `WorkspaceError` 把 `StoreError::Busy` 与 `rusqlite` 的 `DatabaseBusy/DatabaseLocked` **归一到同一个**
  `WorkspaceError::Busy { waited_ms }`：调用方看不出是哪个连接被阻塞（§11.1 要求"错误类型与 P1 相同"）。

**钉住它的测试**：

| 测试 | 证明 |
| --- | --- |
| `cas-store::store::open_with_a_bounded_wait_reports_busy_after_roughly_that_long` | 200 ms 参数生效、约 200 ms 后 `Busy`、对象未发布 |
| `cas-store::store::the_default_open_does_not_wait` | 默认仍是 0：立即失败（ADR-002 §8 未退让） |
| `fs-workspace::core::a_held_main_database_lock_ends_in_a_typed_busy` | 元数据连接有界等待，锁释放后同一操作成功 |
| `fs-workspace::core::a_blocked_cas_index_also_waits_boundedly` | cas-store 连接同样有界等待，且错误类型与前一条相同 |
| `fs-workspace::core::two_workspaces_publish_a_thousand_files_concurrently` | §11.1 验收场景：两 workspace 各 1000 文件并发发布，均成功 |

**余量证据**（同一台机器，F:，debug 构建）：并发发布 1000 文件耗时 **left 401 ms / right 322 ms**，
相对 5000 ms 的等待上限有 12 倍以上余量——有界等待是安全网，不是常态路径。

**architect 的附加风险核对（批量发布 §12 的 fsync 持锁）**：核对结论是**当前实现不存在该风险**，且
`§12` 必须保持同一纪律。`cas-store` 的发布序列是
`published()`（只读索引）→ `stage()`（写 tmp、`sync_file`、rename、`sync_directory`，**全程不持有索引写锁**）
→ `record()`（单条 `INSERT`，隐式事务只覆盖这一条语句）。也就是说**对象 fsync 本来就在索引事务之外**。
批量发布要保住的性质是：先完成一块的对象写盘，再开**一个短**索引事务写这一块的行；不要把 fsync 搬进事务。

**顺带修正的一处**：物化器的 `PageSink` 实现原先把 `StoreError` 压成
`SnapshotError::Malformed { detail }`，这会把 `Busy` 的类型信息抹掉。改为把 store 失败暂存于 sink、
由 `materialize::publish` 还原成 `WorkspaceError`（因此上面那条 cas-store Busy 测试才能断言类型）。
---

## KI-9：`truncate` 缩容后再扩展会**复活**已丢弃的字节（**R3-1，已修复 07a4bb1**）

**发现于**：P2b-0 之后，QA 覆盖率第 3 轮（2026-10-24）的参考模型长 soak：
`cargo test --release -p fs-api --test reference_model -- --ignored`，
`seed=1000 step=357 op=truncate /n2 → 181`。常规 40×200 序列全过，10×1000 才撞到这个形状。

**复现（最小）**：

```rust
workspace.write_at("/f", 0, &[0xEE; 200])?;   // 或 0xE9，任意非零填充
workspace.truncate("/f", 66)?;
workspace.truncate("/f", 181)?;               // 扩展：按 POSIX/ADR-006 §7.3 应为零
// 实际：[66, 181) 仍是 0xEE；应为 0
```

**成因**：`truncate_impl` 只 `DELETE FROM ov_page WHERE page_index >= boundary`
（`boundary = new_size.div_ceil(PAGE_BYTES)`），**跨界页保留、尾部没有清零**；`base_limit` 的下调
只掩蔽 base 字节，掩不住 overlay 页里的旧字节。缩容后 size 检查让它们不可读，但**下一次扩展**又把
它们放了出来。

**影响**：比"字节失配"更重——**被截掉的数据会复活**（信息泄露/语义错误），且 `verify` 看不出来。

**影响面（QA 实测补强）**：不只发生在同一会话内。**未提交 overlay 里的"缩容 → 扩展"，跨 `flush` +
`close` + `reopen` 仍然成立**（overlay 落盘后旧页原样还在）。而 `snapshot`/`commit` 之后再扩展**不显现**——
因为物化把内容搬进 base、`base_limit` 把旧 overlay 页掩蔽了。这是**掩蔽，不是修复**：所以不要拿
"缩容 → commit → 再扩"作为撤销实验的判据（干净树上它也绿）。

**方向记录（重要，防后续误判）**：QA 第 3 轮报告曾写"模型期望 `0xE9`、产品全零"（产品读少了），
**方向写反了，已作废**（原邮件保留不删）。实测是产品**读多**：`first_differing_index=104`，
`product[104..112]=[233…]`、`model[104..112]=[0…]`，两侧 size 均 181。归档一律按"复活"记。

**与既有条目的关系**：KI-1（精确块边界多发零长度对象）与 KI-6（非页对齐缩容丢存活字节）是同一族
（`truncate_impl` 的边界页处理），但本条既不是"多发对象"也不是"丢存活字节"，**不要**并进那两条。

**不变式（真本体）**："任何存储页在文件 EOF 之后不得持有非零字节"——缩容与扩展共用这一条。
实现有两条等价路线，本卡选定**清零**：删除越界页之后，若 `new_size % PAGE_BYTES != 0` 且边界页存在，
把该页 `[new_size % PAGE_BYTES ..]` 填零、页仍保持满 `PAGE_BYTES`；`new_size == 0` 只删页，不走边界页。
（另一条路线是把边界页 blob 截短，ADR-006 §5.3 的 `ov_page` DDL 与 §7.2 已承认短页"短于页长的部分读作零"，
所以两条都合法；选清零的代价只是**不回收空间**——对 P2b 的 pack/空间核算留一条小注记。
**因此本实现不产生短页**：后续阶段不要照本条写短页兼容代码。）

**为什么既有回归没抓到**：`fs-overlay/tests/contract.rs` 的 K6 只覆盖"截到页边界"与"非对齐截断保留
存活字节"，**没有"缩容后再扩展 / 再写越 EOF"的组合**；参考模型的常规序列没撞上，长 soak 撞上了。

**钉住它的测试（修复时需补）**：crate 层至少四例（缩后扩、缩后 `write_at` 越 EOF、缩到 0 再扩、
缩容 → `snapshot`/`commit` → 再扩展读取），加参考模型 soak 全种子汇总。

**其中只有一部分是 A/B 有效的**（QA 实测）：

| 用例 | 干净树上 | 用途 |
| --- | --- | --- |
| 缩后扩（页内 / 跨页） | **红** | A/B 判据 |
| 缩后 `write_at` 越 EOF | **红** | A/B 判据 |
| 缩到 0 再扩 | 绿（`truncate(0)` 删光全部页，无跳界页可残留） | 仅作覆盖用例 |
| 缩容 → **`flush` + `close` + `reopen`（不 commit）** → 再扩 | **红** | 持久化路径的 A/B 判据 |
| 缩容 → `snapshot`/`commit` → 再扩 | 绿（被物化掩蔽） | 仅作覆盖用例，**不得**用作撤销实验判据 |

参考模型 soak 是端到端兑底（`seed=1000 step=357`，干净树必红；修后 10×1000 汇总全绿）。

**修复与验收（QA 终态 PASS，`07a4bb1`）**：清零实现（`live_end = min(old,new)`、删 `⌈live_end/page⌉`
起的页、跨界页自 `live_end % page` 起原地清零，页恒为 `PAGE_BYTES`），A/B 六判据成立（crate 层 3 + API 层 3），
4 例覆盖用例显式标注（对齐缩容 / `truncate(0)` / `commit` 掩蔽 / `snapshot` 掩蔽）。QA 用 10 个自写形状独立
交叉验证：7 例红→绿、3 例绿→绿，与本节分类一致。

**归类更正（同一轮的重要发现）**：soak 改为"跑完全部种子再汇总"后，干净树上 **10/10 种子全部失败**
（`1000 step=357`、`1001 step=195`、`1002 step=357`、`1004 step=201 write_at`、`1006 step=436`、`1008 step=90` …），
不是"只有 seed 1000"。此前"仅 seed=1000 失败"是 panic 即停造成的假象——**首次失败即停的测试会低估缺陷面**，
这也是把汇总模式纳入验收的原因。

**遗留观察项（非阻塞，未在本卡修）**：

- **N1**：`content_state().pages` 只能钉"页数"，钉不住"页不短"（短页也是 1 行）。"本实现不产生短页"目前是
  构造性事实而非测试钉住；建议在 `fs-overlay/src/content.rs` 模块内单测直接断言缩容后 `data.len() == PAGE_BYTES`。
- **N2**：soak 汇总的 `summarise` 截断 240 字符，长 `write_at` 信息行会截掉字节段（step/路径/尺寸保留）。
- **N3**：`k6_aligned_shrink_then_growth_...` 的注释把"扩展不重建页"（由 `pages` 钉）与"读路径为零"（由读断言钉）
  写在一句里，建议分开。

---

## KI-10：`verify` 对 pack 的判据过严会把**健康**的 Store 判为损坏（**P2b-2 内已修**）

**发现于**：P2b-2 的并发发布回归（`concurrent_publishers_of_one_object_leave_one_row_and_a_clean_store`，
20 轮 × 4 线程）。一个完全健康的 Store 被 `verify` 报 corrupt。

**成因**：最初的 `object::verify_pack` 要求索引行**铺满整个 pack**（帧连续、覆盖 `frames_len`、`count == 行数`）。
这与 ADR-008 §5 的 `INSERT OR IGNORE` 冲突：并发发布同一对象时，**败者的帧仍写在 pack 里**（合法死字节），
索引行必然覆盖不全。

**判据（ADR-008 §7 原文）**：每行 `(offset, length)` 在 body 内、非零、**互不重叠**；帧之间允许空洞
（死字节，由 GC 重打包回收）。

**教训**：自校验的判据必须按"规范允许哪些形态"写，而不是按"单写者路径下长什么样"写。并发/崩溃状态
会把"看起来该连续"的假设打破，而 `verify` 一旦过严，就会把正常数据报成损坏——比漏报更糟，因为它会
诱发"修数据"的错误动作。同类风险：任何"应当铺满/应当连续"的断言，都要先问一遍"并发败者/崩溃残留
是否合法地破坏了它"。

**钉住它的测试**：上述并发用例（要求 `verify` 干净）；以及 `verify` 对"人为制造空洞"的 pack 仍判 ok、
对"重叠/越界/零长行"判错。

---

## KI-11：并发发布下 `verify` 可观察到"`pack` 行存在、文件缺失"（**已定性：外部删除，2026-10-24**）

**发现于**：P2b-2 的并发发布验收（`fs-workspace/tests/pack_concurrency.rs`，两条用例），QA 独立复现。

**现象**：两个 workspace 并发发布后，由**新开的** `Store` 做 `verify`，得到

```
VerificationReport { checked: 9, packs_checked: 3,
  failures: [4 个 oid → "store i/o error: The system cannot find the file specified (os error 2)"],
  pack_failures: [1 个 pack_id → 同上], bad_names: [] }
```

即 `pack` 行与 `object_location` 行都在，该 pack 文件 ENOENT；每次恰好缺一个 pack（`packs_checked: 3 / checked: 9`
恒定）。

**复现率**：worker 3–4/12；QA 1/140（`%TEMP%` 40 轮命中 1 次；另两组 0 次——因此**不能**据此断言"Defender 扫 TEMP"）。

**已排除（决定性）**

1. **环境/文件系统**：4 线程 × 500 轮 × 6 = 12 000 次 `write → fsync → rename → stat`，missing 0。只排除了通用 syscall
   模式，**不能**排除"定向删除"（扫描器对特定目录/策略）。
2. **跨用例 static 串扰**：`--test-threads=1` 连跑 12 次仍 4 红。
3. **写路径未落地**：`record_pack` **提交前**断言 `packs/<id>.pack` 存在且大小匹配，从未触发——提交那一刻文件在、
   大小正确。

**因此范围缩到"提交之后文件不再可见"**：删除类（外部删除 / 目录级清理 / delete-on-close）或命名空间可见性，
未定性。QA 的现场打印形似 Windows **delete-pending**（另两个 pack 的 `metadata()` 仍返回正确大小，而
`read_dir(packs/)` 枚举为空）；但她的枚举用了 `.flatten()`，在 Windows 并发枚举下会静默丢弃 entry error，
所以"目录为空"**不能**当铁证。

**对不变式的威胁（为什么必须给它建卡）**：`pack` 行 ⇒ 文件存在，是 P2b-6 的 GC（删无行文件、重打包）与
`verify` 的前提。带着未定性的缺口做 GC 等于在流沙上盖楼。**欠账 → P2b-2c（优先于 P2b-6）**。

**已做的处置**：两条并发用例标 `#[ignore]`（理由写明"索引可指向缺失 pack"），`staging-audit` 诊断 feature
（默认关；含 verify 侧 ENOENT 重试 + 完整路径 + 逐条 entry error、rename 后 evidence 硬链接、`pack_id` 实样）。

**定性结论（P2b-2c，2026-10-24）：外部删除（本机某进程删除新建的 `.pack`）**

- 现场（µs 级忙轮询 + 产品内 `staging-audit`）：rename 返回 Ok 后文件在 `packs/` 存在约 **100 µs**，随后连同
  `evidence` 硬链接一起消失；索引行已提交 → `verify` 看到"`pack` 行在、文件 ENOENT"。
- **决定性实验**：从 rename 返回到索引 `COMMIT` 持有 `share_mode(READ|WRITE)`（**不含** `FILE_SHARE_DELETE`）的
  句柄 → **12/12 干净**（同 harness 基线 4/12）。Windows 上该句柄存在时删除必然失败，且 pin 不改变目录可枚举性
  ⇒ 只能是"阻止了删除"。
- 已排除：路径不一致（发布方 raw/canonical 同目录）、跨用例 static 串扰（`--test-threads=1` 仍红）、
  写路径未落地（rename 后审计仍见文件）、目录级清理（同目录另两个 pack 一直在）、"`%TEMP%` 特殊性"
  （换卷同率 2/200 vs 2/200；但**不能**排除全局 AV/EDR——Defender 扫所有卷）。
- **删除者**：本机由**第三方安全软件**提供实时防护（`SecurityCenter2.AntiVirusProduct.productState = 266240`，
  实时防护位为 1；Defender 不是主防护），其 `Get-MpThreatDetection` = 0、`ExclusionPath` = 空、MPLog 不存在。
  该第三方 AV 是**首要嫌疑但不是结论**（未抓到具体进程）。早期记录保留：`Get-MpComputerStatus`
  在本机失败 ⇒ 只能说"没有可见的 Defender 处置痕迹"，不能说"AV 无关"。
- **产品侧处置**：不修"删除"（非本仓库可控），改为让损坏**可诊断**（P2b-2d：`MissingPack` typed 语义）；
  发布期可见性确认/有界重 staging 是否做，待 architect 判定（P2b-2e）。
- **对不变式的影响**：`pack` 行 ⇒ 文件存在，**在本机环境下不成立**。P2b-6 的 GC 设计不得静默依赖它：
  mark/sweep 遇到"行在文件缺"必须按"存储损坏"计数并报告，不得当成"没有对象"或"孤儿文件"处理。

**已知非缺陷但需记**：`Repo::open(create=false)` 会打开**两次** `Store`（先 probe `busy_timeout`，再正式开），
每次 `open` 都执行 `clear_temporaries()`——一次 commit 期间可能有 4 次 `tmp/` 清扫插进来，这解释了并发下
staging 重试为何频繁（与 ADR-002 §7.0 一致）。
