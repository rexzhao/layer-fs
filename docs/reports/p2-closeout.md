# P2 / P2b 阶段收口报告

> 决定：**owner 2026-10-24 —— P2b 按现状收口**。Seal、K5c 逐字节版、三条 R 升 Y 等作为**后续事项登记**，不放进 P2。
> 基线：`origin/main` 见本文件提交时的 HEAD。

## 1. 已交付（P2 / P2b）

| 卡 | 内容 | 验收 |
| --- | --- | --- |
| P2a-1..8 | Snapshot 格式、Store 布局、Overlay crate、API 门面、CLI 切换、验收矩阵与性能基线 | QA 终态 PASS（`c4ab1e8`） |
| P2b-0 | 参考模型（7 操作 × 40 种子 × 200 步 + transitions + 崩溃前缀断言） | 合入；抓出 2 个真缺陷（`resolve_parent_in`、白化重绑定） |
| P2b-1 | CDC 切块器（8/16/32 KiB）+ profile v2 + 物化器切换 | 合入（`a9ad424`） |
| P2b-2 | pack + 批量发布 + cas-store schema v3（ADR-008 §3–§5） | QA PASS；首次发布 1000 文件 41–52 ms（P2a 基线 7.9–9.3 s） |
| P2b-2b | C6：五处 `source()` 穷举 + 30 条 Display/source 断言 + 覆盖率自证表 | QA PASS（30/30 逐字一致） |
| P2b-2c/d/e | KI-11 定性（外部删除）+ `MissingPack` typed + 发布期 pin（ADR-008 §5.2/§7.1） | QA PASS，无未覆盖项 |
| P2b-6 | 离线 GC：根枚举（60 列 T1 黄金清单）+ mark + sweep + 拷贝式重打包 + CLI + 崩溃五点 | QA PASS（Y=19 / R=3 / FAIL=0） |

**关键数字**：全量 361 passed / 0 failed / 6 ignored；clippy 0；四件套 0。
**ADR**：ADR-001…008 全部 Accepted（ADR-008 的 §6 于 P2b-6 实现并核对）。

## 2. P2 规格项的完成情况（诚实账）

| 规格 §2 Phase 2 条目 | 状态 |
| --- | --- |
| 1 workspace 全生命周期 API | 完成 |
| 2 Overlay 视图（白化/墓碑/重命名/覆盖区间） | 完成 |
| 3 页级脏区间跟踪（重叠/乱序/EOF/truncate/稀疏零） | 完成 |
| 4 前台 read_at 合成 | 完成 |
| **5 Seal/Compact（稳定数据生成 CAS Chunk/Extent）** | **未完成 → 登记为 P3 后续事项**（owner 决定不在 P2 内做） |
| 6 CDC（8/16/32 KiB，policy 版本化） | 完成（P2b-1） |
| 7 Extent Tree + 局部读 + Chunk 复用 | 完成 |
| 8 Snapshot 原子发布 + 依赖完整性 | 完成 |
| 9 Branch/Commit（revert vs reset 区分） | 完成 |
| 10 Durable overlay（flush/fsync 与后台 Seal 分离） | flush/fsync 完成；**后台 Seal 属 §5 未完成项** |
| 11 最小正确 GC（离线） | 完成（P2b-6） |

**因此 P2 的显式未完成项只有一条：Seal/Compact（规格 §2 Phase 2 第 5、10 条的后台部分）。** 这条必须在 P3 计划里被点名，
不能让后续阶段"静默继承"。

## 3. 已知问题（KI）状态

| KI | 主题 | 状态 |
| --- | --- | --- |
| KI-1 | truncate 到精确块边界多发零长度对象 | 已修（P2-0a） |
| KI-2 / 2b | fsync 吞吐；S2 创建预算在高负载下偶发 | 记录（未修，属性能策略） |
| KI-3 | T4 的 1 GiB 数字曾标注错误 | 记录 |
| KI-4 | 快照编码器输入驻留 O(N) vs ADR-005 §8 的 O(N/64) | 记录（未修） |
| KI-5 | B1-3 write/unlink 竞争留孤儿 extent | 记录 |
| KI-6 | truncate 非页对齐丢存活字节 | 已修（P2a-6） |
| KI-7 | `busy_timeout` 条款冲突 | 已关闭（P2a-6 补充） |
| KI-8 | 物化器未复用 base 目录树 oid 与 extent | 记录（已知未优化，~1.4× P2a 的同树再发布成本） |
| KI-9 | truncate 缩容后扩展复活旧字节（R3-1） | 已修（`07a4bb1`） |
| KI-10 | `verify` 对 pack 判据过严把健康 Store 判 corrupt | 已修（P2b-2） |
| KI-11 | 并发发布下 `verify` 看到"行在文件缺" | 定性=外部删除；产品侧已做 typed `MissingPack` + 发布期 pin |

## 4. 收口时刻的验证证据

- `cargo test --workspace --no-fail-fast` → **361 passed / 0 failed / 6 ignored**（QA 在 `c7d07ac` 独立复跑）；
- `cargo clippy --workspace --all-targets` → 0 warning；
- `verify --deep` 在 GC 后干净；`gc --dry-run` 除标记库外 store 字节不变（G1c 用例）；
- 性能：首次发布 1000 文件 41–52 ms；同树无变更再发布 ≈1.4× P2a（KI-8 已知）；
- 本机环境事实：存在第三方安全软件在约 100 µs 内删除新建 `.pack`（KI-11），已用 pin 与 typed 错误隔离，**不是产品缺陷**。

## 5. 后续事项登记

见 `docs/process/backlog.md`。P2 收口时登记 7 项（Seal、K5c 逐字节、三条 R 升 Y、verify --deep 的 overlay 覆盖、
Y21b 窗口型判据、J1 内存判据、KI-4/KI-8 两项已知优化）。

## 6. 进入 P3 的前置

- **规格 §11 的开放决策**在 P1 已全部裁决（ADR-002…008），无需 owner 再决；
- **P3 的硬前置是环境**：WinFsp 已安装 + 管理员权限（安装驱动）。这属于 owner/环境侧，我已让 architect 出设计的同时列清单，
  需要 owner 动作的走 qclaw。
