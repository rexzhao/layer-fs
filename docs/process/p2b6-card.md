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
