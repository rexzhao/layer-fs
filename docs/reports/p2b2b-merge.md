# P2b-2b 合入报告（C6：错误展示与错误链、覆盖率自证、过期表述）

> 合入：main `c17e655`（ff，2026-10-24）。QA 判定 **PASS（六项逐条验证）**。

## 1. 交付

| 提交 | 内容 |
| --- | --- |
| ① | **五处** `source()` 的 `_ => None` 改穷举：`fs-overlay`、`fs-snapshot`、`fs-workspace`、**`cas-store`**、**`fs-namespace`**（`fs-api` 原是正确形制）。新变体会**编译失败**（两处 match 都必须表态）。 |
| ② | 30 变体 Display/source 断言（8+8+14）+ 四层链 `chain()` 4 个链接 + `chain().count() < 10` + `Display` 不以 `()`/`None` 结尾 + `StoreError::Corrupt` 的 `Envelope`/非 `Envelope` 两分支。**期望值取自 QA 探针实跑输出**（探针未入库）。 |
| ③ | 过期表述清理：`grep "table backend"` 从 11–14 处收敛到 7 处，且**逐条核过均为历史/缺陷叙述**。 |
| ④/⑤ | `tree_store.rs` 覆盖率注释（"0% = 没有可执行内容，不是没被测"）+ **实现者 → 驱动测试**表，并修正 `SnapshotTree` 行的引用（`verify.rs` 不经过 `read_view`，改为 `workspace_api.rs` 5 处 / `acceptance.rs` 3 处 / `reference_transitions.rs`）。 |

## 2. QA 独立验证要点

- `grep -rn "_ => None" crates/*/src/error.rs` = **0**；
- 30 变体与真值表**逐字比对 30/30 相同**（"一个字都没被改写"，这是这类卡最常见的越界）；
- `source()` = **直接内层**：包裹型 6 个 `Some(内层 Display)`，非包裹 22 个 `None`，**不递归到最内层**；
- `check_table.py`：先在建表前（① 树）**跑红**证明有效，再在本 SHA 跑绿；反查 `grep -rn "impl TreeRead\|impl TreeWrite" crates/` 的 6 个 impl 块**全部在表内**（无遗漏）。

## 3. 两处需要保留的判断（口径）

1. **`contract.rs:429`** 的 "the table backend cannot meet (its B1 defect)" 是现在时，但主语是**已退役组件的缺陷**、作为"该测试为何存在"的理由 —— **不构成"把表后端当现存实现"**，不阻塞。
2. **包装型 `Display` 全是 `{inner}`**，因此四层链的四个消息文本相同，"链没被跳过"只能靠**条数**证明。worker 已在注释里写明。给包装层加前缀属语义变更（会让 30 条真值表全部失效），**不在本卡**。

## 4. 证据边界（本机环境，必须保留）

> 本机全量因 KI-11 的外部删除者**偶发红**（13 次观察 4 次，失败测试每次不同；单跑 20 次 0 失败，随后 6 次全量 0 红）。2b 判定基于逐项验证 + 6 次连续全量绿，**不**以"全量无条件绿"为前提；任何"重试/吞 `MissingPack`"的改动均视为**掩盖**。
>
> 为什么不写"连续 N 次全绿"：在本机会变成"等运气"，并诱导出"吞错换绿"——那会让真实删除静默通过，比红套件危险得多。
