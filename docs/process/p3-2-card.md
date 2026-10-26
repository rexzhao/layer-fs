# P3-2 任务卡（核心补口：G1 原子 disposition / G2 按 FileId 读路径 / G3 profile v3 / G4 条件 rename）

> 状态：**已派（v4，2026-10-25）**。已吸收：QA 判据预审三处、worker 四条前置事实、QA 的 §8 实测数字。ADR-005 v3 已 Accepted（main `4988fd6`）。依据：ADR-009 §9（G1–G4）、ADR-005 inode record v3 草案（main `101a52e`）、
> ADR-003 R3–R7、ADR-008 schema v3 的先例、P3-0c 真实 v2 夹具（main `a846a46`）。
> **本卡不需要 WinFsp SDK**；ADR-005 需先转 Accepted（architect）。

## 0. 为什么四项一起做

adapter（P3-3a）需要它们才能不是 TOCTOU、才能保证 rename 后旧句柄有效：**G1 让"存在性判定 + 动作"原子；G2 让句柄按 FileId
而不是路径工作；G4 让 `ReplaceIfExists=false` 不再 stat-then-rename；G3 让时间/属性/SD 随 commit 走**（owner 已选 v3）。

## 1. G1：按 NT disposition 的原子 open/create

在**一个 overlay 事务**内完成"存在性判定 + 动作"（ADR-009 §4.2 的 6 disposition × 存在/不存在 × 文件/目录）。

- 入口：`create_or_open(path, disposition, ...) -> Result<Opened, Failure>`；
- 语义：`CREATE_NEW` 已存在 → `NameCollision`；`OPEN` 不存在 → `LeafMissing`；`OVERWRITE_IF` 存在则截断、否则创建；
  `SUPERSEDE` 存在则**替换条目**；`OPEN_IF` 两者皆可；`OVERWRITE` 不存在 → `LeafMissing`；
- **`OPEN` 命中目录**：返回 `Opened(Directory)`，**不产生文件句柄**（6 个目录格都依赖这条）。

**判据**

1. **18 格全表**，每格断言三列：`Action`、`Outcome`、**`FileId` 期望**（`SUPERSEDE` → **新 FileId**；`OVERWRITE_IF` → **保留原 FileId**）。
   理由：两者对外的唯一差别就是 FileId，缺这一列则两格从外部无法区分（QA 预审）。表长 == 6×3，rows/columns 分开断言、缺格即红；
2. **并发 `CREATE_NEW`**：`Barrier` 同时起跑，**20–50 轮**，每轮断言"**恰好一个成功、另一个 `NameCollision`**"（不是"至少一个"），
   事后 `verify` 干净；
3. **原子性的负对照**：测试侧自己写"stat 后 create"的两步版，在同一栅栏用例下必须**出现两个成功**——否则"原子"只是声明。

## 2. G2：按 FileId 的读路径

`stat/read_at/write_at/truncate/remove` 接受 **FileId**（inode serial），不是路径。

**同批补的两个入口（2026-10-26，architect 在 P3-3a 评审时提出的缺面）**

- **`read_dir(dir_id)`**：目录句柄在 rename 之后必须能继续枚举，所以入口只能是 FileId（`fs-api` 目前只有 `read_dir(path)`）。
  判据：① rename 后用**旧 dir_id** 仍能枚举出同样集合；② 枚举顺序**稳定**（同一目录两次枚举顺序相同；顺序由折叠键 + 二级键决定，
  见 P3-3a 卡 §3）；③ 目录被 `remove` 后再枚举得到 typed 错误，不是 panic。
- **`remove_id(file_id)`**：两段删除时最后一个 `close` 调它（W7）。判据：① 幂等（再调一次不报错、无副作用）；
  ② **恰好一次**真正删除（计数 source：mock/计数后端断言调用次数）；③ rename 后仍删对了对象（按 FileId，不按打开时的路径）。

**判据**

1. **范围读只触及覆盖帧**：观测点在 pack 读取器 `read_exact_at`（不是 `Store::get` 返回处）；总读字节 == 帧长之和；
   单对象读 == 恰好一帧；**不触及 48 B 尾部**（`PACK_TAIL_BYTES`，见 §5）；
