# §18-1 参考模型随机序列：实现准备稿（P2b-0，architect 已裁决）

> **状态：三点头已裁决并落地第 ① 步。** 本文档记录裁决内容、已实现部分与后续阶段的形状。
> 裁决来源：manager 2026-10-23 转达 architect。

## 0. 裁决摘要

| 问题 | 裁决 |
| --- | --- |
| 核对粒度 | **全树比对**：全部路径 `stat`（kind/size，**不比时间戳**）+ 全部目录 `read_dir`（排序完整列表）+ 全部文件完整字节。规模护栏：单步 >5 ms 时改为"每步比触达路径 + 每 50 步/flush/snapshot/崩溃点全树" |
| 参考模型独立性 | **自实现 R1–R12**。数据结构朴素（`BTreeMap<路径, Node{Dir\|File(Vec<u8>)}>`，不模拟 serial/overlay/whiteout）；依据**文档**（ADR-003 §3 违反形式枚举表、ADR-004 §4 读写语义），**不参照 `rules.rs`**；**错误类别也要比对**；不一致时先查文档判定谁错，文档不清提交 architect |
| 崩溃插入 | **允许一个序列多次崩溃**。断言是"恢复后 = 序列的某个前缀 S_k（k ≥ f，f = 最后一次 flush 的步号）"，**不是**"= 最后一次 flush 的状态"（后者会在正确实现上失败）。用**子进程**（crash-point / kill）才能测到真正的前缀行为；进程内 `drop` 作为快速回归（k = n 的特例）。加统计断言：全部种子中 `k > f` 确实出现过。掉电语义测不到（ADR-006 §8 已声明） |

## 1. 第 ① 步：已完成（本提交）

`crates/fs-api/tests/reference_model.rs` + `crates/fs-api/tests/reference/support.rs`。

- **7 种操作**：`mkdir` / `create` / `write_at` / `truncate` / `rename` / `unlink` / `rmdir`
- **40 个种子 × 200 步**，每步：模型先判 → 双方比错误类别 → 全树逐字节核对
- **确定性 PRNG**（splitmix64，不引入 `rand`）：失败信息带 `seed=<n> step=<k> op=<label>`，可精确复现
- **`--ignored` 长soak**：10 个种子 × 1000 步
- 耗时：40×200 步全树比对约 **20 s**，在常规回归内

## 2. 第 ① 步抓到的缺陷（真实数据丢失）

**`resolve_parent_in` 未校验最后一级父目录的类型**（`fs-namespace/src/rules.rs`）。

复现：`rename("/file", "/file/child")` —— `/file` 是**文件**：

```
rename file -> own child: Ok(())
  root entries: []                     <-- 根目录空了
  stat /file:    Err(NotFound)
  stat /file/m3: Err(NotFound)          <-- 目标也没建出来
```

文件被解绑后，名字被绑进一个"父目录"其实是文件的位置，**条目从树里彻底消失**。
成因：`resolve_parent_in` 的循环在**下降前**检查当前节点是不是目录，因此最后解析出的一级从未被验证。
修复：循环结束后再校验一次 `current` 的类型（`NotADirectory`）。
回归：本测试的种子序列本身（修复前 seed=0 step=93 即失败）。

> 这类缺陷正是 P2a 期间 `truncate` 数据丢失的同类：**只在"语义空间"的某个角落出现**，
> happy path 与已有测试都覆盖不到。参考模型的价值在第一次运行就兑现了。

## 3. 过程中修正的模型（文档 vs 实现，逐条判定）

| 分歧 | 谁错 | 依据 |
| --- | --- | --- |
| `rename(x, x)` | **模型错** | 同路径重命名是合法 no-op（R8：byte-identical 是 no-op），不是冲突 |
| 文件移进自己的"子路径" | **实现错** | 见 §2 |
| 父是文件时的错误 | **模型错** | `NotADirectory`（路径解析到非目录） |
| `rename` 覆盖已存在的文件 | **模型错** | R3：文件覆盖文件合法；R4/R5 跨类型才拒绝；R6 非空目录拒绝 |

## 4. 第 ②③ 步：待实现（按裁决）

### 第 ② 步：插入 flush / snapshot / commit

- 序列中按概率插入 `flush`、`snapshot`、`commit`；它们不改树，所以全树比对不变
- 需要新增：`commit` 后 `base_commit` 变化不影响视图 ⇒ 比对继续有效
- 校验：插入后 `NothingToCommit` 只在"树与 base 相同"时出现

### 第 ③ 步：崩溃重开（按裁决的修正版）

```text
状态序列: S_0, S_1, ..., S_n            // 模型每步后都留一份快照
f = 最后一次 flush 的步号
崩溃于第 c 步 → 子进程 abort → 重开
断言: 恢复后的树 == 某个 S_k，且 f ≤ k ≤ c
找到 k 后: 模型回退到 S_k 继续跑后续步骤
统计断言: 全部种子中 k > f 出现过（证明"flush 后的操作也可能保留"确实被覆盖）
```

- **子进程**：复用 P2a-6 的 `abort` 注入点（`WCAS_WORKSPACE_CRASH_AT`），或用 kill
- **进程内 `drop`**：作为快速回归，此时 k = n
- **不测**掉电语义（ADR-006 §8 已声明本项目无法构造）

## 5. 下一步

等 architect 的 P2b ADR 与任务卡。第 ②③ 步的实现取决于 ADR 对 `flush`/`commit` 插入点的说法，
特别是"commit 之后崩溃"的 k 边界。
