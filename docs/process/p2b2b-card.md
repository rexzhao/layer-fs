# P2b-2b 任务卡（C6 错误展示与错误链、覆盖率注释、过期表述清理）

> 状态：**待派**（P2b-2 合入后派给 worker；QA 为验证者）。来源：QA 覆盖率交付（`qa/kqwanuwc39`）。
> 不涉及行为变更（一项结构改动除外，见 §1）。

## 1. 范围（按此顺序交付）

1. **`source()` 结构（主项，先做）**：`fs-overlay/src/error.rs`、`fs-snapshot/src/error.rs`、
   `fs-workspace/src/error.rs` 三处 `source()` 都用了 `_ => None` 兜底。后果：新增**包裹型**变体时 Display 会因
   穷举而编译报错（好），但 `source()` 会静默变 `None`，错误链断掉且无人提醒。
   **要求**：去掉三处 `_ => None`，非包裹变体逐个列出返回 `None`（新增变体时两处 match 都强制人做决定）。
2. **30 条 Display 断言**：`OverlayError` 8 / `SnapshotError` 8 / `WorkspaceError` 14 = 30 变体，
   逐个断言 `to_string()` 与 `source()`（存在性 + 内容）。
   **形制**：`Sqlite`/`Store` 变体需要 `rusqlite`/`StoreError`，故放各 crate `src/error.rs` 的
   `#[cfg(test)] mod tests`（crate 私有可见），不做 integration test。
   **期望值来源**：QA 的实测真值表（`qa/kqwanuwc39`，探针源码在 `F:/work/qa-scratch/c6/*_probe.rs`）。
3. **过期表述清理**：P2a-7b 已删表后端，但多处仍用现在时。清 `tree_store.rs:5/14/63/130/153`、
   `inode_view.rs:6`、`rules.rs:39`；**保留** `lib.rs:21`、`model.rs:3` 的历史叙述。
4. **覆盖率注释**：`crates/fs-namespace/src/tree_store.rs` 模块头写"0% = nothing to execute, not untested"，
   并给出**可验证的实现者 → 覆盖测试**表（QA 已给模板，含 `grep -rn "impl TreeRead\|impl TreeWrite" crates/`
   作为保持更新的方法）。

## 2. 验收

- 30/30 变体的 Display 字符串与 QA 真值表逐字一致；30/30 `source()` 语义正确（包裹型 Some、其余 None）。
- 三处 `source()` **无 `_ => None`**（`grep` 为证），且新增包裹型变体时两份 match 都会编译报错。
- `grep -rn "table backend" crates/` 只剩历史叙述（`lib.rs:21`、`model.rs:3`）。
- `tree_store.rs` 注释表里的每条"驱动测试"必须真实存在且覆盖对应实现者（可 grep 核对）。
- 四件套 0；全量测试绿；本卡不改行为，**不得**顺手改语义。

## 3. 交付

- 分支基于当时的 main；不动 main、不 push；提交说明写清"结构改动 1 项 + 断言 60 条 + 文档/注释"，
  以及"为什么 `pages`/`source()` 这类断言必须钉结构而不是只看输出"。
