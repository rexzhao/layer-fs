# P2b-6 任务卡（离线 GC：mark/sweep、重打包、`snapshot drop`、`branch delete`）

> 状态：**已派**（2026-10-24）。依据 ADR-008 §6（P5–P9、§6.1–§6.5）、§7（verify 扩展）、§9 测试 5–7。
> 判据来源：QA 交付（`docs/process/p2b6-criteria-qa.md`，含 G1–G5 与金丝雀 K1–K10）；architect 的反例构造与
> schema 漂移 tripwire 作为**追加条款**随后并入（不阻塞开工，见 §5）。

## 0. 本卡最容易做错的三件事（先读）

1. **`pack` 行 ⇒ 文件存在不是不变量**（KI-11）：本机有外部进程删新建 pack。GC 遇到"有行无文件"必须
   **报 `MissingPack` 并让 `gc` 失败（dry-run 告警）**，**不得**通过删索引行来"修复"——那会把可追查的丢失变成静默消失。
2. **回收判据必须双向**：只验"已知根可读"会漏标（多留垃圾且掩盖根表漂移）；只验"该删的删了"会误删。
3. **测试必须能证伪**：每条判据都要有一个"故意违反 → 必红"的注入例，否则"跑绿"可能只是没接线。

## 1. 范围（ADR-008 §6）

1. **`Repo::open_exclusive`（P5）**：主库 `locking_mode=EXCLUSIVE` 并立即取写锁；再逐个独占打开
   `workspaces/*/overlay.sqlite`。任何一步失败 → `StoreInUse` / `WorkspaceInUse`，**不做任何修改**。
2. **根集合（§6.2 / P6）**：branch head 祖先链 ∪ `branch_log` 出现过的提交祖先 ∪ `snapshot` 表 ∪ 每个 workspace 的
   `base_root`、`root(base_commit)`、overlay `pending_root`、全部 `sealed_content` 引用。
3. **mark（§6.3 / P8）**：临时 SQLite 标记库（**不得放 `tmp/`**，ADR §6.3）；从每个根经 `fs-snapshot` 读取器遍历；
   已标记对象不再展开（共享子树只走一次）。
4. **sweep（§6.3）**：loose 直接删；pack 存活率 0 → 删索引行（事务）→ 删文件；存活率 < `gc_repack_threshold`
   （默认 0.5）→ **拷贝式重打包**（新 pack：tmp → fsync → rename → fsync 目录 → 一个事务切换索引 → 删旧 pack）；
   其余保留。c) 删 `packs/` 下无 `pack` 行的文件、`objects/` 下无索引行的 loose 文件、`tmp/` 全部。
5. **`snapshot drop <root>` / `branch delete <b>`（§6.5）**：`branch delete` 保留 `branch_log`（P7）。
6. **`GcReport`**：`roots / marked_objects / freed_objects / freed_bytes / packs_repacked / packs_deleted / duration`；
   `--dry-run` 只做 mark 与统计。
7. **verify --deep（§7）**：对 §6.2 的**全部根**执行 ADR-005 V1–V6（含 overlay 封存引用）。

## 2. 验收（QA 判据，逐条给结论）

### 2.1 根集合完整性（双向，缺一不可）

| 编号 | 断言 |
| --- | --- |
| G1 | **不误删**：`F ⊇ R`，每个合法根 GC 后可读（`verify --deep` + 全路径逐字节） |
| G2 | **不漏标**：`M = R`，mark 访问到的根集合 == 合法根集合 |
| G3 | 每个根 `visited(r)`（防 mark 提前返回） |
| G4 | `freed ∩ reachable = ∅`（逐对象 ID） |
| G5 | `freed_unreachable ≠ ∅`（**非真空**：证明 GC 真动手） |

**必须留"根枚举可替换"的注入缝**（否则下列反例无法构造）：

