# ADR-007：内容定义分块、增量物化与 Seal

- 状态：**草案（Proposed），分叉已决**——architect 起草，P2b-0/P2b-1/P2b-3/P2b-4/P2b-5 的设计输入；
  worker 落地核对后转 Accepted。分叉 G1/G5/G6/G7 的 owner 结论见 §2.1
- 日期：2026-10-23
- 起草：architect
- 依据：spec §2 P2.5（Seal）、P2.6（CDC）、P2.7（Extent/Chunk 复用）、§5.1–5.5、§6 P2 第 2/3/6 条；
  ADR-005（快照格式，S3/S10）；ADR-006（overlay、物化器 §9.2、W4）；p2a-baseline §4；KI-4、KI-8
- 修订：**取代** ADR-005 §3.2 的"文件形态规范化规则"与 §5 的 profile v1；**取代** ADR-006 §9.2 的全量物化器
  （全量实现降为测试 oracle）；**扩展** ADR-006 §5.2 的 overlay schema（Seal 列）

---

## 1. 背景与问题

P2a 交付了正确但昂贵的发布路径（p2a-baseline §4）：

- 文件内容按 64 KiB **固定块**切分：在文件中间改几个字节，只重写一块，但"插入"在文件系统 API 里
  不存在（write_at 是原地覆盖），所以固定块的局部性已经足够；**固定块的真正缺陷是跨文件去重**：同一段
  内容在两个文件里偏移不同，就落在不同的块边界上，永远不会去重（spec P2.6 引入 CDC 的原因）；
- 每次 snapshot **全量遍历**并重编码：无变更的第二次 snapshot 仍要 2.2 s（KI-8）；
- overlay 中的脏数据只有在 snapshot 时才变成 CAS 对象：10 个 workspace 写同一份 1 GiB，物理上存 10 份，
  直到各自 snapshot（spec P2.5 要求 Seal 在不产生业务 Snapshot 的情况下完成物理去重）。

## 2. 决策摘要

| # | 决策 |
| --- | --- |
| C1 | 文件内容用 **FastCDC（归一化分块，GEAR）** 切分，min 8 / avg 16 / max 32 KiB **[G6 已定]**；参数与 gear 表冻结进 **snapshot profile v2** |
| C2 | 规范形态：对整个逻辑字节流（空洞按零）做 CDC；1 个非零 chunk → Blob；多个 → Extents（全零 chunk 不写）；空或全零 → Empty |
| C3 | 文件内容对象统一用 **role 2（Chunk）**；Blob 是"一个 chunk"的退化情形 |
| C4 | extent 树格式不变（ADR-005 S10）：`length` 可变、`source_offset` 恒为 0；**不做 chunk 切片复用** **[G5 已定]** |
| C5 | 一个**增量分页器**同时服务目录树、inode 表、extent 树与文件内容的局部重切：从变更点前最近的真实切点重切，直到新旧切点重合 |
| C6 | **正确性判据**：任意 base 与变更集，增量结果的根 id ≡ 全量编码的根 id。全量物化器保留为测试 oracle（`test-support` feature），生产只有增量一条路径 |
| C7 | 增量物化的变更集 = overlay 的 `ov_inode` / `ov_entry` 行；依赖不变量"live 行 ⇔ 从根可达"，V4 在 debug/测试/`verify --deep` 中校验 |
| C8 | **Seal = 单文件提前物化**：同一编码器产出该文件的规范 Content，overlay 中以 `sealed_content` 替代该文件的全部页；版本号防竞争，冲突即放弃 |
| C9 | Seal 有字节预算与取消令牌；触发：API / CLI 显式，进程内后台线程默认关闭 **[G7 已定]** |
| C10 | profile v1 的根一律拒绝（`UnsupportedProfile`），不迁移 **[G6 已定]** |

### 2.1 owner 决策记录

以下分叉均由 **owner 于 2026-10-23 通过 qclaw@2qcuc12p 确认**按 architect 建议执行（经 manager 转达，
mailbox `manager/c7yba9ud6s`、`manager/taek3xts1d`）：

| 分叉 | 结论 | 落在本 ADR |
| --- | --- | --- |
| G1 Seal 纳入 P2b | 纳入（spec P2.5 必须项；§6 P2 第 3/6 条依赖） | C8/C9、§6 |
| G5 chunk 切片复用 | **不做**；与 spec P2.7 字面"旧 Chunk 和 Chunk 切片复用"的偏差如实记录：旧 chunk 复用做（§4.3），切片不做（会破坏规范形态与历史无关性），"避免小编辑全文件读写"由 CDC 重同步满足 | C4、§3.4 |
| G6 CDC 参数与 profile | FastCDC 8/16/32 KiB；profile v2；v1 根拒绝、不迁移 | C1、C10、§3.5、§7 |
| G7 Seal 触发 | API / CLI 显式；进程内后台 sealer 默认关闭、可开启 | C9、§6.4 |