2. **计数 source + 正反两侧读数**（ADR-009 §9 的写法，不用拓扑形状）：同一 FileId 连读两次 → 每次都真正取对象（计数 +1）；
   共享子树在遍历路径上只取一次；
3. **rename 后 FileId 解析仍指向同一对象（L1 代理）**：本卡只证明"解析不受路径变化影响"；
   **真正持句柄跨 rename 属 adapter 的 L2**（P3-3a），这里写明，避免被读成"G2 证明了 Windows 句柄语义"；
4. 跨重启（同一 commit）同一 FileId 仍能定位同一对象。

**归属说明**：`delete-pending` 期间"用 FileId 继续读"属 **adapter/模型层**（P3-1 的 `delete` 模块已建模），**不在 G2**；
存储层不持久 delete-pending（与 Windows 语义一致：重启后 pending 消失）。这条写进卡以免 G2/G3 互相越界。

## 3. G3：profile v3（inode record 升级）

按 ADR-005 v3 草案：inode 表页 `fmt=2`，新增 `accessed_ns`、`changed_ns`、`attributes`、`security_len` + security bytes。

- **字段顺序与 `security_len` 上限 65536 按草案，不得自行调整**；属性只存 `READONLY/HIDDEN/SYSTEM/ARCHIVE`，
  `DIRECTORY/NORMAL` 派生、落盘即解码拒绝，非法位拒绝；
- **`Stat` 破坏性变更一次性改完**（加三个字段，不保留旧构造）；
- **v2 读取路径不得因 v3 被删**（拒绝判据与兼容读取不得打架）。

**旧根拒绝判据**

1. **v2：真实夹具**（`crates/fs-snapshot/tests/fixtures/snapshot-v2-store/`）→ `UnsupportedProfile`；README 的字节清单作外部事实；
2. **v1：只对合成的 profile 字段断言** `UnsupportedProfile`，并在卡里标注"**合成根只测字段识别，不证明历史兼容**"
   （本 build 造不出真 v1，P3-0c 已证明）；
3. **拒绝前后 `store.sqlite` + `-wal` 字节哈希不变**（`-shm` 不计）；**同一夹具连续两次都被拒**；
4. **fmt 自识别**：把 v3 根的 inode fmt 改回 1 → `UnsupportedFormat`（证明不是只靠 profile 串）。

**基线与阈值（不含阈值的"原始数据"只是报告）**

- 改动**前**先测基线（10 万 inode）：树高 / 页数 / 写放大 / 耗时；
- 阈值：**树高 ≤ 基线**；**页数 ≤ 基线 × (1 + 24/4096) + 5%**；**耗时 ≤ 基线 × 1.2**。超出即红，并作为**代价问题**上报给 owner（我转）；
- **每页 inode 数的分母要写出来**：`floor(PAGE_BYTES / inode_old)` → `floor(PAGE_BYTES / inode_new)`
  （具体两个数由 QA 预先算出填入本卡，见 §8）。

## 4. G4：带 `replace` 的 rename

`rename(from, to, replace: bool)`，判定与动作在同一 overlay 事务内。

- `replace=true`：目标存在则替换（R3）；跨类型（R4/R5）与非空目录（R6）规则不变；
- `replace=false`：目标存在即 `NameCollision`，**且不得有任何改动**。

**判据**

1. true：替换成功，目标内容为源内容、源消失；
2. false 冲突：返回 `NameCollision` **且指纹不变**——`store/` 的 `(路径, 长度, 哈希)` 清单 + `object_location` 行数 +
   目标父目录 `read_dir` 三者前后相同（"返回冲突"与"没改动"是两件事）；
3. 并发 `replace=false`：`Barrier` + 20–50 轮，每轮"一成一败"。

## 5. 顺带清理（`PACK_TAIL_BYTES`）