- 反例 C：把 snapshot 行从根表删掉但对象仍可达 → **G1 必红**；
- 反例 D：mark 少给一个根 → **G2 必红**；
- 反例 E：构造确定不可达对象 → **G5 必红**；
- 反例 F（本卡新增）：登记一个根但跳过其访问（访问计数 = 0）→ **G3 必红**；
- 反例 G（本卡新增）：让 `freed` 混入一个仍在 `reachable` 的对象 → **G4 必红**。

### 2.2 金丝雀矩阵（K1–K10，见 `p2b6-criteria-qa.md`）

K1 branch 直引保留；K2 仅 snapshot 引用保留；K3 仅未发布 workspace 的 base 引用保留；K4 保留期内历史 commit 保留 /
期外允许回收（两侧各一）；**K5 两根共享子树必须保留**；**K6 页面被多文件共享，删一个文件后仍保留**；
K7 完全不可达回收且 `verify --deep` 干净；**K8 `pack` 行在文件缺 → GC 不得删行，必须报 `MissingPack`**；
K9 死字节允许重打包回收、不得报 corrupt（KI-10）；K10 空 pack 一律 FAIL。

### 2.3 崩溃安全（§6.4）

五个注入点逐个：mark 中 / 删索引后删文件前 / 重打包新 pack 落盘后切换前 / 切换后删旧 pack 前 / c) 中。
每个注入点之后：重开（非独占）→ `verify --deep`（全部根）干净 → 全部根可读、逐字节等于 GC 前。

### 2.4 独占与拒绝（§6.1）

- 有任何使用者（另一进程/另一连接）时 `Repo::open_exclusive` 必须失败且**不做任何修改**（对比操作前后目录指纹）；
- GC 运行期间其他进程的 `Repo::open` 失败（不等待）。

## 3. 交付

- 分支基于最新 `origin/main`（**交卡前** `git fetch` → `rebase` → `merge-base --is-ancestor` → `git diff origin/main..HEAD -- docs/` 必须为空，四步固定）；
- 四件套 0；全量绿（若本机偶发 `MissingPack` 红，按 `docs/process/submission-workflow.md` 的口径贴失败测试名，**不许吞错**）；
- 提交说明：根集合来源逐条对照 §6.2、每类 sweep 的判据、重打包阈值两侧、`GcReport` 字段含义、与 ADR 的差异。

## 4. 边界

- 不做在线 GC、不做历史过期、不做 pack 压缩/delta、不做 loose→pack 迁移（ADR §8）。
- 不改 pack 格式与 schema（`gc_repack_threshold` 是新策略值，按 ADR-008 §4 的"策略值 vs 格式常量"判据校验范围 `[0,1]`）。

## 5. 追加条款（architect，随后并入）

architect 将提供：根集合反例构造的细节、schema 漂移 tripwire、以及"mark 阶段访问完整性"的判据形式。**开工不受阻**：
先落"根枚举可注入"的测试骨架（决定 GC 架构，事后补不了），再等他这一段并入。

## 6. 追加条款 A（interface，QA + architect 独立同构，**开工前必须满足**）

`gc(roots)` 这种签名只能构造反例 C；D/F/G **造不出来**。接口至少要能分离三件事：

```rust
gc(
    declared_roots: &[RootRef],              // 「应该有哪些根」——G2 右侧
    mark_source: impl Fn() -> Vec<RootRef>,  // mark 实际遍历的根——反例 D 的注入点
    hooks: GcHooks,                          // on_root_visited / extra_free_candidates
) -> GcReport
```

`GcReport` 必须能直接支撑 G1–G5（**不靠解析日志**）：

- `roots_declared` / `roots_visited`（G2/G3 直接比较）；
- `freed: Vec<ObjectId>`（**逐 ID**：G4 要交集，G5 要"某个确定不可达者被回收）；只报计数不够；
- `reachable_count`、`packs_repacked`、`packs_deleted`、`freed_bytes`、`duration`；
- `roots_by_kind` / `visited_by_kind`（按来源类的计数，抓"UNION 了但按 kind 分派时漏了"）；
- `gc-test-hooks` 下另给**流式回调** `visit_sink(kind, oid)`（测试收集成 `BTreeSet`），**不要**返回全量 `Vec`
  （大库会破坏 ADR §5.4 的内存判据）。