## 3. CDC（C1–C4）

### 3.1 算法（规范定义，实现必须逐位一致）

```text
gear[i]  = u64::from_be_bytes(BLAKE3("wcas/gear/v1\0" || u8(i))[0..8])      i = 0..255
fp       = 0
对位置 p 处的字节 b（相对当前 chunk 起点 s）：
  fp = (fp << 1).wrapping_add(gear[b])
  若 p - s + 1 <  MIN                    → 不判定（跳过判定，但 fp 照常滚动）
  若 p - s + 1 <  AVG 且 fp & MASK_S == 0 → 在 p 之后切
  若 p - s + 1 >= AVG 且 fp & MASK_L == 0 → 在 p 之后切
  若 p - s + 1 == MAX                    → 在 p 之后切
切点之后 fp = 0、s = p + 1（**每个切点重置全部状态**——§4 的前提）
MASK_S = 高 16 位为 1（归一化级别 2：log2(AVG)+2），MASK_L = 高 12 位为 1（log2(AVG)-2）
```

- 掩码取**高位**：`fp << 1` 使第 k 高位依赖最近 64 字节，高位掩码的有效窗口是 64 字节；低位掩码只看
  最近十几个字节，切点质量差；
- "跳过 MIN 之前的判定"是 FastCDC 的加速手段，但 fp 必须照常滚动——否则切点会依赖实现是否跳过字节，
  两个实现得到不同结果。规范上**不允许**跳过滚动；
- 文件末尾剩余字节构成最后一个 chunk（可短于 MIN）。

测试向量（P2b-1 必须提交）：固定种子的伪随机 1 MiB、全零 1 MiB、重复 4 KiB 模式 1 MiB，各自的切点列表
与 chunk oid 写进测试，任何修改导致不一致即失败。

### 3.2 规范形态（C2，取代 ADR-005 §3.2 的规范化表）

| 条件 | 唯一合法形态 |
| --- | --- |
| `size == 0` | Empty |
| CDC 只产生 1 个 chunk 且不全零 | Blob（该 chunk 的 oid） |
| CDC 产生 ≥2 个 chunk 且存在非零 chunk | Extents（全零 chunk 不写，留空隙） |
| 所有 chunk 全零 | Empty |

解码器校验：Blob 的值长度 == size 且 size ≤ MAX；Extents 中每段 `length ≤ MAX`、`source_offset == 0`、
相邻两段的间隙必须是"被省略的全零 chunk"（解码器无法验证间隙内的切点，只验证长度上界——伪造的
间隙会被 `verify --deep --content` 的重切比对发现，见 §8）。

### 3.3 role（C3）

所有文件内容对象用 **Chunk（2）**。理由：小文件（单 chunk）与大文件中的相同 chunk 是同一个对象，跨文件
大小去重；Bytes（1）在 profile v2 中不再用于文件内容（ADR-001 允许 Bytes 承载"任意规范字节"，此处只是
不再使用）。不新增 role、不升对象 version。

### 3.4 不做切片复用（C4，偏离 spec P2.7 字面）

spec P2.7："支持局部读取、旧 Chunk 和 Chunk 切片复用，避免小编辑全文件读写"。

- **旧 Chunk 复用**：做——§4 的局部重切使未变区域的 chunk oid 原样复用；
- **Chunk 切片**（extent 引用旧 chunk 的一段）：不做。切片使同一字节内容有"新 chunk"与"旧 chunk 的
  切片"两种编码，破坏规范形态与历史无关性（ADR-005 S3），从而破坏 C6 的判据与物理去重；
- "避免小编辑全文件读写"由 CDC 的重同步性质满足：改几个字节只重切 1–3 个 chunk（§4.3）。

### 3.5 profile v2

```text
wcas/snapshot/v2;fmt=1;mask_bits=6;min_entries=4;max_page_bytes=16384;
cdc=fastcdc-nc2;cdc_min=8192;cdc_avg=16384;cdc_max=32768;gear=blake3:wcas/gear/v1;
content_role=chunk;max_object_value=16777216;name_max=255;hash=blake3;domains=inode,dir,extent
```

