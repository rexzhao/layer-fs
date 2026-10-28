# P2b-6 预备分析：GC 根集合完整性怎么"证明"（非判据，等卡）

状态：architect 预备分析，**不是验收判据**；P2b-6 卡出后据此收敛成"哪条断言会红"的形式。
依据：ADR-008 §6.2/§6.3/§6.4、ADR-006 §15、`crates/fs-workspace/src/repo.rs::roots()`。

## 0. 为什么这条最容易"看起来对"

"GC 没删掉活对象"不能只靠"GC 后读一遍我想到的根"来证明：漏掉一个**根来源**（例如
`branch_log` 里的被覆盖提交、某个 overlay 的 `pending_root`）时，测试若只枚举自己知道的根，
仍会全绿——直到某天用户去读那条历史，对象已经没了。所以证明要覆盖**"根来源的集合"**，
而不只是"根指向的对象"。

## 1. 现状（供卡参考）

`Repo::roots()`（`repo.rs:690`）现在是：

```sql
SELECT root_oid FROM "commit" UNION SELECT root_oid FROM snapshot UNION SELECT base_root FROM workspace
```

- 它取的是**全部 commit 行**，是 ADR-008 §6.2"各 branch head 的祖先 ∪ `branch_log` 提交的祖先"的**超集**
  （commit 表只增不删），所以覆盖 §6.2 前两行；但**没有** overlay 的 `pending_root` 与 `sealed_content`；
- `sealed_content` 列目前不存在（Seal 属后续卡），所以 P2b-6 若先于 Seal 落地，根查询**无法**引用它。

## 2. 建议的证明结构（四层，从便宜到强）

### 层 1：逐根来源的"金丝雀"矩阵（核心，必做）

对 §6.2 的**每一个**根来源，各放一个"只经由该来源可达"的对象，GC 后必须存活；再把该来源单独移除，
必须只回收它独占的对象。行数 = 根来源数，失败条件清晰（"金丝雀 X 被回收"）。

| 来源 | 构造 | 移除方式 |
| --- | --- | --- |
| branch head | 非默认分支的独有文件 | `branch delete`（保留 branch_log → 仍须存活！见下一行） |
| commit 祖先 | 只在旧提交里的文件 | 不能移除（历史保留策略）→ 与 branch_log 一起测 |
| `branch_log`（force 覆盖） | force 移动后旧提交的独有文件 | 不移除：P2b 不做过期（G2），**必须存活** |
| snapshot 行 | 独立 snapshot 的独有文件 | `snapshot drop` → 必须回收 |
| workspace `base_root` | 只由 workspace 基座引用的文件 | `workspace_discard` → 必须回收 |
| workspace `base_commit` 的根 | 已 `branch delete` 但 workspace 仍基座其上 | 删除 workspace → 回收 |
| overlay `pending_root` | 崩溃注入 after-intent（commit 行未写） | 重新打开后清除 → 下次 GC 回收 |
| overlay `sealed_content` | Seal 卡之后；无 Seal 时跳过并显式记录 | discard workspace → 回收 |

失败条件是"某行 GC 后不在"或"移除后仍在"，都直接可判。注意最后两行的**方向**：`pending_root`
被回收是安全的（recovery 会放弃该意图），所以 GC 应**包含**它（保守），泄漏留给下一次 GC；
这是"宁可多留"的唯一允许方向。

### 层 2：独立闭包比对（防"roots() 少一行"）

- 测试用**逐表 SQL**自己枚举根（不调 `Repo::roots()`），再用 `fs-snapshot` 读取器独立遍历出闭包；
- 与 GC 的 mark 集比对（dry-run 需要暴露 marked oid 集，或 test-only 导出）：
  `marked == closure(roots_from_tables)`；
- 这一层能抓到"roots() 忘记某个表/列"和"遍历漏了一类对象（inode 表页/目录页/extent 树页/chunk）"。

### 层 3：schema 漂移 tripwire（防未来加列）

- 测试枚举 `sqlite_master` 的所有列，凡名字匹配 `*_root` / `*_oid` / `*content` 的，必须出现在
  "根来源白名单"里（或显式标注"非根引用"）；
- 白名单与 `roots()` 的 SQL 同源（或由同一常量生成），新增引用列时测试先红。这是"完整性"唯一
  能对未来代码生效的手段——金丝雀矩阵只能证明今天。

### 层 4：负向控制（必须被回收）

- 崩溃残留：rename 后、事务前（unindexed pack）；
- 失败/放弃的发布产物（`BranchMoved` 前的物化、`NothingToCommit` 的产物）；
- `snapshot drop` 后不再可达的对象；共享子树只回收不可达部分；
- pack 内死帧（重打包，§6.3b）。
每一条都要有"GC 前存在、GC 后不存在、且所有根仍逐字节可读"的三段断言。

## 3. 两个必须先定的时序/语义问题

1. **Seal 与 GC 的先后**：`sealed_content` 在 overlay schema 里还不存在。若 P2b-6 先落地：
   - 选项 A：根查询先不引用 sealed_content，卡里写明"Seal 合入时必须同卡更新 roots() + 金丝雀行"；
   - 选项 B：等 Seal 再开 P2b-6（GC 的"根集合完整性"一次做全）。
   我倾向 B（GC 的正确性论证依赖完整根集合，分两次做要重跑同一套证明）；若排期必须先做 A，
   则把"sealed_content 金丝雀"作为 P2b-6 的**未完成项**写进 ADR-008 §6.2，不留隐含假设。
2. **`pending_root` 与恢复的相互作用**：`repo.rs:525` 在 workspace 打开时若 commit 行缺失会清
   `pending_root`。GC 离线打开 overlay 时不该走这条恢复路径（否则"意图"在 marking 前被丢掉）；
   即使走了，方向也是安全的（对象变孤儿 → 下次 GC 回收）。判据只需一条："after-intent 注入后
   直接 GC，pending root 的闭包仍在"——不要断言 `pending_root` 一定非空。

## 4. 等卡时我会补成判据的部分

- 金丝雀矩阵的测试名与每行的三断言（存活 / 移除后回收 / 根逐字节可读）；
- 独立闭包比对需要的接口（dry-run 暴露 marked 集？仅 test-support？）；
- §6.4 四个注入点后的"重开 → verify --deep 干净 → 全部根可读"顺序与失败条件；
- 独占性：GC 期间 `Repo::open` 被拒、overlay 打开即拒、拒绝时不改动任何字节。

## 5. QA 的两条方向（2026-10-24，与 §2 合并）

QA 给的表述可直接作为"完整性"的两条半形式断言：

1. **可读集合 ⊆ 合法根集合**：任何 `verify --deep` 能读到的对象，都必须能由某个合法根（§6.2）
   经规范遍历到达。反向用法：测试构造一个"只由某根来源可达"的对象，GC 后必须仍可读。
2. **每个合法根在 mark 阶段都被访问到**：GC 的 mark 必须对 §6.2 的每个根来源至少触发一次访问；
   用逐个来源"金丝雀"（§2 层 1）证明，并记录"访问计数 ≥ 根数"，避免"roots() 里漏了一行 SQL"
   却被空集遍历掩盖。

与 §2 的关系：这两条是层 1（金丝雀）与层 2（独立闭包比对）的判据化表述；层 3（schema 漂移
tripwire）是"未来新增根来源也不漏"的机制保证，层 4 是负向控制（该回收的必须回收）。