**G3 的表达**：以根 **oid 去重集合**为准，不是按行：

```
distinct_oids(visited) == distinct_oids(roots)
且 roots_by_kind[k] > 0 ⇒ visited_by_kind[k] > 0
```

## 7. 追加条款 B（architect：注入缝与 tripwire）

**前提**：所有注入缝走同一个 feature `gc-test-hooks`（默认关，照 `crash-points` 先例），且**只能"少给/跳过/多删"**，
不得替换 mark 遍历或 sweep 算法本身（否则测的是平行实现）。

| 反例 | 注入面 | 注入内容 | 必须红 |
| --- | --- | --- | --- |
| C 根来源漏掉 | 来源注册表 `const ROOT_SOURCES` 上的 `omit_source(RootSource::X)` | 去掉**某一类**来源 | G1、G2（及该类金丝雀） |
| D 少给一个根 | `omit_root(oid)` | 只去掉一个 oid | G1、G2、G3 |
| E 不可达对象 | **无需注入**（数据级：失败发布 / 被 drop 的 snapshot / 被 discard 的 workspace） | — | G5（且 G1 仍成立） |
| F 登记但跳过访问 | mark 循环内 `skip_visit(oid)` | 根仍在枚举集合、visit 不计数 | **G3**（若闭包未标记 → G2；对象被回收 → G1） |
| G freed 混入 reachable | sweep 删除集合 seam `force_free(oid)` | 把 mark 集里的 oid 强行加入删除集 | **G4**、G1；事后 `verify` 必须报 `MissingPack`（同时是 K8 的反面） |

两条硬约束：

1. **反例 C 不能用"删根表行"构造**：根行被删后回收它是**正确**的，G1 红反而是测试错。C 必须注入在"查询/来源注册"
   这一层（模拟 `roots()` 漏了一个来源），不是"数据里少了一行"。
2. **每个注入用例必须自带阳性对照**：同一 fixture 先跑**不注入**的 GC 并断言 G1–G5 全绿，再跑注入版并断言指定判据红。
   否则缝可能是空的（注入没生效、或者红在别的原因上）。

### schema 漂移 tripwire（三层）

- **T1 列级 allowlist**：测试枚举 `sqlite_master` + `pragma_table_info` 的**两个库**（主库与每个 `workspaces/*/overlay.sqlite`），
  凡列名匹配 `(^|_)(root|oid|content)($|_)` 的必须出现在测试源码的显式表里，分类为 `RootSource(..)` 或
  `NotARoot(理由)`；**新列未分类 → 红**。
- **T2 注册表 ↔ 判据映射**：每个 `RootSource` 在 T1 里至少对应一列，且**至少被一条金丝雀 K** 覆盖（K 矩阵按
  `RootSource` 索引）→ "加了来源没加测试""加了列没加来源"都会红。
- **T3 值域启发**：T1 判为 `RootSource` 的列抽样至多 N 行，断言值可解析为 ObjectId（64 hex）。
- T1 必须覆盖 **overlay**（`ov_meta.pending_root`、Seal 落地后的 `ov_inode.sealed_content`）。`sealed_content` 目前
  不存在——**卡里写死**：Seal 合入时若 T1 未分类，tripwire 必须红（这是分两次做的护栏）。

## 8. 追加条款 C（QA：反例的期望值必须独立构造）

1. **测试自己持有 ground truth**：`R_true` 由测试用**自己的知识**构造（"本轮我创建了 3 个 branch + 2 个 snapshot，
   所以 `R_true` 是这 5 个"），**不得**从实现的根枚举/`GcReport` 里取。否则注入后 `R_true` 跟着缩小，反例 C 永远绿
   （"用被测物证明被测物"，与 P2b-2b 的 `contains` 同源）。
