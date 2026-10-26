# SUPERSEDE 方案 (a) 的影响清单（owner 已裁决 (a)，2026-10-26）

- 作者：architect，2026-10-26；基于 `origin/main` `c92a0da`
- 裁决对象：ADR-009 §4.2 SUPERSEDE 由"解除旧名字 + 新 inode（新 FileId）+ 旧 inode 保留、旧句柄读旧内容"改为
  **(a) 同一 FileId 上截断为 0 + 属性整体替换（`ReplaceFileAttributes=TRUE`），旧句柄读到覆盖后内容**。
- 依据：WinFsp 2.1 `src/dll/fsop.c`（tag `v2.1`）:929-972——`FILE_SUPERSEDE` 与 `FILE_OVERWRITE_IF` 同走
  `FspFileSystemOpCreate_FileOverwriteIf`，目标存在时 `Open` + `Overwrite(同一 FileContext, ReplaceFileAttributes=Supersede)`；
  `Create` 只在目标不存在时调用。原语义经传输层无法触发（流程 207）。QA 已复核，并补两点：(b) 的内核 FileNode 缓存死因；`ReplaceFileAttributes` 判别信号。
- 归属：属性替换/合并规则落在 **G1 修订卡**（manager 裁，一张卡改完全部）；排在 G3 收尾之后（同一函数，不并行）。
- 总原则（owner）：与普通文件系统真实行为一致，适用于以前和以后的开发内容。
- ADR 修订草案：同分支 `docs/adr/ADR-009-winfsp-frontend.md`（§4.2、§5.4、G1 行、GC 段、修订记录）、
  `docs/adr/ADR-008-pack-gc.md`（§6.1、§8）、`docs/process/backlog.md`（B-8 作废说明、B-10 来源）。

判据影响：**改变**（翻转方向：SUPERSEDE 命中既有文件时 FileId 由"新"变"同"；旧句柄由"读旧内容"变"读覆盖后内容"；`create_or_open.replaced` 由"SUPERSEDE 时 Some(old)"变"恒 None"）。

---

## 1. 代码（main 上已合入，worker 修订）

| 位置 | 现状 | (a) 下应为 |
| --- | --- | --- |
| `crates/fs-semantics/src/disposition.rs:107` `Action::ReplaceExisting` | SUPERSEDE×File 的动作 | 删除该变体，或改名为 `TruncateReplacingAttributes`；SUPERSEDE×File 与 OVERWRITE×File 动作相同、只差属性策略 |
| `crates/fs-semantics/src/disposition.rs:150-152` | `(Supersede, File) => ReplaceExisting` | `=> TruncateExisting { attributes: Replace }`（或等价表示），OVERWRITE/OVERWRITE_IF 为 `Merge` |
| `crates/fs-semantics/src/disposition.rs:235-245` `TABLE` | SUPERSEDE 行 `ReplaceExisting` | 同上 |
| `crates/fs-namespace/src/rules.rs:64-68` `CreateOutcome.replaced` 文档 | "The entry that lost the name, for SUPERSEDE" | "恒为 `None`（保留字段，只加不减）" |
| `crates/fs-namespace/src/rules.rs:159-241` SUPERSEDE 分支 | 解绑旧名字、新建 inode、`replaced=Some(old)`，旧 inode 行保留 | 与 OVERWRITE 共用截断路径（同 FileId、size=0、属性替换）；`replaced=None` |
| `crates/fs-winfsp/src/ops.rs:77,94-95` | 文档写"SUPERSEDE 后新句柄 size 0"、"replaced 仍可被自己的句柄读" | 改写：SUPERSEDE 后**所有**句柄（含旧句柄）`size==0`；`replaced` 对 create_or_open 恒 None |
| `crates/fs-winfsp/src/mock.rs:318-370` `ReplaceExisting` 分支 | 新 id + 保留旧 inode | 同一 id 截断 + 属性替换 |
| `crates/fs-winfsp/src/lib.rs:5` 模块注释 | 列 "SUPERSEDE" 为句柄语义难点 | 改为"SUPERSEDE（同 FileId 覆盖）" |
| 属性写入（G1 修订卡） | G3 引入 `attributes` 字段 | **G1 修订卡**实现：`ReplaceFileAttributes=TRUE` 整体替换、`FALSE` 合并；判别只凭该布尔（ADR-009 §4.2 (a) 判据 1、4） |

## 2. 测试（期望翻转）