`block_bytes` 退场；ADR-006 的 overlay 页大小（4 KiB）与 CDC 无关，继续由 `workspace_policy` 管理。

## 4. 增量分页器（C5/C6）

### 4.1 前提

ADR-005 §4 的分页规则与 §3.1 的 CDC 有同一个结构性质：**状态在每个切点重置，切点只依赖"上一个切点之后的
输入"**。因此：

- P1：从任意一个**真实切点**开始重新切，得到的切点与从头切完全相同；
- P2：若修改之后某个新切点与旧切点落在**同一位置**，且其后再无修改，则其后的切分与旧切分相同。

（ADR-005 的字节护栏"页长 ≥ 上限即切"同样只依赖当前页内容，满足同一性质。）

### 4.2 算法（一层）

```text
输入：旧层页序列 [(first_key, oid, count)]、排序的变更流 Δ（upsert / delete）
1. 透传：页与 Δ 无交 → 输出 (first_key, oid, count) 原样（复用 oid，不读页）
2. 重切：遇到第一个受影响的页 P：从 P 的起点开始，解码 P 的条目、合并 Δ 中落在其中的变更，按规则切页；
   若当前新页未在 P 的终点处切开，继续解码下一页并入（"借"页），直到：
   新切点 == 某个旧页的终点 且 Δ 中再无 ≤ 该点的变更 → 回到 1
3. 上一层：以"本层产生的新页 + 被替换掉的旧页"作为变更流，递归，直到只剩一页（根）
```

所有树（inode 表、每个目录树、每个 extent 树）共用这一个函数。

### 4.3 文件内容的局部重切

文件内容是"字节流 → chunk"，结构与 §4.2 相同（键 = 偏移，切点 = chunk 边界）：

1. 脏区间 = overlay 页覆盖的区间 ∪ size 变化区间 ∪ `base_limit` 之后的区间；
2. 从第一个脏区间之前最近的**旧 chunk 边界**开始重切；读的字节 = base chunk（只读这一段涉及的旧
   chunk）⊕ overlay 页；
3. 越过该脏区间后，一旦新切点与某个旧 chunk 边界重合，跳到下一个脏区间重复 2；
4. 新 extent 列表 = 未触及的旧 extent（复用）+ 新切出的 chunk，作为 extent 树的变更流交给 §4.2。

改 1 GiB 文件中的几个字节：重切窗口通常是 1–3 个 chunk（≤ 96 KiB 读、≤ 96 KiB 新对象），加上 extent 树
一条路径、inode 表一条路径、根。spec §6 P2 第 2 条的预算：**新增物理字节 ≤ 256 KiB**（P2b-4 实测给出
真实值，预算是断言上限）。

### 4.4 判据（C6）

`incremental(base, Δ).root == full(apply(base, Δ)).root`，对任意 base 与 Δ。

- P2b-3：分页器层面的属性测试（随机键集、随机变更集、多层树）；
- P2b-4：物化器层面——参考模型随机序列（P2b-0）中**每次** snapshot 都同时跑全量 oracle 并断言相等；
- oracle 放在 `fs-workspace` 的 `test-support` feature 下，生产构建不含它。

这不违反"同一算法不得有第二份实现"（`docs/architecture.md` §2）：oracle 是规格，与 P2b-0 的参考模型
同一性质，只在测试中出现。

## 5. 增量物化器（C7，取代 ADR-006 §9.2）

### 5.1 变更集

| overlay 状态 | 对新根的作用 |
| --- | --- |
| `ov_inode` live 行 | inode 表 upsert（kind/size/times/content 重算或复用） |
| `ov_inode` 墓碑（live=0） | inode 表 delete |
| 新建 inode 被删 | 行已删除，对 base 无影响 |
| `ov_entry` 行（bind 或 whiteout） | 其 `parent` 目录树的变更流 |

**依赖的不变量**：live 的 `ov_inode` 行 ⇔ 该 inode 在视图中从根可达。由 TreeStore 契约 K1/K2
（`drop_inode` 连同内容丢弃）与 rmdir 只删空目录保证。原先由"从根遍历"按构造保证的 I2/V4，改由：

- debug 构建与测试：每次增量发布后检查 V4（inode 表键集 == 可达集）；
- `verify --deep`：对全部根检查 V4；
- P2b-0 随机序列 + oracle 相等断言。

### 5.2 每类节点