2. **注入只发生在来源/查询层**：让 GC 只看到 `R_injected ⊂ R_true`，断言 `F ⊇ R_true` 失败。
3. **阳性对照固定三步模板**：
   1. 不注入跑一次 → 全绿；
   2. 注入跑一次 → **只有指定判据红**（其它仍绿，防"一注入全红"的假精度）；
   3. **关闭注入再跑** → 回到全绿（证明注入面真被消费、无残留）。
   第 3 步专抓"标志没被消费 / 跨用例泄漏"——本项目已踩过两次同类（`SWEEP_ONCE` 全局、`pack_read_bytes` 进程级）。
4. **`visit_sink` 的测试实现自带 `BTreeSet<(kind, oid)>`**：接口保持流式，由测试决定是否落内存。

## 9. 追加金丝雀 K5b/K5c（"共享 + 根消失"，QA 设计，2026-10-24）

比 K5 更贴近真实误删路径：K5 测**静态共享**（mark 从两个根各走一遍就过），K5b 测**共享 + 根消失**，打穿三种常见实现错误：

- 每个根各算一份可达集合、最后只留下"最后一个根"的集合（union 写成 overwrite）；
- 先按根分组回收再合并（跨组共享被误判）；
- 用"只被某个根引用"的启发式判断（共享页在根消失后看起来"只被旧根引用过"）。

### K5b `shared_page_survives_dropping_one_of_two_roots` / K5c `shared_page_is_freed_once_both_roots_are_gone`

五条要求（缺一条这组用例就可能是空的）：

1. **先证明"共享"成立**：GC 之前断言两个根指向的**中间页 oid 相同**（去重后同一对象），并记下该 oid。不先断言共享，后面的"保留"可能只是"本来就不是同一个页"——用例恒绿。
2. **配对对照**：双根都丢掉 → `freed` 必须包含该 oid、`verify --deep` 干净。两条一起才构成非真空：**只丢一个根时保留、两个都丢时回收**，才证明实现真的按可达性判断，而不是"什么都不回收"。
3. **用真实 API 制造根消失**：`snapshot drop`（**不是**直接删根表行）。与反例 C 同源——"删根行后回收它是正确的"，只有通过合法 API 让根消失，语义才是我们要测的。
4. **幸存根要逐字节读到那一页**：走 `read_view` 把内容读出来比对。只断言"对象还在"会漏掉"对象在但引用链断了"这一类误删。
5. **`verify --deep` 覆盖 snapshot 表 + workspace base**（卡里的硬要求），本组是它的第一个实战场景。

**意图备注**：这组用例同时是"GC 是否把 snapshot 表当根"的活体检测——若实现只从 branch head 出发，丢一个 snapshot 根后共享页仍会被回收。写进注释，避免后人当成 K5 的重复。

## 10. 追加验收（QA，2026-10-24）

- **I6「到达点号 == 期望点号」**：崩溃注入的老坑是"注入了但没走到那个点"——只断言"重开后 `verify` 干净"不够
  （没走到也会干净）。每个注入点必须打印**实际到达的步骤号**并与期望比对。
- **G1c「dry-run 无副作用」**：`gc --dry-run` 之后，除**标记库**外 store 内每个文件的哈希不变。它同时验证两件事：
  dry-run 真的只读；标记库位置正确（若写进 `packs/` 或 `tmp/`，哈希就会变）。

## 11. 接口新增（mark 依赖，architect 裁决：`references` 而非 visitor）

**不采用 visitor 的 `for_each_object`**，理由（architect）：visitor 由库控制下降，调用方无法说"这个 oid 已标记，别再往下走"——
若库不内部去重，共享子树被反复展开，直接违反 §6.3；若库内部去重，则"去重"被搬进 fs-snapshot，而 GC 的 mark 集才是
G2/G3 的判据本身。另外 visitor 的回调不能返回错误，而 GC 每访问一个对象要写标记库、失败必须中止；遍历递归在库内还会吃栈。

采用：

