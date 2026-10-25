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

## 8. 实现前必须定死的四点（worker 复核 b7f2f05 后补充，2026-10-24）

### 8.1 `sealed_content` 的 schema 形态：建议**单独一张表** `ov_sealed(serial, oid)`

现读法是 `SELECT DISTINCT sealed_content FROM ov_inode WHERE … AND sealed_content <> ''`，只在"一个 inode 只有一个 id"时成立。
**一个 inode 封存一整段内容完全可能是多个 chunk**；若该列存列表/JSON，`parse_oid` 失败会让 GC 拒绝整次运行。

- (a) 列 = 单个 id（一 inode 一 chunk / 一 extent 根）→ 现有读法可用；
- **(b) 单独一张 `ov_sealed(serial, oid)`**（一 inode 多 chunk 的天然形态）→ 枚举读该表；T1 清单加整表列，T2 仍映射到
  一个 `RootSource(SealedContent)`。

**本卡倾向 (b)**：避免未来为"一 inode 多 chunk"再做一次 schema 升级，且 T1 的分类粒度更清楚。**形态未定前不动枚举代码。**

### 8.2 label 必须含 inode serial

现 label 是 `sealed:{workspace}:{root}`，而 `:root` 是对象 id、不是行 id。**两个不同 inode 封存同一个 chunk 时 label 相同**，
而 `RootSet::try_from_entries` 是**按 label 去重**的 ⇒ 两条事实被合并成一条 ⇒ `roots_declared` 少一条。
这正是 §29 修掉的"看不到"缺陷，只是这次出在 label 生成上。

**要求**：`sealed:{workspace}:{serial}:{root}`（行 id 进 label）。`pending_root` 无此问题（一个 workspace 一行）。

### 8.3 不留"树根快路径"

草稿 §2 允许"若是 `FilesystemRoot` 则 `open_any`"。**不要**：两条路径意味着"树根走 A、裸根走 B"，判据只覆盖其中一条时另一条是盲区；
而 `references(oid)` 对树根同样正确。直接删掉 `run` 里的 `open_any`，`visited_oids`/`reachable` 语义不变。

### 8.4 "被 drop 的 workspace 仍持有 sealed 内容"的回收时机必须写成可判的

P2b-6 的实际行为：`snapshot_drop` 只删行，被 drop 的 workspace 若仍持 `sealed_content`，其对象**仍可达**（overlay 根还在），
直到该 workspace 行被 `workspace discard`。**两种回收时机都能"通过"不可分辨的测试 ⇒ 必须在本卡写明是哪一种**，并据此写用例。

### 8.5 若选 `ov_sealed` 表：T1 按**列**分类，不按整表

新增表的列要逐列分类：`oid` → `RootSource(SealedContent)`；`serial` → `NotARoot(overlay inode identity)`（可能还有 `created_ns`）。
**不要**把整张表算成一个根来源：否则 T3 会去抽样 `serial`，而它"能解析成 hex 但查不到索引行"⇒ 误红。

### 8.6 删掉 `open_any` 之后，"裸根不失败"必须有独立证据

`gc::run` 删掉 `open_any` 后不再有任何地方校验"根是不是树"。这不影响判据（G1/G3 比 oid 集合），但要留一条**独立**用例：
把"根是 Bytes 对象"与"根是 chunk"两种裸根各跑一次 mark，断言 `references` 分别给出**空边集**与**真实边集**——
这样"裸根不失败"是**有证据**的，而不是"删掉了检查所以不会报错"。