- **目录**：没有 `ov_entry` 行 → Dir oid 原样来自 base 记录（不遍历、不解码）；有 → §4.2 增量分页；
- **文件**：
  - 无页、无 `sealed_content`、size 与 `base_limit` 未变 → 复用 base content；
  - 有 `sealed_content` 且封存后无新页 → 直接用封存内容（不读字节）；
  - 其余 → §4.3 局部重切（若有 `sealed_content`，以封存内容作为"旧"层，替代 base）；
- **inode 表**：以 §5.1 的 upsert/delete 为变更流做 §4.2；
- **首次 snapshot / 空 base**：变更集 = 全部，同一个函数；不另写全量路径。

### 5.3 成本

O(变更数 × 树深) 的页读写 + 脏区间附近的 chunk I/O。KI-8 的"无变更第二次 snapshot 2.2 s"应降到毫秒级
（P2b-4 实测）。KI-4（编码器输入 O(N) 驻留）：增量分页器流式消费变更流，增量路径不再驻留全部条目；
首次全量时 inode 记录的 O(N) 驻留保留为已知限制。

## 6. Seal（C8/C9，spec P2.5）

### 6.1 overlay schema 扩展（overlay `schema_version` 2，旧 overlay 拒绝）

`ov_inode` 增加：

| 列 | 含义 |
| --- | --- |
| `version INTEGER NOT NULL` | 每次 write_at / truncate +1 |
| `sealed_content TEXT` | 封存内容：`blob:<oid>` / `extents:<oid>` / `empty`；NULL = 未封存 |
| `sealed_size INTEGER` | 封存时的 size |

### 6.2 读路径

页 > 封存内容 > base。封存内容代表**整个文件**（`base_limit` 只对未封存文件生效）。封存后的写入照常落
页（首写拷入时从封存内容读该页的当前字节）；封存后的截断：删页、`size` 变小，封存内容中超过新 size 的
部分不可见（读路径按 size 截断）——下一次 Seal 或物化会重新规范化。

### 6.3 一次 Seal

```text
1. 一致读：选出候选文件（有页，且最近一次写距今 ≥ seal_idle），记录各自 version
2. 逐文件（受预算约束、检查取消令牌）：
   用 §4.3 的编码器对该文件当前视图编码 → 规范 Content；chunk 经批量 PageSink 发布（ADR-008）
3. 每个文件一个 overlay 事务（持句柄锁）：
   若 version 未变：删除该文件全部 ov_page；写 sealed_content/sealed_size；（version 不变）
   否则：放弃（chunk 留作孤儿，GC 回收；不重试——D10）
```

- **不改变可见字节**：交换前后视图逐字节相同（交换只在 version 未变时发生，内容就是该视图的规范编码）；
- **不产生 commit / snapshot**：不触碰主库的 commit/branch/snapshot 表；
- **崩溃**：步骤 2 中崩溃 → chunk 孤儿；步骤 3 是单个 overlay 事务，原子；W4 不受影响（Seal 的交换不是
  用户操作，崩溃后视图仍是某个用户操作前缀，因为交换不改变视图）。

### 6.4 预算、取消、触发

- 预算：单次运行处理的字节上限（默认 256 MiB）、文件数上限；取消令牌在每个文件之间、每批 PageSink 之间检查；
- `seal_idle` 默认 5 s；均写入 `workspace_policy`，打开时读回校验；
- 触发：`Workspace::seal(budget, cancel)`、CLI `workspace seal <w>`；进程内后台 sealer 线程（周期 + idle
  检测）**默认关闭**，SDK 长驻进程与 P3 挂载可开启 **[G7 已定]**；
- 指标（spec §5.6 的 seal queue）：`status()` 报告"待封存文件数 / 字节数"。

### 6.5 物理去重（spec §6 P2 第 6 条）

两个 workspace 写入相同内容 → 各自 Seal → 同一组 chunk oid → Store 中物理一份。验收断言 **pack 文件中的
物理字节数**，而不是 oid 相等（spec："而非只统计逻辑哈希"）。

## 7. 版本与兼容

| 对象 | 变化 | 处理 |
| --- | --- | --- |
| snapshot profile | v1 → v2 | v1 根 `UnsupportedProfile`，不迁移 **[G6 已定]** |
| overlay schema | 1 → 2（Seal 列） | 旧 overlay 拒绝；要求先 snapshot/discard（ADR-006 §14 先例） |
| workspace_policy | 新键 `seal_idle_ms`、`seal_budget_bytes` | 读回校验 |

## 8. verify

