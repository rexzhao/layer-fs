# 开发前调研：LayerFS 参考研读与 P1 落地建议

状态：manager 调研报告，2026-10-22
依据：[workspace_cas_fs_spec.md](../../workspace_cas_fs_spec.md)（v0.1）§9 任务 1–5 的前置调研
参考对象：`reference/`（LayerFS @ 71e0a3c，MIT 许可）。**只研读机制与参数，不复制代码或协议**（spec §10 借鉴边界）。

## 1. LayerFS 与本项目的分层映射

| 本项目（spec §3/§7） | LayerFS 对应 | 成熟度参考 |
|---|---|---|
| cas-core（ObjectId/编码） | layerfs-content `object/`（id.rs/codec.rs） | 已投产，冻结式设计 |
| cas-store（持久化/事务） | layerfs-persistence + layerfs-storage `store/`、`save/` | SQLite + pack |
| fs-content（Blob/Chunk/Extent） | layerfs-content `file/`（cdc、mapping） | FastCDC + Extent B 树 |
| fs-namespace（目录/inode/Root） | layerfs-content `filesystem/`（root、directory、inode、sorted） | inode 表 + 排序目录页 |
| fs-workspace（P2） | layerfs-overlay / layerfs-workspace | 每挂载独立 SQLite overlay |
| fs-winfsp（P3） | （LayerFS 用 Linux FUSE，无 WinFsp） | 本项目自建；PoC 已验证 WinFsp 可行 |
| Snapshot/Branch/Commit | layerfs-persistence `history/`（branch/commit/lineage） | CAS head + lineage 可达性 |

关键结构性结论：LayerFS 把**一切内容**（文件块、extent 树页、目录页、inode 表、甚至元数据增量）都做成规范 CAS 对象，SQLite 只存索引/位置/历史；本项目 P1 可以采用相同原则但简化物理层（spec 建议：SQLite 元数据 + 独立对象文件）。

## 2. 关键机制研读

### 2.1 对象身份与信封（ADR-001 的直接参考）

- `ObjectId = BLAKE3("layerfs/object/v2\0" || canonical_object_bytes)`，32 字节，域分隔符**冻结且带版本**。
- 规范信封：`magic(4) | kind(1) | payload_len(BE32) | value_len(BE32) | value`，单对象上限 16 MiB；解码器拒绝一切 magic/kind/长度/尾字节违规。
- **身份 ≠ 物理表示**：同一规范对象可存为 FULL / STORED / PREFIX（压缩/增量）三种物理记录，身份永远由规范字节决定。读取路径负责从物理记录还原规范字节并（按策略）验证身份。
- 流式身份：`IdentityStream` 支持分片喂入、不驻留整对象。

**对本项目的启示**：spec §4 的 `domain || version || role || payload` 唯一身份与 LayerFS 的"冻结域 + 规范信封"完全同构；version/role 应作为信封内的显式字段参与哈希，防跨角色字节巧合（spec §4 第 1 条）。

### 2.2 FilesystemRoot 与命名空间

- Root = `profile ObjectId + inode scope(32B) + root serial(u64) + inode-table root ObjectId`，编码为带魔数/版本/role 的规范对象。
- **Profile description 字符串钉死全部命名空间参数**（scope 位宽、serial 位宽、页大小、深度上限、目录填充率…），`profile_id = BLAKE3(description)`；打开/解码时校验 profile，不匹配显式拒绝、绝不静默转换。
- inode 表与目录页本身也是 CAS 对象（B 树，页 ≤8 KiB，深度 ≤31）；目录叶 = `name → inode serial` 行。

**启示**：① "冻结 profile + 显式拒绝"是我们对象格式 v1 必须抄袭的**思想**；② P1 可以先做"SQLite 表存目录行 + 事务发布 Root"，P2 再对象化目录页（见 §4.3 两条路线）。

### 2.3 CDC 分块（P2.6 参数印证）

- 两字节滚动 GEAR FastCDC：**min 8 KiB / target 16 KiB / max 32 KiB**，归一化位移 2，seed=0，双 mask（small/large）。
- `profile_id = BLAKE3(域 + 全部参数 + gear 表)`——**改任何参数即改变所有 chunk 身份**，profile 记录进 Store policy。

