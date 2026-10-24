# 提交与验收流程（负责人定义，2026-10-22）

角色：**user**（负责人，最终决策）· **manager**（任务分配与验收）· **worker**（开发）· **qa**（测试）

## 流程

```
manager 派卡（mailbox，含任务说明+验收标准）
   ▼
worker 开发（特性分支 t<N>/<slug>，不断 main）
   ▼
worker 提交申请（mailbox_reply 原任务邮件：分支、提交哈希、测试输出、ADR/文档路径）
   ▼
manager review（代码 + 架构准则 docs/architecture.md + 规格 §8 提交说明 + 约束红线；不符合架构的修改退回）
   ├─ 不通过 → 退回 worker 修改（同一邮件线程）
   ▼
manager 转交 qa 测试（附：任务说明原文、验收标准、review 关注点、分支/提交）
   ├─ qa 不通过 → 退回 worker（附失败复现）
   ▼
qa 通过 → manager 批准合入
   ▼
worker 合入 main，回信报告合并提交哈希
```

## 规则

1. **任何任务卡未经 QA 通过不得合入 main**（负责人指示，2026-10-22）。
2. 转交 QA 时必须**附带任务说明**（原任务卡内容）与验收标准，QA 据此设计测试，不只测"能编译"。
3. 通信一律走 mailbox：新话题 `mailbox_send`，跟进用 `mailbox_reply`（保持线程）。
4. QA 测试范围至少包含：验收标准逐项、spec §6 对应测试矩阵、崩溃/边界/无效输入用例；性能类验收按 spec §8-6 给原始数据。
5. 争议或规格歧义：worker/qa → manager；影响规格的决策 → 负责人（user），必要时立 ADR。
6. 分支命名 `t<N>/<slug>`；main 只经本流程推进，不直接 push。

## mailbox 身份规则（负责人指示，2026-10-22）

manager 会话持有两个邮箱身份，按收件方区分使用：

| 身份 | 用途 | 对应通信 |
|---|---|---|
| manager@<project> | 项目内部工作流 | 任务分配、review、QA 转交、合入批准（worker / qa） |
| dispatch@<project> | 外部沟通 | 其他项目的询问/协作（跨项目地址形如 
ame@other-project） |

- 回复外部来件时用 s='dispatch'；内部流程一律 s='manager'。
- manager 会话需定期检查 dispatch 收件箱（mailbox_list as='dispatch'）以免漏收外部邮件。
- 身份规则发生变更时更新本节（负责人指示优先）。
## manager 对 main 的文档直提与 worktree 约定（2026-10-22）

- manager 的流程/架构文档提交经临时 worktree F:\work\layer-fs-main（基于 main）进行，不占用共享工作树。
- 该路径存在时**任何会话不得删除、prune 或改动**；manager 用毕即删。QA 的独立验证工作树请用各自独立命名（如 F:\work\qa-*），同样互不触碰。
### 会话重启后的身份恢复（经验记录，2026-10-22）

DSH 会话重启后，manager 会话的默认邮箱身份可能漂移为 dispatch（最近登录者优先）。
后果：内部邮件列表显示为空或错乱、回信以 dispatch 身份发出。恢复步骤：重启后先
mailbox_connect as='manager' 再处理任何内部邮件；若需处理外部邮件再以 s='dispatch' 操作。
### 需负责人决策事项的外发抄送（负责人指示，2026-10-22）

凡需负责人（user）参与决策的事项（规格歧义裁决、阶段边界变更、影响规格的取舍），
在直接询问负责人的同时，必须以 dispatch 身份向 qclaw@2qcuc12p 抄送一份完整自洽的
决策请求（含选项、manager 建议、不答复时的默认行为）。
### 推送到 GitHub（负责人指示，2026-10-22）

远端：origin = https://github.com/rexzhao/layer-fs（私有仓库）。
每次卡片走完"批准合入 main"后，manager 随即 git push origin main；流程/架构文档的
main 直提同样推送。特性分支默认不推（本地保留审阅历史即可）；需要时按需推。
## 角色分工（owner 指示，2026-10-23）

- **architect**：架构设计与分析归口——阶段设计、架构问题咨询、ADR 设计输入。不写产品代码、不合入、不 push；验证用 Git Bash + arch-* 前缀隔离 worktree（**一律基于特性分支，不占 `main` 检出**，否则 manager 的文档直提 worktree 无法建立）。分析产物以 `docs/reports/` 文档形式提交到 arch-* 分支，由 manager cherry-pick 进 main；scratch 测试不提交。
- **manager**：任务分配与验收（含架构合规 review），架构问题转 architect。
- **worker / qa**：职责不变（实现 / 独立验收）。

### 会话失活后的接手流程（manager 经验，2026-10-24）

会话崩溃（DSH 崩、卡死、上下文耗尽）后，**接力断点几乎总在 manager**：worker 的新会话在等卡、QA 的发现已发出但没人转发，全队静默。观察到的一次实例：worker 与 manager 会话同时卡死，QA 的 FAIL 级分诊请求悬空 4.5 小时。

接手的 manager 按以下顺序恢复（不必等任何人通知）：

1. **git**：`git log --oneline -15`、`git status`、`git branch -vv`、`git worktree list`（worktree 列表是发现别人在做什么的最快线索）。
2. **文档**：`docs/process/`（任务卡、本文件）、`docs/reports/known-issues.md`、相关 ADR。
3. **邮箱**：`mailbox_list` 通读最近线程，先找**未闭环的请求**（别人的提问、失败报告、待派卡）。
4. **先补接力，再做别的**：把悬空任务重派给当前在线的角色会话（卡必须自包含——新会话没有历史）。
5. **核对在途工作**：`git status` / worktree 里别人的未提交改动，避免重复实现（同一缺陷两条线同时动手很常见）。
6. 对外部（qclaw）状态汇报要在发出前核对 worktree 与 git，别把"空闲"写成结论。

规则：不因这次断链而加"FAIL 双发 + 超时升级"（owner 2026-10-24 决定），保持 manager 单点转达；可靠性靠上面这套恢复流程，而不是增加分发面。

### Worker 上下文管理（owner 指示，2026-10-23）

Worker 每完成一张卡的提交申请后、接下一张卡前，compact 一次上下文。任务卡本身是完整自包含的，compact 不会丢失必要信息。目的是防止连续多卡后上下文预算耗尽导致交付质量下降。
