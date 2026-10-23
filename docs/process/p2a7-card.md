# P2a-7 任务卡：API/CLI 切换到 Repo/Workspace + 删除直写路径 + schema v3

- 状态：已派发（2026-10-23，manager）
- 分支：`p2a/switch`，基于 main = `fca1d3b`
- 前置：P2a-1~6 + KI-7 已合入；worker 已 compact
- **P2a-7 分两步：7a 新 API 门面（与旧 Fs 并存），7b 切换 + 删除 + CLI**（2026-10-23，manager 裁决）

## 先读

ADR-006 全文，尤其 **§14（P1 组件退役清单）**——你提到的"逐项核对"是对的，那张清单是本卡的骨架。另外看 ADR-004 §3（fs-api 现有 API 面，需要全部重新设计）。

## 范围

### 1. fs-api 重新设计：从直写句柄到 Repo/Workspace

**删除**：
- `Fs` 类型及其全部 18 个方法（直写路径）
- `Fs` 的全部测试（semantics 16 / large_file 7 / shape 6 / holes 6 / acceptance 4 / coverage 10 + errors 13）
- `fs-namespace` 的 `SqlTree` 表后端 + `Namespace<SqlTree>` 的 `open/with_write_tx` 及 D+ root 链方法
- `content.rs` 的表后端内容方法（`replace_content`/`replace_extents`/`shape_of`/`content_state` 等——这些是 P1 直写路径的）
- **schema v3**：`namespace`/`file`/`directory_entry`/`file_extent`/`root` 表全部废弃；`file_id_allocator` 由 `scope_allocator` 接管
- `fs-namespace` 的 DDL（v2）拒绝 + v3 新 DDL（可能只剩 namespace_policy 或整体移交 fs-workspace）

**新增**：
- `Repo`：`open(store_root, mode)` → 打开 cas-store + fs-workspace 的主库 → 提供 `workspace_create/branch_create/branch_head/commit log/reset/restore/revert` 等管理操作
- `Workspace`：`open(repo, workspace_id)` → 持有 overlay + base → 提供 `read_at/write_at/truncate/stat/read_dir/mkdir/remove/rename/flush/snapshot/commit` 操作
- `Workspace` 的读写走 overlay（fs-overlay 的 OverlayStore + TreeStore trait）
- 读历史：`repo.read_view(commit_id)` → 返回只读视图（零表依赖，走 fs-snapshot 读取器）

**关于你问的测试迁移 vs 删除**：
- **迁移为 trait 契约测试**：K1–K6（已在 fs-overlay/tests/contract.rs）保持不动；TreeStore 契约（TX1–TX5）已在 fs-namespace/tests/，保留但只对 overlay 跑（表后端删除后 SqlTree 的实现文件也删）
- **删除**：fs-api/tests/ 里的直写路径测试（semantics/large_file/shape/holes/acceptance/coverage/errors）——它们测的是被删除的 `Fs` API；其中断言**语义**的部分（如"空目录返回空列表"）会在新的 Workspace API 测试里以新形式重现
- **保留**：cas-store / fs-snapshot / fs-overlay / fs-workspace 的全部测试不动
- **逐项列表写进提交说明**：每个被删除的测试文件一行"删除原因 + 语义是否在新测试中重现"

### 2. fs-cli 重新设计：git 式显式命令

```
# 项目管理
layerfs init <path>                          # 创建 Store + scope + main branch（空树 genesis commit）
layerfs branch <name>                        # branch create
layerfs branch                               # branch list
layerfs checkout <branch>                    # 切换当前 branch（workspace create from branch head）

# 工作区操作（当前 workspace 内）
layerfs put <local> <path>                   # 写入文件
layerfs get <path> <local>                   # 读取文件
layerfs ls [path]                            # 列目录
layerfs stat <path>                          # 元数据
layerfs mkdir <path>                         # 创建目录
layerfs rm <path>                            # 删除（文件或空目录）
layerfs mv <from> <to>                       # 重命名/移动

# 提交
layerfs commit -m "message"                  # snapshot + commit + branch head 推进
layerfs commit --force -m "message"          # BranchMoved 时强制覆盖
layerfs status                               # workspace 状态（base、dirty 数量等）
layerfs log [limit]                          # commit 历史
layerfs reset <commit>                       # 移动 branch head
layerfs restore <commit>                     # base_root 恢复为该 commit 的 root
layerfs revert                               # 撤销 head

# 历史
layerfs cat <commit>:<path>                  # 读历史版本的文件（走快照读取器）
layerfs ls <commit>:[path]                   # 列历史版本的目录

# 校验
layerfs verify [--deep]                      # 闭包校验 + 内部一致性
layerfs stats                                # 存储/去重统计
```

### 3. schema v3

- 主库（store.sqlite）的 `namespace` 相关表（namespace/file/directory_entry/file_extent/root/file_id_allocator）全部 DROP
- `workspace_policy.schema_version` 升为 3
- **零部署库**：v2/v1 库直接拒绝（同 P1 的版本策略），无迁移
- DDL 自校验更新

### 4. 不做

- 不做 P2a-5（批量发布优化）
- 不做 P2b（CDC/GC/pack）
- 不做 P3（WinFsp）

## 验收标准

- 四件套 + 红线全绿
- **CLI 全命令集成测试**（init→branch→put→ls→commit→log→cat→restore→revert→status→verify→stats 的完整往返）
- Workspace 读写走 overlay（非表后端）——可用 fs-overlay 的 K1–K6 契约测试间接验证
- 读历史（cat/ls）走 fs-snapshot 读取器（零表依赖）
- schema v3：旧库拒绝 + 新库创建 + DDL 自校验
- 被删除测试的逐项清单（提交说明内）
- ADR-006 §14 的退役清单逐项核对

## 约束

- 命令一律 Git Bash；不 push
- 文件 ≤999 行 / mod.rs ≤200 行
- 收尾清单五条 + 提交前第 5 条
- **上下文预算**：这张卡会删大量代码 + 写新 CLI。如果接近上限，优先交付核心（fs-api 切换 + CLI 基础命令 + schema v3），高级命令（revert/restore/rebind）可后续补

## 完成后

mailbox_reply 派发邮件：分支/哈希/测试输出/删除测试清单/ADR §14 核对结果。
