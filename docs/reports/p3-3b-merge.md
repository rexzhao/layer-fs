# P3-3b 收口报告（真后端 + 读侧单点化 + 写侧补口）

- 合并范围：`p3-3b/real-backend` 5 个提交（`51583c0` … `2d8e80d`），ff 合入 main。
- 前置：`2e2e500`（G1 修订收口）。
- 全量：**554 passed / 0 failed / 9 ignored**（QA 验收同轮），clippy 0。
- 验收：QA 三项清单全过 + 一条定向变异（真后端 read 改走路径缓存 ⇒ `scenario_identity_across_rename` 红）+ 抓捕协议第三次出现带全名（§4）。

## 1. 交付了什么

| 提交 | 内容 |
| --- | --- |
| `51583c0` | 读侧 `page_format(profile, role)` 单点化：**枚举角色、无 `_` 兜底**（新角色编译器逼答）；11 处归一、残留 0；20 行审计表**转成 CI 断言**（"审计过"≠"不可能再犯"） |
| `1673ac6` | `write_at_id` / `truncate_id`：**读写 API 面对称**；两条 rename 形状对照（"让新对象占用旧路径"比"旧路径空着"强）；目录检查主动补全（"免费得到的检查"在按 id API 里是系统性风险） |
| `03bea5d` | 真后端 `ApiBackend`（fs-api，方向无环）；场景上移 `src/scenarios.rs`（定义要能被所有消费者够到）；7a 双后端 + **目录存在性断言** |
| `c7d0f8b` | 判据 3 拆分线写进模块文档（"否则缺席看起来像遗漏"）；核心半边逐条断言 |
| `2d8e80d` | ④ 属性落盘两向（跨 reopen）——**抓到核心缺陷** |

## 2. 本卡的实质产出：三个"只有跨后端/跨会话才暴露"的缺陷

| # | 缺陷 | 暴露条件 | 修复 |
| --- | --- | --- | --- |
| 1 | `ROOT` 常量不同源（fs_winfsp 1 vs 核心 0） | 第二个后端（mock 恰好对） | 常量同源（优于翻译） |
| 2 | **核心从未实现属性替换**（只有 mock 有） | 重开（持久层；同会话内存态通过） | `TreeWrite::set_attributes` + `TruncateExisting` 按 Supersede 清零 |
| 3 | 场景函数在 `tests/` 跨 crate 够不到 | 真后端实例化 | 上移 `src/scenarios.rs`（一处定义两处实例化） |

> **意外变异 + 真后端 = 免费的变异测试**——C1-d 的现场证明（三个意外 + QA 一个定向 = 四个红得对）。

## 3. 关键设计决定

- **真后端放 fs-api**（前端 trait ← api 实现，无环；P3-4 的 FFI 只调 fs-api，正好是链的中段）；
- **`InvalidRecord` 在 `failure_for` 之前单独处理**——状态映射（可折叠，206）与动作分派（不可折叠，adapter 对"过期"与"没了"动作相反）是**两个消费者**；
- **SUPERSEDE 后 `get_info` 报 0x80（NORMAL 派生）而非 0**——"替换" = 存储位清零 + 派生 NORMAL（G3 裁决的现场应用）；
- 判据 3 拆分：核心半边（remove_at 三分类）本卡、adapter 半边（DeletePending）P3-4——**拆分线写进文档**。

## 4. 抓捕协议：第三次出现，全名捕获

```
concurrent_puts_of_one_object_store_it_once（cas-store/store.rs:740）
panic: MissingPack { pack_id: "9edb73ab…" }
单跑 37/37 绿；串行全量 554/0/9 绿
```

**定性**：与 KI-11 完全同族（并发发布时 index 指向不存在的 pack）。此前两次"名字未捕获"大概率同族——现在有名字、pack_id、可复跑命令。**KI-11 的证据从"复现描述"升级为"三处独立现场 + 完整留档"**。该测试保持活断言（单跑稳定绿）；KI-11 本体两条 ignored 维持，一并挂 owner：**并发发布→MissingPack 是一个待诊断问题的三个症状面**。

## 5. 下一步

**P3-4 派发**（自写最小 FFI + 真实挂载）——最后一张卡。卡已备（`p3-4-card.md`：FFI 最小面、挂载生命周期 M1–M9、L2 NTFS 对照 12 类、U1–U14）；SDK 已装（`F:/work/winfsp-sdk/`，`FspVersion` 冒烟通过）；P3-3b 的 `ApiBackend` 正是它的调用目标。
