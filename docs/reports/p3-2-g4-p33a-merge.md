# P3-2 G4 + P3-3a 合并报告

- 合并范围：`p3-2/g4` 12 个提交（`5aee89f` … `c940875`），ff 合入 main。
- 前置：`05447b8`（G2 合并报告）。
- 全量：**499 passed / 0 failed / 8 ignored**（QA 独立跑两次一致），`clippy --workspace --all-targets` **0**。
- 验收：QA 整卡验收（A 段机械 6/6、B 段 5/6 + 红法全部实测、7a 与映射单源）；三处待补均为"对照/计数缺失，不是行为错误"。

## 1. G4：条件 rename（`5aee89f`）

| 项 | 内容 |
| --- | --- |
| `rename_with(from, to, replace) -> RenameOutcome` | 存在性判定 + R3/R4/R5/R6/R9 与移动**同一个 write 闭包**；`replace=false` 不再是 stat-then-rename |
| `RenameOutcome { binding, replaced, file_id }` | `binding` 直接可交 `remove_at` 用（判据 4 强版本：binding + 源 id 删到 to）；`replaced.is_some()` iff 覆盖 |
| `NameCollision { path }` | 与 `AlreadyExists` 分开（后者报类型冲突）；`replace=false` 的判定**先于**类型规则 |
| R8 路径相同 | 写闭包**外**的 read 事务短路（no-op 无需写锁；先 resolve 保证 from 不存在报 NotFound） |
| 判据 9 的守卫 | `rename(x,x)` ⇒ `replaced=None` 且**不 retire 自己**（变异证明承重） |

**注**：`1dd6c6c` 曾是一次残提交（只有测试文件）——根因是 manager 误在 worker 工作树执行 `git checkout -- .`；amend 为 `b6d2476`（并入本批）。由此新增流程：**manager 不动他人工作树**（第 152 条）。

## 2. P3-3a：adapter 骨架 + mock（becc22f … b836938）

**`crates/fs-winfsp`**：无 FFI、无平台门控、CI 可在任意平台跑（A 段 6/6）。

| 层 | 交付 |
| --- | --- |
| `WorkspaceOps` trait | 11 方法（一次核心动作）；**不混 adapter 策略**（delete_pending/set_delete/close/share/引用计数在外） |
| mock | **"mock 就是命名空间"**：真实 `(parent,name)→child` 绑定、FileId 单调不复用、逻辑时钟 tick、disposition 走 `fs_semantics::decide` 不自判 |
| `remove_at` | **三分类**（别的条目 ⇒ 拒 / 已墓碑 ⇒ 幂等 Ok / 仍活 ⇒ NotFound），与核心同一套 |
| 句柄表 | 绑 FileId 不存路径；share 复用 `fs_windows::admit`；`close` 只**报告** `CloseEffect::Remove(id)`（删除由 adapter 调一次 remove_at）；`DELETE_ON_CLOSE` 开即 pending |
| dirent 游标 | 首读快照（接口承诺非优化）；**不去重**（A/a 并存）；marker 失效 ⇒ 空页（重复比空页更糟）；`./..` 由 adapter 合成 |
| 错误映射 | 只调用 `fs_windows` 的表（`NtStatus::` 构造数 = 0）；**≥8 个不同状态**（防全 Internal 退化）；`ALL_FAILURES` 钉 16 |
| 判据 7a | 三段式：definitions == instantiations / per_name 无 repeated / **foreign 为空**；扫描器两处对照 |

**本卡产生的新判据形态**（已进 `criteria-and-acceptance.md`）：
- **就地红法**：`close_removes` 抽成 fn + 两个错法对照 ⇒ **变异固化成断言**（每次 CI 都在反驳错法）；
- **互斥失败面**：判据 2 的三条断言各有只有它抓的错实现；
- **计数是指纹**：测试文件的用例数动了就要能解释（判据 5 事故被它救了）。

## 3. 三处待补（QA 验收提出，均为补钉子非返工）

| # | 项 | 处置 |
| --- | --- | --- |
| ① | `OpsError::InvalidRecord` 折叠（adapter 拿不到"绑定过期"信号） | **P3-3b 前置**：保留变体或映射成可区分的码；G3 当前批后做 |
| ② | N2 变异（mock 多返回一个 `.` ⇒ adapter 输出 2 个 ⇒ 红） | G3 后顺手 |
| ③ | B3(iii) 计数断言（`remove_at` 调用 == 1） | 同批 |

## 4. 未验证（三态口径）

| 项 | 状态 |
| --- | --- |
| 判据 1/2 的真后端半边 | **未验证（依赖 G2 的真后端，P3-3b）**；mock 上已判定 |
| FSD"目标有句柄不下发 Rename" | **未验证（P3-4 L2 实测）**；若推翻 ⇒ rename-replace 改用 SUPERSEDE 保留语义 |
| Linux CI | **待环境** |
| B-15 读放大 | **owner 已定不动格式**；判据 A/B 两条活断言（1 条判据 2 条断言） |

## 5. 同批携带的 G3 前置（f4baffc … c940875）

- 基线重测：`pages 1472 / largest 16110` 与 41694d0 **逐位相同**（形状复用的交叉验证）；耗时 175–185 ms 重测（判据 4 不进 CI）；
- 判据 1/2/3 活断言（`g3_budget.rs`）：判据 3 拆两条（高度超限 = 扇出 / 页数变多 = 容量），根 height 字节从**产物**读；
- `Profile::V3` 身份（描述串字面量 + BLAKE3 id + **v2 id 不变**）；`inode_fmt` 进 Profile（fmt 分配是 profile 的一部分，不是代码路径）；
- `inode_record_v3` 编解码：**字节级布局断言**（round-trip 只证对称不证布局）；**派生位不落盘**（DIRECTORY/NORMAL ⇒ Malformed）；`security_len ≤ 65536` 含等号（边界两侧各一例）；**校验写读都跑**（防"本 build 写的才合法"自证循环）。

## 6. 下一步

G3 继续（38 处构造一次提交 + fmt 矩阵 + 反半 v3 判据 → 反向拒绝三件 → Stat/SD → 基线）→ 三处待补（fs-winfsp 收尾）→ **P3-4**（真实 FFI + 挂载；SDK 已就位，`FspVersion` 冒烟通过）。
