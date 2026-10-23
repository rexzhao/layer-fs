# P2a-3：GAT 形态与 rusqlite 事务的生命周期问题（最终结论）

- 日期：2026-10-23
- 状态：**已解决**——形态 1 成立，问题出在我的实现细节

## 结论

`TreeStore::write(&self, body: impl FnOnce(&mut Self::Write<'_>) -> R)` 与
"视图内部持有一个借用连接的 `rusqlite::Transaction`" **可以同时成立**，前提是：

1. 视图**只借用事务**，不额外借用 `self` 的任何东西；
2. `RefMut` 先绑定到局部变量，再在上面开事务；
3. 规则层**导入 `TreeRead`**，superttrait 方法才可调用。

architect 的判断全部正确。我之前"安全 Rust 下无解"的结论**是错的**。

## 三个错误及其确切原因

### 错误 1：视图持有两个毫无关系的借用

```rust
pub struct SqlWrite<'a> {
    tx: rusqlite::Transaction<'a>,
    name: &'a str,        // ← 借自 &self，与 tx 的借用毫无关系
}
```

`Self::Write<'_>` 只有一个生命周期参数，而这个视图实际上要求两个不同的存活期。
编译器无法用单一 `'t` 同时满足 → E0597 / E0599 连锁。

**修法**：`name: String`（自有）。一次 clone 相对 SQLite 往返的开销可忽略。

### 错误 2：`RefMut` 是临时值（architect 排查点 2）

```rust
let tx = self.conn.borrow_mut().transaction()?;   // RefMut 语句末尾析构
```

**修法**：先 `let mut connection = self.conn_mut();` 再 `connection.transaction()?`。
（我的实现里这一点原本就是对的，所以不是本次主因。）

### 错误 3：superttrait 方法需要 trait 在作用域内（architect 排查点）

`rules.rs` 只 `use crate::tree_store::{OpLabel, TreeStore, TreeWrite}`，缺了 `TreeRead`。
`TreeWrite: TreeRead` 是继承关系，但**调用父 trait 的方法必须显式导入父 trait**。

报错信息 `no method named 'lookup' found for &mut <S as TreeStore>::Write<'_>`
很有迷惑性——它看起来像 GAT 绑定没传播，实际是导入缺失。

**修正**：加入 `TreeRead` 到 use 列表，注释说明原因。

## 我错在哪里（方法论层面）

我做了 5 个探针，得到"4 失败 1 通过"的结论，然后宣称**安全 Rust 下无解**。

**这个推论超出了证据。** 我的探针全都复现了同一个有缺陷的结构（视图持两个借用 / 缺导入），
它们只能证明"我这个写法不通"，不能证明"这条路不通"。

正确的表述应该是："我试过的 N 种写法都失败，需要 architect 提供可对照的完整代码。"
——而这正是 manager 后来要求我做的事。

**教训**：探针数量不等于证据强度。多个探针共享同一处缺陷时，它们是同一个实验的重复，
不是多个独立证据。判断"无解"之前，应该先确认自己的最小复现**和被测实现**的差异已穷尽。

## 相关文档

- `docs/reports/p2a3-mapping.md`：trait 操作面的映射与四个缺口
- ADR-006 §7.1：形态与 TX1–TX5 契约