**启示**：与 spec §2 P2.6 的建议值完全一致；我们的 Store Policy 必须同样冻结并持久化 profile_id，打开时校验。

### 2.4 Extent 树（P2.7 结构印证）

- 逻辑偏移 → chunk 载荷切片：叶 `ExtentSlice{payload_object_id, source_offset:u32, logical_len:u32}`；分支带累计逻辑字节/条目数 + 子页 ObjectId。页 ≤8 KiB、每页 ≥64 条目。
- 关键不变量：切片上界按**chunk 语法全局上限**校验（不是按恰好指向的 payload），解码即拒绝伪造页。

### 2.5 持久化布局与 schema 治理

- 表：`store_policy`（format_profile、small_file_threshold、delta 深度）、`object_location`、`inode_value`、`content_signature`、`store_totals`、`pack`/`pack_unit`（**对象数据以 pack BLOB 存在 SQLite 内**）、`history_meta`、`layer_stack`/`layer`/`commit`/`branch`、`scope_allocator`、`file_digest`、lineage 系列（`layer_lineage`/`commit_lineage`/`absorbed`/`reach`）。
- schema 版本：`PRAGMA user_version` + `store_policy.schema_version`；打开时**校验 schema_definition 文本逐字一致**，防篡改/防误迁移；Store 永不静默迁移。
- Pack 常量：group target 48 KiB、pack 上限 256 KiB、group ≤256、record ≤8191、批 ≤512 对象——**波次（wave）受事务容量约束**。

**启示**：① schema 自校验（存储建库语句文本并打开时比对）值得 P1 直接采纳；② LayerFS 把对象数据放进 SQLite pack 行——简化一致性但放大库体积；spec 建议 P1 用"SQLite 元数据 + 独立对象文件"，两者事务边界不同（见 §4.2）。

### 2.6 Save 生命周期与写入路径

- `Save` = 单生产者：`accept(FinalizedObject)` 分批 → 有界 wave 落盘 → `finish()` 终结；**已发布批次幸存后续失败**；终态后拒绝再写。
- 写入顺序（spec §4 的原型）：完整数据写入+校验 → 索引提交 → Root 发布前验证全部依赖可达。

### 2.7 Branch/Commit 的 CAS 语义（P2.9 印证）

- `commit_branch` 单事务：校验 expected_head/expected_base → 校验 expected_root 与 head 一致 → 插入不可变 commit → 推进 branch head；头已移动则返回**带期望/实际值的类型化错误**，绝不静默覆盖（force 覆盖另有显式请求 + displaced 报告）。
- 可达性：lineage 表（height/run/depth + reach）支撑"谁引用了什么"，是 GC 正确性的数据基础。

### 2.8 值得选择性借鉴的工程治理（LayerFS 仓库规则）

- **一次操作、不自动重试**；精确失败保管（失败保留原始原因与已产生效果，不猜恢复）。
- **单写者 + 短持有**：重活全部在事务/锁外，事务内只做原子提交步骤。
- 生产文件 999 行 / mod 200 行上限；每提交记录生产 LOC 对比；测试单次运行 ≤2 分钟。
- 声明与证据分离：实现、研究、测量、提案的陈述必须分开，"研究不等于能力"。

采纳建议：前三条作为本仓库 worker 任务验收标准的一部分（写入任务卡）；LOC 对比 P1 暂不强制（规模小），但提交说明必须含"实现范围/未实现列表"（spec §8 已要求）。

## 3. 与本项目的关键差异（避免照搬）

