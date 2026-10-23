# P2a-6 任务卡：fs-workspace crate

- 状态：已派发（2026-10-23，manager）
- 分支：`p2a/workspace`，基于 main = `7c8ec74`
- 前置：P2a-4 已合入；worker 已 compact

## 先读

ADR-006 全文（main 里已合入的版本），尤其：§2.1（主库表）、§2.3（workspace 生命周期）、§5（物化器）、§6（snapshot/commit/branch 协议）、§9（发布协议 + pending_root）、§10（BranchMoved 语义 + rebind + --force）、§11（Busy 有界等待）。

## 范围

### 1. 新 crate `crates/fs-workspace`（依赖 cas-core/cas-store/fs-namespace/fs-snapshot/fs-overlay）

**主库（store.sqlite）新表**（按 ADR-006 §2.1 的 DDL 草案）：
- `workspace_policy`（版本治理）
- `scope_allocator`（serial 段预留，每次 1024）
- `workspace`（id、branch、base_commit、base_root、状态、overlay 文件路径）
- `branch`（name、head_commit）
- `commit`（commit_id = BLAKE3、root、parent、时间、message）
- `branch_log`（reflog + GC 根 + displaced 记录）
- `snapshot`（具名/匿名 GC 根）

**Workspace 生命周期**：
- `workspace_create(store, branch)` → 打开 base（branch head 的 root）→ 创建 overlay 文件 → 注册主库行
- `workspace_open(store, workspace_id)` → 独占打开 overlay + 主库行
- `workspace_discard` → 删 overlay 文件 + 主库行
- `workspace_close` → 关闭句柄（不删数据）

**物化器（从根可达，I2）**：
- 把 workspace 有效视图（base ⊕ overlay）编码为 CAS 对象图（调用 fs-snapshot 编码器）
- 未变的目录直接复用 base 的目录树 oid（§5 物化器规则）
- **物化器只走"从根可达"遍历**，不扫全表

**Snapshot / Commit / Branch 协议**：
- `workspace_snapshot` → 物化 → 发布对象 → pending_root 两阶段 → 主库单事务（commit 行 + branch head + branch_log）
- `commit` → 同上 + BranchMoved CAS（F1 = 冲突错误，不静默覆盖）
- `--force` → parent = base_commit（非被取代 head），branch_log 记 commit-force + displaced
- `branch_create` / `branch_head` / `reset` / `restore` / `revert`（只 head，F3）
- `workspace rebind`（只允许挂到 head == base_commit 的 branch）
- **Busy 有界等待 5s**（F2，主库短事务，超时类型化 Busy；overlay 保持 0）

### 2. fs-workspace 公共 API（不改 fs-api/fs-cli——那是 P2a-7）

API 面以"CLI 显式风格（git 式）"为目标形态。

### 3. 崩溃注入（§9.4 注入点表）

- 物化过程中 kill → 恢复后 overlay 完好、主库无部分发布
- 发布事务前后 kill → 恢复后 branch 状态正确
- discard 中 kill → 恢复后可重开
- pending_root 两阶段协议的崩溃恢复

### 4. 不做

不改 fs-api/fs-cli（P2a-7）、不做批量发布优化（P2a-5）、不做 CDC/GC（P2b）。

## P2a-4 留的接缝

- `OverlayOptions.serial_first/serial_end` 换成 scope_allocator 真实预留
- `ov_meta.pending_root` / `workspace_id` 已预留为空串——接入真实值
- `flush()` 已实现，pending_root 两阶段协议是本卡的事

## 验收标准

- 四件套 + 红线全绿
- 主库表 DDL 自校验（各 crate 各自 policy）
- workspace 生命周期全矩阵测试（含异常路径）
- 物化器：同一 workspace 两次 snapshot（无变更）→ 同一个 root id
- BranchMoved：两 workspace 同 branch 同 head 先后 commit → 后者收到类型化冲突 + workspace 原样保留 + --force 可覆盖 + displaced 在 branch_log
- Busy 有界等待：主库事务持有 >5s → 等待有界后返回 Busy
- 崩溃注入按 §9.4 逐条 + 恢复断言
- ADR-006 实现一致性独立报告（同 P2a-4 模式）

## 约束

- 命令一律 Git Bash；不 push
- 文件 ≤999 行 / mod.rs ≤200 行
- 收尾清单五条 + 提交前第 5 条
- **上下文预算**：这张卡很重，接近上限时优先交付核心（主库表 + 生命周期 + CAS + 崩溃注入），物化器优化（未变目录复用 base oid）可标"已知未优化"如实报告

## 完成后

mailbox_reply 派发邮件：分支/哈希/测试输出/ADR 一致性报告路径。