```rust
impl SnapshotReader<'_> {
    /// The objects the value `oid` **directly** references (not `oid` itself):
    /// filesystem root → inode table page; inode table page → child table pages and each
    /// inode's content root; directory page → child pages; extent node → child nodes/chunks;
    /// Empty/Blob → nothing. Pure read: same `oid` always yields the same list.
    ///
    /// **Only object ids**: a child's exact `Role` lives in the child's own envelope, so
    /// returning it here would make the caller read every child twice for nothing.
    pub fn references(&self, oid: ObjectId) -> SnapshotResult<Vec<ObjectId>>;
}
```

- **role 明确不带**（architect 终裁）：他逐条排查过"必须知道 role"的场景——GC mark 只看"是否已标记"；`verify --deep` 要的是
  父侧**期望** role（实际 role 在子对象信封里，`get` 时自然解码）；统计可由 `get`/`inode` 派生；pack 统计与 role 无关。
  唯一"免费"的 role 是被展开对象自己的，对调用方无用。
- **`references` 枚举的是对象 id，不是 inode serial**：目录页里存的是 **serial**，不是对象 oid；`serial → content root` 这条边
  出现在 **inode 表页那一层**。文档必须写清，否则很容易把"目录页引用了 serial"误当成"引用了对象"。
- **去重与展开策略留在 GC**：从每个根入队 → 弹出 oid → 已标记则不展开（§6.3 的字面算法）→ 否则写标记集并把 `references`
  的子节点入队。"共享子树只走一次"是 **mark 的性质，不是遍历器的性质**；
- 内存：每次只持有一个对象的直接引用（页扇出有界），全量集合在临时 SQLite（§5.4）；
- 错误：每个对象一次 `Result`，GC 天然可中止；损坏对象**报错**而不是静默跳过；
- 约束：不暴露页级结构（`Entry`/`Shape`/`build_tree` 仍 `pub(crate)`）；纯新增；测试要求：重复调用结果一致（纯读）、
  `Empty`/`Blob`/叶目录页返回空、损坏/未知 role 报错而不是跳过。

**ADR 记录**（待直提）：ADR-008 §6.3 末尾 + ADR-005 读取接口半句（"读取器暴露引用边，不暴露页级结构"）。

## 12b. 追加验收：`references` 的两条数据丢失面（QA，2026-10-24）

| 行 | 断言 | 为什么重要 |
| --- | --- | --- |
| **C10'** | `references(file_inode)` 必须**含内容对象**（页/块） | 若只返回目录子项、不返回文件内容，**所有文件内容都会被当成不可达** → 一次 GC 删光数据，而 `verify` 之后还"更干净"。测法：发布 3 页文件 → `references` 返回集合必须包含那 3 页 oid（用索引交叉核对，或"删掉其中一页后读文件必须失败"反证）。
| **C11'** | 两个文件共享同一页时，**各自调用都要返回该页** | 若 `references` 内部做了去重/缓存，第二个文件会"看不到"共享页；去重必须留给 mark 集（G2/G3 的判据所在）。
| **C8'** | `references` 纯读且**幂等** | 同一 oid 连调两次结果逐元素相同；调用前后 store 文件哈希不变。
| **C9'** | 解码失败**中止整个 GC** | typed 错误；`freed` 为空；`pack` 行与 pack 文件**逐字节不变**；随后 `verify` **仍报同一错误**（不是"已修复"）。

K5/K5b 另加一条：`reachable_count` 必须是**去重后**的数（"去重发生在 mark 而不是库"的可观测证据）。

## 12. 追加验收：解码失败不得被当成"没有子节点"（QA，数据丢失路径）

> `references` 遇到任何读取/解码失败**必须返回错误**，GC **必须中止**（不是跳过子树）；`MissingPack`/`CorruptPack` 走 2d 的
> typed 错误；报告体现"因存储损坏而中止"，**不得删任何对象**。

即：**一次读取失败不能变成一次静默删除**——若把损坏节点当叶子，其子对象全部变"不可达"而被回收，而且之后 `verify`
仍然干净（垃圾没了，看起来更健康）。

