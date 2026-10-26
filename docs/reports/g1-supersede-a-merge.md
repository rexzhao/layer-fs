# G1 修订卡收口报告（SUPERSEDE 裁决-S 翻转）

- 合并范围：`p3-2/g1-supersede-a` 2 个提交（`892058a` + `3f5b56c`），ff 合入 main。
- 前置：`b80e372`（P3-2 G3 收口 + ADR 定稿）。
- 全量：**544 passed / 0 failed / 9 ignored**（QA 验收同轮 --no-fail-fast，无失败留档项），clippy 0。
- 验收：QA 六条全过（机械核对 / 注释清零含 6 处逐个判定 / B-8 作废 / 两向判据 / 因果断言 / 数字对账）。

## 1. 翻转了什么（裁决-S：SUPERSEDE = 同 FileId + 截断 + 属性替换）

| 层 | 翻转前 | 翻转后 |
| --- | --- | --- |
| 语义表 | `(Supersede, File) ⇒ ReplaceExisting`（新 FileId） | `⇒ TruncateExisting`（与 OVERWRITE 同格；**ReplaceExisting 变体删除，不留别名**） |
| 核心 | 解绑旧名 + 新建 inode + `replaced=Some(old)` | **同一 inode 截断到 0 + 属性替换**；`replaced` 恒 `None` |
| mock M4 | 新 id + 旧 inode 保留 live | 同 id 截断；按 `disposition == Supersede` 决定属性整体替换 |
| P3-3a 判据 2 | 旧句柄读旧字节 / 旧对象未删 / 按 id 删得掉（3 例） | `a_supersede_is_visible_through_every_handle` + `the_superseded_id_is_still_the_deletable_one`（**2 例；第二例因"没有第二个对象"失去意义，删除而非改恒真**） |
| 8 处测试 | "new FileId" 断言 | "same FileId" + 因果断言 |

## 2. 新增判据（本卡的增量）

- **ReplaceFileAttributes 两向**（表驱动 3 行）：SUPERSEDE ⇒ 属性整体替换（存储回到"什么都没设"）；OVERWRITE/OVERWRITE_IF ⇒ 属性保持（FALSE）。**只断一向的 backend 恒替换或恒保留都能溜过**——两向才钉住差异；
- **因果断言**（2 处）：SUPERSEDE 与 OVERWRITE **两格必须相等**（守语义表的结构，防将来重新开格）；`replaced=None` 处同时断 `file_id == created.file_id`（None 的原因是**条目活着**，不是字段坏了）。

## 3. 数字对账（每步有说明）

```
544（翻转前）→ 543（判据 2 三例并两例，净 −1，"reduction is expected, not a loss"）
            → 544（两向判据 +1）
```

## 4. B-8 作废与替代用例

B-8（SUPERSEDE 后旧句柄跨 GC 读）作废——同 FileId 下该语义不存在。替代用例并入 **P3-4 L2**：`open(h) → CREATE_ALWAYS 同路径 → read(h)` 读到空/新内容 + **FileIndex 不变**（QA 已确认按它验收）。

## 5. 注释清零的判定说明（QA 六处逐个判定）

grep 命中 6 处，**0 真残留**：3 处新语义陈述、1 处翻转记录（数字对账的载体，保留）、1 处 remove 竞态文档（无关）、1 处 CDC（无关）。QA 报告列全 6 处——防止下一个人 grep 到再疑一遍。

## 6. 依据链（ADR-009 §4.2，已在 main）

三重证据：传输层不给时机（fsop.c:930/697-705/958-963）+ (b) 自相矛盾（fsop.c:981 + **create.c:1265** 两层链路，带 v2.1 tag）+ 存储层表达不了（Blob 整对象替换）。owner 总原则"与真实行为一致（含前后）"为最高准则。
