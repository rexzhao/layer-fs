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
manager review（代码 + 规格 §8 提交说明 + 约束红线）
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