反例式表述（可直接作为 K8b 的用例注释）：

> 跳过 = 把损坏节点当作叶子 = 它的子树全部被判不可达 = **一次读失败变成一次静默删除**，且事后 `verify` 更"干净"。

K8 拆两例：**K8a** 发布前缺失（`pack` 行在、文件缺）；**K8b** 遍历中途缺失（"把损坏当叶子"的活体检测）。

## 13. 实现护栏（architect，2026-10-24；都是 §6.3 的字面要求，写 mark 时最容易被"先跑通"跳过）

1. **根自身必须先入标记集**：`references(root)` 不含自身 → GC 的入队循环必须"先把根 mark 进标记库，再展开"。
   否则 **G3 会假绿**：根被"访问"了但从未被标记，其直接内容靠子边才被标记，**空根 / `Empty` 根会漏**。
2. **mark 必须完整结束后才允许任何 sweep 写操作**：§6.3 是 mark → sweep 两段。判据：`sweep` 第一次删索引行/删 pack
   之前，`mark` 必须已对**全部**根跑完并成功返回。若 mark 中途遇到损坏对象（`references`/`get` 报错）→ **typed 错误
   中止，零删除、零行改写**（用目录指纹 / 行数快照证明）。否则"先删了一部分再撞到坏对象"会留下半修状态，**比不跑
   GC 更糟**。
3. **`mark` 的读路径必须与 `verify --deep` 同一套解码**：`references`/`get` 已验证身份与角色（ADR-002 §6），mark 不得
   自己解析字节。因此"根不可读"表现为存储损坏（`MissingPack`/`Corrupt*`），**不是**被当作"空子树"跳过——与 §12 同向：
   **不得用"跳过"来实现健壮**。

## 14. 追加验收 C12'：`references` 的**按 kind 边覆盖** + kind tripwire（QA，2026-10-24）

C10' 只覆盖"文件内容"一种 kind；同样的漏读可以发生在任何 kind 上。判据是**每种 kind 各构造一个"该 kind 有出边"的 fixture**，
断言 `references(oid)` **恰好**返回那些边（不是"包含"：多返回让 mark 集虚胖→少回收，少返回是数据丢失，**两端都红**）。

| 对象 kind | 应有的出边 | 漏了会怎样 |
| --- | --- | --- |
| root | inode 表根 | 整棵树不可达（K1 能抓） |
| inode 表页 | 下一层表页 | **一层子树全丢**（根仍可读，K1 未必抓得到） |
| 目录 inode | 目录页 | 目录内容不可达 |
| 目录页 | 子 inode | **子项全丢** |
| **文件 inode** | 内容页/块 | **所有文件内容全丢**（= C10'） |
| （Seal 后）`sealed_content` | 封存树 | 未来版本的同型风险 |

**kind tripwire（照 T1 的样子）**：枚举 `fs-snapshot` 能解码出的所有 kind，每个必须在 `references` 的分类表里有一条
（"返回哪些边"或"确认无出边 + 理由"）；**kind 新增而分类表没新增 → 红**。Seal 引入 `sealed_content` 时会当场现形。
fixture 仍要写，但 tripwire 保证不漏 kind。

理由：K1/K5 只覆盖"最终可读"，覆盖不到"中间层漏边但根仍可读"——而后者正是"删掉一半数据后 `verify` 仍干净"的来源。

## 15. `GcReport` 计数语义（architect，2026-10-24；写死字段含义，防 G5 被"重打包"喂饱）

- `freed_objects` = **被判为不可达而删除的 oid 数**（sweep a 删的行 / 整体删除的 pack 里的行）。**重打包不增加它**：死帧里的
  oid 本来就没有索引行，重打包只是搬运存活帧、回收死字节。
- `freed_bytes` **分成两个数**（或一个结构），不要混：
  - `freed_object_bytes`：不可达对象按索引 `length` 累计（能对应到 oid）；
  - `reclaimed_dead_bytes`：重打包回收的帧内死字节（含无行帧、空包）。
  理由：K9 的判据会含糊——"重打包腾出 10 KiB"与"删掉 3 个对象"是两种事实。