- `cas-store` 导出 `pub const PACK_TAIL_BYTES: u64`，文档注明"由 ADR-008 §3 定义"；**不要顺手导出 `PACK_TAIL_MAGIC`**——它当前无测试消费者，导出会变成第二个"无人引用的常量"（假绿风险）；
- 删掉 `cas-store/tests/pack.rs` 与 `tests/sync_probe.rs` 的本地 `const TAIL_BYTES: usize = 48`；
- 只留一处断言 `PACK_TAIL_BYTES == 48`。

### 5.1 落地版判据（2026-10-26，与 `crates/cas-store/tests/tail_constant.rs` 一一对应）

五条检查已从“清单里的 grep”变成**每次 `cargo test` 都跑的检查**（判据不是清单：清单会被改文案绕过）。

| # | 判据 | 说明 |
| --- | --- | --- |
| R1 | 字面量绑定 = 0 | `let/const … = 48` 之类 |
| M2 | 收窄名字版 = 0 | 名字以 `tail`/`_bytes`/`_len`/`_size` 结尾且值含数字（`0x30`、`4+4+8+32` 都算） |
| M1 | 名字白名单 | 名字含 `tail` 且值含数字 ⇒ 必须叫 `{tail, tail_bytes, tail_len, tail_size}` 之一（抓 `tail_bytes2 = 0x30`） |
| F1–F5 | 白名单 | 就地 + 带理由 + **条目数 == 命中数（双向）** + **上限即信号**（第 2 条出现时该消灭知识而不是登记） |
| — | 三级分辨率 | 裸名字 `pub const TABLE` 7 处 / 类型化形状 3 处 / 语意判据 1 处——**每个数都带自己的命令** |

三级分辨率那句原话（QA）：**每提高一级，被误计为“定义”的“提及”就少一类。**

另外两条落地约束：`// tail-allow:` 行内豁免（当前**只许 1 条**：`src/pack.rs` 的 `at + 16..at + 48` 是摘要结束位置，
不是第二份尾长）；`let tail = 4 + 4 + 8 + 32` 这类“算出 48”由 M1 抓。

**验收用语义检查，不用 `grep 48`**（`48` 可能是别的数，会假红/假绿）：

1. `grep -n 'pub use.*PACK_TAIL_BYTES' crates/cas-store/src/lib.rs` = **1 行**；
2. `grep -rn 'const TAIL_BYTES' crates/*/tests/` = **0 行**；
3. `grep -rn 'PACK_TAIL_BYTES == 48' crates/` = **1 行**，且**不是** `assert!(PACK_TAIL_BYTES > 0)` 这类弱断言；
4. `grep -rn 'PACK_TAIL_BYTES' crates/ | wc -l` = **11**（1 定义 + 1 导出 + 4 src + 5 tests）：
   逐文件 `src/pack.rs` 5、`src/lib.rs` 1、`tests/pack.rs` 4、`tests/sync_probe.rs` 1。
   **绝对数比“≥ 使用点数”硬**：少一处 = 漏改（别名常量还在，仍能编译）；多一处 = 多留副本。

## 6. 判据预审四问（QA，开工前逐条答完）

1. **可构造吗**；2. **可达吗**；3. **它会怎么红**；4. **期望独立吗**。
本卡 v2 已按这四问改过一轮；开工前若 worker 对某条有异议，回卡不自行改口径。

## 7. 交付与验收

- 分支基于当时 main；四步检查；提交说明**首行判据影响行**；
- 四件套 0；全量不回归；P3-0c 夹具用例仍绿；
- 报告：G1 的 18 格（三列）+ 并发与负对照；G2 的计数读数与"L1 代理"声明；G3 的旧根拒绝四项 + 基线原始数据与阈值对照；
  G4 的三条（含指纹）；§5 的四条语义检查；
- QA 按 §6 与逐条"怎么红"验收，`L1/L2/L3` 与"待环境/未验证"分档照 P3 骨架。

## 8. §8 已并入 §11（本节仅留指路）

原“待填（每页 inode 数 `floor(4096 / rec)`）”已删除：inode 表用的是 `max_page_bytes = 16384`（**不是** `PAGE_BYTES = 4096`），
容量为 `ceil((16384 − 311 − 6) / rec)`。**一切以 §11 为准**——若按 `floor(4096/66) = 62` 算，那是把页大小搞错了。