1. **持久化档位**：LayerFS 全局 Store 是 Disposable/WAL/sync=OFF（进程崩溃存活即可，刻意不做掉电持久）；**我们 spec P1.7 要求崩溃恢复**——P1 对象文件与索引提交必须走 durable 顺序（写 tmp → fsync → rename → fsync 目录 → 提交索引事务），成本更高但符合规格。
2. **平台**：LayerFS 是 Linux/FUSE 产品（Windows 不可编译，前次分析已证）；我们核心 Rust 跨平台、Windows 优先。核心代码从第一天起不得引入 `nix/libc/std::os::unix` 无门控依赖（教训来自 LayerFS 的 115 处 cfg 站点 + 4 个未门控 nix 依赖）。
3. **多进程模型**：LayerFS 后期演进到多写者准入很复杂；P1 我们按 spec §11-2 只承诺"单机多进程 + 一个本地 Store"，靠 SQLite 自身事务即可，不自建准入。

## 4. P1 设计建议（供 ADR-001/002 起草）

### 4.1 对象格式 v1（ADR-001 输入）

```
ObjectId = BLAKE3( "wcas/object/v1\0" || canonical_bytes )   // 32B，域冻结
canonical = magic "WCAS" | version u8 | role u8 | payload_len BE32 | value_len BE32 | value
roles(v1): 1=bytes(整文件/任意规范字节) 2=chunk 3=extent-leaf 4=extent-branch
           5=directory-leaf 6=directory-branch 7=inode-table 8=filesystem-root
```

- role 与 version 显式进信封（满足 spec §4-1"跨角色不因字节巧合混淆"）。
- 上限初值：单对象 16 MiB（对齐 LayerFS 经验值），文件名长度、目录页/extent 页条目数在 ADR-001 定死并写入 Store Policy。
- 读取校验策略：默认整对象 BLAKE3 重算（P1）；chunk 级抽样/缓存校验是 P2 优化项，须另立 ADR。

### 4.2 Store 物理布局（ADR-002 输入）

```
<store>/
  store.sqlite        // 元数据+索引+历史（WAL 模式）；建库 SQL 文本存入自校验表
  objects/ab/cd/<oid> // 独立对象文件，2 级前缀目录；内容=canonical bytes
  tmp/                // 写入暂存，崩溃后启动清理
```

- put 顺序：写 `tmp/<uuid>` → fsync → rename 到 objects 路径 → fsync 父目录 → SQLite 事务提交 location 索引。索引提交即"发布"；重名对象幂等（先查 exists）。
- P1 GC：只做"启动时清 tmp + verify 全量校验工具"；引用计数 GC 留 P2.11。
- 理由：与 spec §11-1 建议一致；比 LayerFS 的"全 pack 进 SQLite"少一个 pack 引擎，代价是大对象下 SQLite 体积可控但文件数多（2 级前缀 + 后续 P2 可加 pack 化扩展位：role/format 字段保留 `stored_as` 列）。

### 4.3 Namespace 两条路线（ADR-001 必须二选一，建议 A）

- **A（建议，P1）：SQLite 常驻 namespace + 事务发布 Root**。目录行/ inode 记录存 SQLite 表；每次写操作 = 单事务（改行 + 生成新 root 记录 + CAS head）；root 表存 `root_id = BLAKE3(规范 root 描述)`，旧 root 行保留不可变（=快照雏形）。满足 spec P1.3/P1.6（原子发布、旧 root 不被破坏）且实现最短。
- B：LayerFS 式全对象化（目录页/inode 表是 CAS 对象）。优点：Root 自包含可导出/分发；缺点：P1 工作量翻倍。P2 若需"Store 导出/迁移"再演进，届时写 ADR + 迁移。

### 4.4 大文件（P1.4）

P1 用固定块（建议 64 KiB，2 的幂、≠CDC 目标以免混淆）+ 行程编码的 extent 列表存 SQLite（`file_extent(file_id, logical_start, len, object_id, source_offset)`），`read_at` 按区间查询合成；不做 B 树对象。1 GiB 文件 ≈ 16384 行，SQLite 索引下随机读可接受（P1 验收测试会给出实测数）。

## 5. 开放决策建议（spec §11，待负责人确认，不阻塞 T1）