- **G5 非真空用 `freed_objects > 0` 或 `marked_objects > 0` 判，不得用 `freed_bytes > 0`**：K9 场景（只有死字节、没有不可达
  对象）应当 `freed_objects == 0` 且 `packs_repacked == 1`——这正是一条反向断言，能抓"把重打包算成删除"的实现。
- `packs_deleted`：因**存活率为 0** 而整体删除的 pack；`packs_repacked`：阈值内**拷贝式重打包**的 pack。两者**互斥**
  （同一 pack 不得同时出现在两数里），测试断言 `packs_deleted + packs_repacked <= packs_before`。

## 16. 验收硬指标与判据冻结（manager + QA，2026-10-24）

**Y 清单（22 条，必须"独立复现"，只有 R/N 即不算通过）**

| # | 断言 | # | 断言 |
| --- | --- | --- | --- |
| Y1 | G1 不误删 | Y12 | C9' 解码失败中止 |
| Y2 | G2 不漏标 | Y13 | C10' 文件内容必须入图（反证法） |
| Y3 | G5 非真空 | Y14 | C11' 共享页各自返回 |
| Y4 | K9 反向断言（`freed_objects==0 && packs_repacked==1`） | Y15 | C12' 按 kind **恰好**（六 kind 各一） |
| Y5 | K1 | Y16–Y20 | 崩溃五点 + **到达点号**（每点一条） |
| Y6 | K5 | Y21a | **反向权威**：先用原始连接 `BEGIN IMMEDIATE` 持写锁 → `gc` 在有界时间返回 typed `Busy`/`StoreInUse`，且**不删任何文件** |
| Y7 | K5b | Y21b | （窗口型，走观察窗口节）大 store 下窗口内原始连接写入：报窗口时长/采样/占比；`M=0` 且窗口 <1 ms 则写"未验证" |
| Y8 | K5c | Y22 | §15 计数语义：用**可手算 fixture** 独立算期望值，不读实现公式 |
| Y9 | K7 | | |
| Y10 | K8a | | |
| Y11 | K8b | | |

**判据冻结**：⑤ 开工后不再新增验收判据（除非发现新的**数据丢失路径**）；报告按 Y1–Y22 编号填，
`R/N` 不计入通过数（例：`Y=20 / R=5 / N=2`）。

**Y21a 的取向**（采纳 QA）：正向"GC 期间撞 `SQLITE_BUSY`"有窗口难题（0 次撞上 ≠ 通过），反向"先持锁再 GC"可确定性复现，
同样证明"权威在 SQLite"。

## 17. ⑤ 前基线（QA 只读测于 `b73f413`，2026-10-24）

```
cargo test --workspace --no-fail-fast → 299 passed / 0 failed / 6 ignored
gc_roots --list     = 14
gc_exclusive --list = 2   (其中 1 条当时 ignore；④ 完成后已摘)
四件套              = build 0 / clippy 0 / fmt 0 / doc 0
```

用途：⑤ 交上来后跑同一套对照。**新增/减少的测试数必须可解释**（`--list` 原文 + "新增可见测试 N 个"），
`gc_roots` 若从 14 变化，逐个名字对 Y1–Y22，防"多了一个测试但不覆盖任何 Y"。

**独占面现状**：`gc_exclusive` 只有 2 个用例；卡里 8 条独占必测中，有 4 条依赖 ⑤ 的真实 GC 运行态。报告须注明
"独占面共 2 用例、其中 4 条断言依赖 ⑤ 运行态"，避免读者误以为独占已全覆盖。

**转交注意**：`b73f413` 已与 main 分叉，`git diff origin/main..b73f413 -- docs/` 会显示 **`p2b6-mark-handoff.md` 被删**，
也就是说**从 `b73f413` 直接开工的人本地没有这份交接材料**。恢复工作第一件事必须是 `git fetch && git rebase origin/main`。