## 9. 前置事实与实现顺序（worker 核对 + QA 复核，2026-10-24）

> **前置**：P3-1 已在 main（`737c646`），`crates/fs-windows` 存在 ⇒ §12 的“表上移”可以开工（`git ls-tree origin/main crates/` 可验）。

1. **`PACK_TAIL_BYTES` 现在是 `pub(crate)`**（`cas-store/src/pack.rs:85` 的 `TAIL_BYTES`）。G2 判据要断言“不触及 48 B 尾部”，
   测试需要它 ⇒ **P3-2 的第一件事**就是把 `cas-store` 的公开面收敛到这里：
   - 导出 `pub const PACK_TAIL_BYTES: u64`，文档注明“由 ADR-008 §3 定义”；**不要顺手导出 `PACK_TAIL_MAGIC`**（当前无测试消费者 = 假绿风险）；
   - 断言 `PACK_TAIL_BYTES == 48` **只在** `cas-store` 一处；
   - 删掉 `tests/pack.rs` / `tests/sync_probe.rs` 的本地 `const TAIL_BYTES`；使用点必须引用常量（§5 的四条语义检查）。
2. **G2 不是从零造类型**：`fs_snapshot::SnapshotReader::inode(serial)`、`root_serial()` 已有（`read.rs:151/160`），
   `fs_namespace::FileId` 已贯穿 `materialize.rs:46`。G2 的工作是**把路径入口换成 serial 入口**，并补齐判据。
3. **G1 复用同一张语义表，不写第二份矩阵**：`fs-windows::disposition::{Disposition, Exists, Action, Outcome, TABLE, decide}`
   已存在（P3-1）。**裁决：把这张表与 `decide` 上移到核心 crate（`fs-namespace`，`fs-api` 再导出），`fs-windows` 改为 re-export**。
   理由：分层方向只能是 adapter 依赖核心（`fs-windows → core`），不能反过来；两份矩阵必然漂移。
   验收：`fs-windows` 里不再有独立的 `TABLE` 定义，只有 `pub use`；P3-1 的 18 格用例仍绿（证明搬运无行为变化）。
4. **FileId 期望列排在 G3 之前**：`SUPERSEDE → 新 FileId`、`OVERWRITE_IF → 保留` 这一列与 `Stat` 字段集合、overlay 的 serial
   分配语义互相牵连 ⇒ **先定 FileId 语义与列，再动 G3**，否则实现顺序会来回改表。

**因此实现顺序**：§9.1（常量收敛）→ G1（含表上移）→ G2（serial 入口 + 读放大）→ G4（条件 rename）→ G3（profile v3 + 基线）。

## 10. 两条字面约束（worker 建议，manager 采纳）

1. **L1 测试不得使用 adapter 的 handle 表**：G2 只证明“FileId 解析在 rename 后稳定”（L1 代理）；若 L1 借用 adapter 的 handle 表，
   等于提前“证明”了 P3-3a 的 L2 判据，变成自证。
2. **旧根拒绝的度量口径**：`-wal` 在**只读打开**时也可能被创建/截断（P3-0c 实测到 `-shm`/`-wal` 会被 SQLite 造出来）。
   度量写成：**`store.sqlite` 与“已存在的 `-wal`”的内容哈希**，并对 `-shm`/`-wal` 的**出现/消失**明确豁免——与 ADR-008 §6.1 一致。

## 11. §8 的数字（QA 实测，非估算；编码器临时插 24 B 跑完已还原）

### 每页 inode 容量

```
guard = max_page_bytes(16384) − max_entry_bytes
max_entry = name_max(255) + 8 + 32 + 8 + 8 = 311  ⇒ guard = 16073
leaf_overhead = 1 + node(1 B) + 4 = **6**（首字节 node 必须计入）
capacity(rec) = ceil((16073 − 5) / rec)
```

| 记录类型 | v2 长 | v2 每页 | v3 长 | v3 每页 | 降幅 |
| --- | --- | --- | --- | --- | --- |
| 有内容（带 32 B oid） | 66 B | **244** | 90 B | **179** | −26.6% |
| 无内容（空文件/空目录） | 34 B | **473** | 58 B | **278** | −41.2% |