| # | 问题 | 建议 | 依据 |
|---|---|---|---|
| 1 | SQLite+对象文件 vs Day-1 Pack | **SQLite+独立对象文件** | spec 建议；LayerFS pack 复杂度高 |
| 2 | P1 单机多进程单 Store | **是** | SQLite WAL 已足够；避免 LayerFS 式准入复杂度 |
| 3 | 核心大小写语义 | **核心区分大小写**（原始字节名），Windows 前端折叠 | LayerFS PathName 同此；P3 责任边界清晰 |
| 4 | P1 write_at 事务 | **每调用一内部事务**，无显式 begin/commit | spec 建议；LayerFS per-op 同构 |
| 5 | 保留策略/GC Root 归属 | **P2 由 fs-workspace 持有 root 集合**（lineage 表模式）；P1 仅 head+commits | LayerFS lineage 先例 |
| 6 | 去重安全域 | **同一可信本地用户/Store** | spec 建议 |

## 6. 风险清单（P1 执行时盯住）

1. **Windows fsync 语义**：`File::sync_all` 映射 FlushFileBuffers，目录 fsync Windows 无直接 API——用 `rename` + 父目录重开/或接受"最后写入者命名事务"弱化点并在 ADR 声明。崩溃测试必须真做（kill -9 / 任务管理器强杀）。
2. **对象文件数量爆炸**：10 万小文件 → 10 万对象文件；NTFS 目录枚举性能。2 级前缀（256×256）是下限，P1 基准测试必须覆盖 100k 文件场景（spec §6）。
3. **BLAKE3 依赖**：纯 Rust、跨平台、无 unsafe 问题，LayerFS 同选；锁定版本。
4. **规格红线**：不得提前实现 CDC/delta/WinFsp/Workspace（spec §0-1）；P1 的对象格式必须留扩展位但**不宣称**未实现特性（spec §0-5）。
5. **借鉴边界**：LayerFS MIT，可借鉴思想；但对象信封/域字符串/SQL schema 必须**自行设计**，不逐字复制（spec §10）。

## 7. P1 任务分解（worker 任务卡索引）

按 spec §9 顺序，串行依赖 T1→T2→T3→T4→T5（T3/T4 可在 T2 验收后并行）：

| 卡 | 内容 | 验收要点（spec §6/§8） |
|---|---|---|
| **T1** | Cargo workspace 骨架（按 spec §7 精简为 3 crate：cas-core/cas-store/fs-cli，其余 P2 增设）+ ADR-001（对象格式 v1、ObjectId、大小写、错误语义）+ `ObjectId`/信封编解码 + 流式身份 + 单元测试 | 编译零警告；编解码往返/畸形拒绝/跨 role 不混淆测试；ADR-001 入 docs/adr/ |
| **T2** | cas-store：objects/ 布局 + put/get/exists/verify + 原子发布顺序 + 重启恢复 + tmp 清理 + 崩溃注入测试（写入各阶段强杀） | spec §6 P1-2/3：幂等、恢复、坏对象报错不填零；故障点覆盖 spec §4-8 |
| **T3** | fs-namespace（路线 A）：目录/inode/root 表 + 路径 API 语义 + 事务发布 Root + 旧 Root 不可变 | spec §6 P1-1/4：100 路径同内容只存一份；写前后 Root 可读 |
| **T4** | fs-api + CLI（init/put/get/ls/stat/mkdir/rm/mv/verify/stats）+ 错误类型 | CLI 与 SDK 同核；无效路径/越界/短读全测（spec §6 P1-6） |
| **T5** | P1 验收矩阵全量执行 + 性能基线（100k 小文件、1 GiB 随机读、内存上界）+ 报告 | spec §8 格式提交；性能不得单次最佳值 |

任务卡全文与实时状态经 mailbox 由 manager 分发/验收；每张卡完成定义 = 测试绿 + 提交说明符合 spec §8 + manager 验收。

## 8. 结论

LayerFS 验证了本 spec 的技术路线全链路（CAS/冻结 profile/CDC/Extent/CAS-head/lineage-GC）在真实工程中可行且已投产。P1 按"简化物理层 + 保留规范层思想"推进：SQLite+对象文件、固定块 extent、路线 A namespace。最大风险在 Windows durable 语义与海量小文件，已列入 T2/T5 验收强制项。