- `verify --deep`：V1–V6 不变（ADR-005 §9），对全部根执行；
- 新增 `verify --deep --content`（重，按需）：对每个 Extents/Blob 文件读出全部字节重新 CDC，断言得到的
  extent 列表与存储的逐项相同——发现"格式合法但不规范"的伪造或 bug（§3.2 解码器无法验证的部分）。

## 9. 未实现 / 推迟

| 项 | 去向 |
| --- | --- |
| chunk 切片复用 | 不做（§3.4） |
| 首次全量物化的 O(N) inode 记录驻留 | 已知限制（KI-4 剩余部分） |
| 后台 sealer 默认开启 | P3 视挂载需求再议 |
| Seal 的跨 workspace 协调（避免重复编码同一内容） | 不做；内容寻址保证结果去重，代价是重复 CPU |

## 10. 参考模型随机序列（P2b-0，ADR-006 §18-1 的落地合约）

本 ADR 的 C6 判据、Seal 的"视图不变"、ADR-008 GC 的"根不变"都以它为底座，所以合约写在这里
（architect 裁决，`manager/8dk7j722ea`；worker 准备稿的三个倾向中，前两个采纳，第三个按 M4 修正）。

| # | 合约 |
| --- | --- |
| M1 | **模型独立实现**：数据结构朴素（`BTreeMap<路径, Dir \| File(Vec<u8>)>`），不模拟 serial / overlay / whiteout；规则依据 ADR-003 §3 与 ADR-004 §4 的**文档**实现，不参照 `rules.rs` |
| M2 | **错误类别也比对**：模型给出期望类别（NotFound / AlreadyExists / NotADirectory / IsADirectory / DirectoryNotEmpty / IntoOwnSubtree / IsRoot / InvalidPath），实现返回的类别必须一致 |
| M3 | **每步全树比对**：全部路径的 stat（kind、size；不比时间戳）、全部目录的有序 read_dir、全部文件的完整字节。单步比对超过约 5 ms 时才允许改为"本步触及路径 + 每 50 步与每个 flush/snapshot/崩溃点全比对"；不抽样 |
| M4 | **崩溃按 W4 断言**：模型保存最后一次 flush 时的状态 S_f 及其后每步状态 S_f+1..S_n；崩溃重开后实现的全树状态必须**等于某个 S_k（k ≥ f）**，模型回退到 S_k 继续；一个序列可多次崩溃。**不得**断言"等于 S_f"——正确实现在进程崩溃时通常保留 flush 之后已提交的操作 |
| M5 | **覆盖统计**：全部种子中 k > f 的情形必须出现过 |
| M6 | **崩溃的造法**：进程内 drop 句柄（不 flush）只得到 k = n，保留为快速回归；真实前缀行为用子进程在随机步数经 crash-point 或 kill 退出、父进程重开核对。掉电不可测，照抄 ADR-006 §8 的声明 |
| M7 | **历史比对**：每次 snapshot/commit 记录模型树副本；之后任意时刻 `--at` 读该提交，逐字节等于副本 |
| M8 | **随卡扩展**：P2b-4 每次 snapshot 断言 `incremental == oracle`；P2b-5 插入 seal 并断言视图不变；P2b-6 插入 gc（关闭 → GC → 重开）并断言视图与全部历史不变 |
| M9 | **可复现**：失败时打印种子与步号，`WCAS_SEED=<n>` 单独重放 |

模型与实现不一致时，先按文档判定谁错；文档未写清的，提交 architect 裁决，不让模型迁就实现。

## 11. 测试清单

1. CDC 测试向量（§3.1）；重同步：1 MiB 随机数据中间改 1/16/4096 字节，变化的 chunk 数 ≤ 3；
2. 规范形态四种情形各一例；解码拒绝：Blob 超长、extent `source_offset ≠ 0`、`length > MAX`；
3. 增量分页器属性测试：随机键集（含多层树）× 随机 upsert/delete 集，`incremental == full`；
4. 物化器：P2b-0 随机序列中每次 snapshot 断言 `incremental == oracle`；V4 每次检查；
5. 1 GiB 文件改几个字节：新增物理字节 ≤ 256 KiB（报实测值）；
6. KI-8 场景：1 000 / 10 000 文件、0 / 1 / 1% 变更的 snapshot 耗时（三次重复，P50）；
7. Seal：视图不变（随机序列中插入 seal）；version 竞争 → 放弃且无可见变化；预算截断；取消；
8. 两个 workspace 相同内容 Seal 后的物理字节（pack 统计）；
9. v1 根、overlay v1 被拒绝。