**但页数只涨 +4.1%（不是容量降幅）**：`mask_bits = 6` 的哈希切分在**平均 69 条/页**处就切开，远早于字节容量 244 ⇒
**v2 的 inode 表页数由哈希切分主导，不是字节预算**。实测 99,999 inode：leaf 1448 → 1511、总页 1472 → 1533。
因此 **§3 的基线报告必须同时给 `mask_bits` 与"平均每页条数"**，否则 "+4%" 不可解释。

### 基线阈值（10 万 inode、带内容、最坏情况）

| 指标 | 基线（v2 实测） | 红 |
| --- | --- | --- |
| 树高 | 2 | **> 2** |
| leaf 页数 | 1448 | **> 1521**（+5%） |
| 总页数 | 1472 | **> 1546**（+5%） |
| inode 表字节 | ≈ 6.60 MB | **> 6.94 MB**（+5%） |
| **单页最大字节** | **16110**（= 6 + 244×66；≤16384） | **> 16384**（越界即红） |
| 编码耗时（本机 debug） | ≈0.34 s / 99,999 条 | **> 基线 × 1.2** |

**更正**：早前写的 "1 + 24/4096" 是"按 4096 页"的错觉（实际 `max_page_bytes = 16384`、且哈希切分主导），**以本节为准**。

**为什么必须有"单页最大字节"这条**：`encode.rs:106-109` 注释记载第一次 scale run 曾出现 **16440 > 16384**——装填规则真的溢出过。
所以它不是形式条款，而是回归防线：记录变大时同类 bug 会以"+24 B"的形式再现。

### 报告必须给的两个原始数

1. **v3 每页容量实测值**（应为 **179 / 278**）——不符即说明字段布局没按 ADR-005 §2.2；
2. **`mask_bits` + 分析形状**（有内容/无内容各多少条）——否则页数无法解读。

## 12. G1 的表放哪：**`fs-semantics`**（manager 裁决改判，2026-10-26；原方案 `fs-namespace` 作废）

> **改判理由（QA 用一条 `cargo tree` 抓到的）**：`fs-namespace → cas-store → rusqlite(bundled) → cc/pkg-config/vcpkg`
> 共 23 个 crate。若把表搬进 `fs-namespace`，`cargo test -p fs-windows` 就需要 **C 工具链**，且 `fs-windows` README 的
> “no FFI / no SDK / any platform” 三条声明**全部失守**（`libsqlite3-sys` 就是对 C 的 FFI）。
>
> 现方案：新建**零依赖** crate **`fs-semantics`**，只装决策词表；依赖方向 `fs-windows → fs-semantics`、
> `fs-namespace → fs-semantics`、`fs-api` 再导出。实测 `cargo tree -p fs-windows --depth 1` 从 **0 → 1**（只多这一条），
> 无 C 工具链。落地见 `8f71814`。
>
> 边界（同批裁决）：**类型跟表走**（`Disposition`/`Exists`/`Action`/`Outcome` 全进 `fs-semantics`，否则表引用它们会形成
> `fs-semantics → fs-windows` 反向依赖）；**`FileId` 不进**（存储概念，`CreateOutcome { file_id, kind, action }` 留在
> 定义 FileId 的那层）。

- **表与 `decide` 上移到 `fs-namespace`**（`fs-api` 再导出）；`fs-windows` 改为 `pub use`，**不得保留副本**；
- 依赖方向：`fs-windows → fs-namespace`（adapter 依赖核心，方向正确）；**P3-1 README 里 "no deps" 的表述同卡改成
  "依赖核心的语义表、无 FFI、无平台依赖"**（这是事实修正，不是退让）；
- `share`（NT 共享位矩阵）**留在 `fs-windows`**：它只有 adapter 用，核心不需要；
- 验收：`fs-windows` 无独立 `TABLE`（只有 re-export）；P3-1 的 18 格用例仍绿（搬运无行为变化）；`fs-namespace` 的表有
  "缺格即红"基数断言（6×3，rows/columns 分开）。
