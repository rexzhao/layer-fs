# P2a-8 任务卡：P2a 验收矩阵 + 性能报告（P2a 收官）

- 状态：已派发（2026-10-23，manager）
- 分支：`p2a/acceptance`，基于 main = `0f1576f`
- 前置：P2a-1~7b 已合入；worker 已 compact

## 先读

ADR-006 §18（测试清单分配表）中标注给 P2a-8 的项；`docs/reports/p2a6-adr-conformance.md`、`docs/reports/p2a7-retirement.md`、`docs/reports/known-issues.md`（KI-1…KI-8）；P1 基线报告 `docs/reports/p1-baseline.md`（测量规则 R1–R5 沿用）。

## 范围

### 1. 验收矩阵

按 ADR-006 §18 + spec §6 P2 相关项，逐条执行并记录：

| # | 场景 | 来源 |
|---|---|---|
| A1 | 10 workspace 共用同一 base 并发乱序写、互不影响 | §18-3 / spec P2 |
| A2 | flush 后重启数据仍在（I4 已有进程级测试，这里是完整场景） | spec P2 |
| A3 | 历史提交 `read_view` 零表依赖（workspace_discard 后仍可读） | §18 / Root 审查 Q2 |
| A4 | BranchMoved 冲突 → 类型化错误 → workspace 原样保留 → rebind 或 --force | §10.3 / F1 |
| A5 | 两个 workspace 并发 snapshot 各 1000 文件均成功（Busy 5s 余量） | §11.1 |
| A6 | 同一 workspace 内可线性化（K3 系列全过） | §7.4 |
| A7 | 物化器从根可达（I2）：孤儿 inode 不进快照 | A1③ |
| A8 | schema v3 拒绝 v1/v2（含 create=true 场景） | §14 |

### 2. 性能报告（`docs/reports/p2a-baseline.md`）

测量规则沿用 P1 基线 R1–R5（≥3 次重复、全部原始 run、载荷特征标注、不取最佳单次、方差一等数据）。

| 指标 | 方法 |
|---|---|
| write_at 延迟（overlay 路径） | P50/P95/P99，1000 次操作（对比 P1 的 fsync 直写 ~3ms/对） |
| flush 耗时 | workspace 级（不同脏页数量：100/1000/10000） |
| snapshot 耗时 | vs 树规模（100/1000/10000 inode）和变更量（0/10/100） |
| commit 耗时（含发布） | 同上 |
| read_at 延迟 | overlay 热（已 flush）vs 冷（base 合成）vs read_view（纯快照） |
| **首次快照 vs 后续快照** | 全量物化的成本分布（未变部分是否 AlreadyPresent → 不写盘） |
| 存储效率 | logical_bytes / unique_canonical_bytes / stored_bytes / dedup_saved |
| 并发 snapshot 余量 | 两 workspace 并发时最长单次锁等待（KI-7 的余量证据） |

### 3. 不做

- 不做 P2b（CDC / GC / pack / 批量发布优化）
- 不做 P3（WinFsp / 原生 FS 对照）
- 性能数字只记录不优化

### 4. DoD 对照

P2a 的完成定义（由 manager 依据 spec P2 范围 + owner 决策拟定）：
- D1：多 Workspace 并发可用（A1 + A5 + A6）
- D2：flush 语义明确（A2 + I4 已有测试）
- D3：历史零表依赖可读（A3）
- D4：Branch/Commit CAS 语义完整（A4）
- D5：直写路径已退役（P2a-7b 已完成）
- D6：性能报告可复现（R1–R5 合规）
- 不满足项如实标注（哪些留给 P2b）

## 验收标准

- 验收矩阵 A1–A8 全部执行并有证据（测试名或实测数据）
- 性能报告符合 R1–R5（环境、命令、原始 run、方差）
- DoD D1–D6 逐条判定
- 四件套 + 红线全绿
- QA 将独立交叉验证（自写脚本复跑关键场景 + 性能复现）

## 约束

- Git Bash；不 push
- 重场景可用 feature 门控或 --ignored（常规回归 ≤2 分钟红线）
- 上下文预算：如果接近上限，优先交付验收矩阵 + 核心性能指标（write_at / snapshot / commit），次要指标（read_at 三路对比、存储效率）可标"待补"

## 完成后

mailbox_reply：分支/哈希/矩阵结论/报告路径/DoD 判定。
