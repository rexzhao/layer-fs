# P3-2 G2 合并报告（按 FileId 的读路径 + 删除侧）

- 合并范围：`p3-2/g2` 9 个提交（`96475ff` … `bdca0a2`），ff 合入 main。
- 前置：`e946188`（G1 合并报告 + 文档批次之后的卡状态）。
- 全量：**459 passed / 0 failed / 8 ignored**，`clippy --workspace --all-targets` **0**。
- 验收：Manager review 多轮 + QA 独立复跑（变异逐条实测）；所有红法各自实测。

## 1. 交付了什么

| 项 | 说明 |
| --- | --- |
| `read_at_id` / `read_dir_id` / `stat_id` / `truncate_id` | 按 FileId 的入口（与路径版同一实现，只去掉 resolve） |
| 两个真缺陷修复（读侧逼出） | `read_at_id` 曾绕过 kind 检查（读目录把页当字节返回）；`read_dir_id` 曾对已删目录返回空列表（藏起过期句柄） |
| 路径复用判据（两例） | 目录侧 + 文件侧；含"自证 /d1 已改指 d2"与"对照 d2 仍枚举 3 条" |
| **删除语义单一化** | `TreeWrite::drop_inode` **删除**，只留 `tombstone`（`live=0`，行保留）；`unlink`/`rmdir`/rename-replace 四个调用点全迁；物理回收归 B-10 |
| `remove_at(parent, name, expected_id)` | 守卫三分类（`InvalidRecord` / 幂等 `Ok` / `NotFound`）；幂等 + 计数恰一次 + rename 后按 id 删对 |
| `CreateOutcome` 扩字段（只加不减） | `binding: (FileId, Vec<u8>)` + `replaced: Option<FileId>`；18 格三列表断言不变 |
| 读放大判据（A/B 两条活断言） | A：64 KiB 读 512 B ≤ 36 KiB；B：`cost(256 KiB) ≤ cost(32 KiB) × 1.2` |
| 跨重启（④） | `close` 后重开 `Repo`，同一 FileId 仍定位同一对象 |

## 2. 判据 1（读放大）的定案过程——三次读法，最后一次才与机制一致

| 版本 | 结论 |
| --- | --- |
| `≤ 24 KiB` | **不可达成**（格式下限就是一个 32 KiB 块）——错的是界，不是实现 |
| `k = min(8, chunk 数)`（读路径缺陷） | **数值巧合**（33,976 ≈ 8 × 4,110，实为 1 个 32 KiB 块）——机制判断错 |
| **A + B（定案）** | A 绝对（一块 + 索引）；B 相对（不随文件大小增长，**与格式无关**）。同一条变异（fetch-all-extents）同时红两边 ⇒ **1 条判据、2 条断言** |

**根因（格式级，owner 已定不动）**：≤32 KiB 的文件存为单个 `Content::Blob`（`base.rs` 整对象读，之后才切片）；≥64 KiB 按 32 KiB 块取。读"整个 extent 列表"很便宜（索引页），贵的是读**每个 extent 的数据**——B-15 的修法范围因此收窄为"改粒度或接受"。

`g2_amplification.rs` 的头注释保留了三个被否形态及各自被否的理由——**失败实验记成"别再这样做"**。

## 3. 关键裁决

1. **`remove_at` 守卫三分类**（实现先行，回写卡与 ADR）：别的条目 ⇒ `InvalidRecord`；已是墓碑 ⇒ **幂等 `Ok(())`**（崩溃后的重复清理是正常路径）；仍活着 ⇒ `NotFound`。
2. **删除语义单一**：`drop_inode` 删除而非保留——两套可见性就是两套 bug 面；物理回收归 B-10（届时需另建入口，推荐 feature-gated `purge`）。
3. **tombstone 判据成对**：行还在（白盒 `live_marker`，gc-test-hooks 门控）+ 不可见（**L1 层直接对 `TreeRead`**，含"活着的 id"对照）——**断言住在它承诺的那一层**。

## 4. 未验证 / 未达标（三态口径）

| 项 | 状态 | 依据 |
| --- | --- | --- |
| 读放大的格式下限 | **通过（A/B 均绿）**；每请求成本由格式粒度决定（B-15，owner 已定不动格式） | A 33,976 ≤ 36,864；B ratio 1.031 ≤ 1.2 |
| 读放大"不触及尾部" | **未验证**（API 层不可判：`read_at_id` 是文件区间读；帧可被读两次使 pack 尺寸比对失效） | 见卡 §2 判据 1 注 |
| rename-replace 目标无句柄（FSD 契约） | **未验证（P3-4 L2 实测）** | ADR-009 §5.4 已标注；失效时改用保留语义 |
| Linux CI | **待环境** | 本机只能证"无平台门控" |

## 5. 已知缺口（进 backlog）

- **B-15** 读放大：格式粒度取舍（owner 已定不动格式；判据 B 是将来"块取成整文件"时唯一会红的断言）；
- **B-14a** W2 判据（同进程两次 `workspace_open` ⇒ typed `WorkspaceInUse`）——本卡夹具已具备，worker 下一批带上。

## 6. 给 P3-3a 的输入（本卡定下的形状）

- trait 用 `remove_at(parent, name, expected_id)`（B 形状，不封死 A 实现）；
- mock 的 M6 与核心**同一套三分类**；mock 范围 M1–M10、M12–M14（M11 故障注入记 B-16）；
- `OpenOutcome`/`RenameOutcome` 都带 `binding`/`replaced`；`size` 必须是取回时读到的值（不是缓存）。
