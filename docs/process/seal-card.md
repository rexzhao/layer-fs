# Seal 卡（草稿，待 owner 决定阶段顺序后派发）

> 状态：**草稿**。来源：worker 的只读影响分析（2026-10-24）+ ADR-007 / ADR-006 §15–§16 / ADR-008 §6.2。
> 未派发：owner 尚未决定「P2b 收口」还是「直接进 Seal」。

## 0. 为什么单独成卡

`sealed_content` 是 P2b-6 的**唯一 pending 根来源**：`RootKind` 有、列无。P2b-6 已用两层 tripwire（T1 全列清单、T2 注册表↔判据）
与 K 矩阵把"Seal 合入时必须回来补"写成了机制。**本卡的第一件事就是让那三处按设计变红、然后修绿**——不是"顺便加一列"。

## 1. 三处 tripwire（设计好的护栏，必须经历 红 → 修 → 绿）

| # | 位置 | Seal 前的状态 |
| --- | --- | --- |
| 1 | `gc_roots.rs::CLASSIFIED`（全列黄金清单，60 列） | 新列出现 ⇒ 实际集合 ≠ 冻结集合 ⇒ **红** |
| 2 | `t2_every_classified_root_source_has_a_kind_and_a_kind_has_a_column` | `SealedContent` 是**唯一允许的 pending 项** ⇒ 必须删掉这个例外 |
| 3 | `gc_canaries::every_root_kind_is_covered_by_a_named_canary` | 同上 ⇒ 删 pending + 补一条**真金丝雀** |

**验收**：先提交一次"只让它们红"的证据（跑出三处红），再修绿。**不许**直接把 pending 项删掉了事；顺序必须是 红 → 修 → 绿。

## 2. 结构性改动：根不一定是树（worker 分析，最高优先级）

ADR §6.2 说 `sealed_content` 引用「Blob 的 chunk，或 Extents 树根」。而 `gc::run` 现在对每个根都走
`SnapshotReader::open_any(&source, root)`，它要求 role = `FilesystemRoot`。**把 chunk 或 extent 根当树根传进去会 `WrongRole` 失败**
——于是 GC 在一个完全合法的 sealed 根上中止。

**裁决：统一成"从任意对象起步"**（`references` 本来就对任意 role 有定义，树根只是其中一种）：

- `run` 的 mark 不再对每个根调 `open_any`；树根的"取根值"只发生在需要时（或保留一条"若是 FilesystemRoot 则 open_any"的快路径）；
- 接受"根不是树"这一事实：`reachable` 里会出现裸 chunk；
- G1/G3 的判据不变（只比较 oid 集合）。

**用例**：根是 chunk 时 mark 不失败，且该 chunk 的引用被覆盖；根是 extent 根时同理。

## 3. 需要 architect 裁定的问题（可能删数据）

ADR-006 §15 提到 overlay 页可能**直接引用**已封存 chunk。若如此，**引用边在 overlay 的字节里**，而 GC 的 `references` 只懂
fs-snapshot 对象 ⇒ **GC 看不见这条边 ⇒ 静默漏标 ⇒ 删数据**。

**worker 的建议（我倾向采纳）**：Seal 卡按"**凡被 overlay 页引用的 chunk 必须同时登记在某行 `sealed_content`**"设计，
即 overlay 引用是 `sealed_content` 的子集。这条必须写进 ADR-006 §15 或 ADR-008 §6.2，否则 GC 的正确性没有依据。

替代方案（不采纳，除非 architect 反对）：给 GC 一个 overlay 侧的引用读取器（新代码 + 新判据 + 新 tripwire）。

## 4. 金丝雀（Seal 版）

`K5c` 变体：一个对象**仅**被某 overlay inode 的 `sealed_content` 引用 → 丢掉该 seal 之后的下一次 GC 被回收，之前不被回收。
若 Seal 尚未提供建该状态的 API，用 `gc-test-hooks` 的 seam 直接写行（与 K9 同一先例）。

## 5. 已就绪、不需要改（worker 已核对）

- `repo.rs::root_rows_from` 已探测 `pragma_table_info('ov_inode')` 并逐行加根、过滤空串 ⇒ 列是"一行一个 id"时**枚举无需改**；
- `MaintenanceRoots` / `Overlays::Held` / `MaintenanceSource` / `repack` / 崩溃五点 / T3 值域 都不需要改；
- `verify --deep` 只覆盖主库根（ADR §6.2）⇒ **sealed 内容没有任何 verify 覆盖**；这是判据归属问题，不是实现问题。

## 6. 待定项（需要裁决，不能默认）

1. **overlay 页引用 chunk 是否必须同时登记 `sealed_content`**（§3）；
2. **sealed 内容是否要 `verify` 覆盖**：当前答案"不覆盖"，但必须在 ADR 里写明"不静默宣称"；
3. Seal 与 `branch_log` / `snapshot drop` 的交互（sealed 内容被 drop 的 workspace 引用时如何回收）——若 ADR-007 已有定论，引用之。

## 7. 交付顺序（拿到裁决后）

1. 先落"三处 tripwire 变红"的证据提交；
2. §2 的裸对象根改造 + 用例；
3. §4 的金丝雀；
4. §3/§6 的裁决落 ADR（我直提，worker 只改代码与测试）；
5. 常规四件套 + 全量 + 崩溃五点回归。