| 位置 | 现断言 | (a) 下应为 |
| --- | --- | --- |
| `crates/fs-semantics/tests/disposition.rs:65-72` `supersede_replaces_the_entry_while_overwrite_if_reuses_it` | SUPERSEDE×File ⇒ `ReplaceExisting`（"new FileId"） | 改名/改断言：SUPERSEDE 与 OVERWRITE_IF **同 FileId**，只差属性策略 |
| `crates/fs-windows/tests/disposition.rs:71-74` | 同上 | 同上 |
| `crates/fs-workspace/tests/g1_create_or_open.rs:118-135` | 18 格 FileId 列：SUPERSEDE→新 | SUPERSEDE→**保留** |
| `crates/fs-workspace/tests/g1_create_or_open.rs:438-484` `the_outcome_reports_the_binding_and_the_replaced_entry` | `superseded.replaced == Some(old)`；`overwritten.file_id != superseded.file_id` | `replaced == None`；两者 file_id 均等于原 id；区别改为属性断言 |
| `crates/fs-workspace/tests/g1_truncate.rs:122-152` `supersede_unbinds_the_name_and_leaves_the_old_entry_readable` | 名字指向新 entry、旧 inode 行存活 | 名字仍指向原 id、size 0、无额外 inode 行 |
| `crates/fs-winfsp/tests/contract.rs:45-52,121` `scenario_supersede_keeps_old_object` | `replaced == Some(old)`、旧对象可读且保留 size | 重写为 `scenario_supersede_overwrites_in_place`：同 id、所有句柄读到 size 0 |
| `crates/fs-winfsp/tests/criteria_1_2.rs:131-240`（3 个测试） | 判据 2：旧句柄读旧字节；旧对象未删；按自己 id 删除 | 判据 2 重写：旧句柄读覆盖后内容、FileId 不变；后两个测试**删除**（其前提"旧对象独立存在"不再成立） |
| `crates/fs-winfsp/tests/mock_identity.rs:72-85` M4 | `ReplaceExisting` 保留旧 id live | M4 重写：同 id 覆盖 |

## 3. 卡与判据纪律（docs，manager 修订）

| 位置 | 需改 |
| --- | --- |
| `docs/process/p3-2-card.md:18,23,339` | 18 格 FileId 列 `SUPERSEDE → 新 FileId` 改为 `→ 保留`；"替换条目"措辞 |
| `docs/process/p3-3a-card.md:13,46,70-71,75,183-184,211` | §0 第 2 条、size 缓存示例（SUPERSEDE 后另一句柄 size 0 仍成立）、判据 2 + N3（"旧句柄读旧内容/旧 size"翻转）、判据 5 的 SUPERSEDE 交互、M3/M4、§10 纪律句（"保留必须真保留"的例子改为 rename-replace U1 不成立路径） |
| `docs/process/criteria-and-acceptance.md:19,118,128,132,155,417-418` | 第 118 条（"替换 = 全新空文件"）、128（`replaced.is_some() iff ReplaceExisting`）、132（"SUPERSEDE 置 tombstone ⇒ B-8 破"）、155（SUPERSEDE 三条互斥断言）、417-418（rename-replace 兜底"改用 SUPERSEDE 保留语义"→"改用保留语义"）——条目保留作为历史教训，但需加"已被 (a) 修订"的注 |
| `docs/process/p3-4-card.md`（`arch/p3-4-card` `5ddc346`） | §1 U2 改为"已裁定 (a)，L2 只验 NTFS FileIndex 一致性"；§2 `Overwrite` 仍为 P0；§9.1 关闭 |
| `docs/process/backlog.md` B-8 / B-10 | 已在本分支草拟（B-8 作废说明；B-10 来源改写） |

## 4. 历史报告（不改正文，追加注）

`docs/reports/p3-2-g1-merge.md:16-19,28,58`、`p3-2-g2-merge.md:17,57`、`p3-2-g4-p33a-merge.md:52` 描述的是当时合入的事实，**不改写**；
在文末追加一行"2026-10-26：SUPERSEDE 语义已按 ADR-009 修订 (a) 变更，见 `supersede-a-impact.md`"，避免读者把旧描述当现状。

## 5. 不受影响（核对过）

- G2 `remove_at` / tombstone、G4 rename/`replaced`（rename 的 `replaced` 语义不变）、G3 profile v3 格式、ADR-008 pack/GC 主体、P3-3a 判据 1/3/4/6/8、§3.9 路径复用。
- B-10 仍需要（`remove_at` 与 rename-replace 的 tombstone 行仍只增），只是来源变窄。

## 6. 顺带发现的一处既有不一致（与 (a) 无关，单独核对）

ADR-009 §4.2 第三列"目录（请求文件/目录）"写 SUPERSEDE/OVERWRITE/OVERWRITE_IF → `STATUS_FILE_IS_A_DIRECTORY`、OPEN_IF → "打开"；
`fs-semantics/src/disposition.rs:140-153` 则是 OPEN_IF/OVERWRITE/OVERWRITE_IF × Directory → `NotADirectory`、SUPERSEDE × Directory → `AccessDenied`。
两者的"Directory"列含义可能不同（请求文件而目标是目录 vs 请求目录）。建议在 (a) 修订卡里顺带对齐，并以 L2 NTFS 对照的返回码为准。

## 7. 若最终裁决为 (b)

本清单作废；保留现有代码与测试，但 ADR 必须写明 adapter 如何在 `Overwrite(Supersede=1)` 中把**同一 FSD 节点**的旧句柄与新 inode 区分开（这是 (b) 的未解难点），并补传输层触发路径。
