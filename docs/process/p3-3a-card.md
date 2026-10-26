# P3-3a 任务卡（adapter 骨架 `fs-winfsp` + `WorkspaceOps` trait + mock 后端；**不需要 SDK**）

- 状态：**派发**（2026-10-26，manager → worker；原卡误派 architect，已改派，2026-10-26）。
- 上游：ADR-009（WinFsp 前端）§10.3 的卡序 —— P3-3「adapter（`fs-winfsp`）+ **mock 后端**：把 handle 表、两段删除、dirent 游标、错误映射接到 fs-api 的 trait 上，用 mock 跑；**仍不需要 SDK**」。
- 本卡**只做"驱动无关的那一半"**：全部逻辑跑在 trait 上，用 mock 后端验证；**不引入 WinFsp 类型、不 `LoadLibrary`、不写 `extern "C"`**。
- 下游：**P3-4**（自写最小 FFI + 真实挂载）。SDK 已于 2026-10-26 就位（`F:/work/winfsp-sdk/`，含 `inc/winfsp/winfsp.h`、`lib/winfsp-x64.lib`；运行时 2.1 已装且驱动已加载），但本卡**不使用**。

---

## 0. 为什么先做"驱动无关的一半"

1. **P3-4 的 FFI 面越小越好**：FFI 层最容易出错且最难测（需要真实挂载、真实 UAC、真实 Win32 行为）。把"所有能用普通 Rust 测的逻辑"先做出来，FFI 层就只剩**转译**（把 WinFsp 回调翻译成对 `WorkspaceOps` 的调用）。
2. **句柄语义是本阶段最容易错的语义**（旧句柄跨 rename / SUPERSEDE / 两段删除仍有效、`DELETE_ON_CLOSE`、share 位规则），而它**完全不需要驱动**就能测。
3. **本卡是 mock-only**：trait 描述的是**目标面**（P3-2 G1/G2/G3 与 P3-4 要实现的面），不是"`fs-api` 既有面"。真实 `fs-api` 后端的同一批契约测试移到 **P3-3b**（P3-2 G1/G2 合入后）；本卡不因缺面而改 `fs-api`（缺面记 §5）。

## 1. 交付物

新 crate **`crates/fs-winfsp`**（`no FFI`、无平台依赖、`cargo test -p fs-winfsp` 在任何平台可跑）。内部至少四个模块：

| 模块 | 内容 |
| --- | --- |
| `ops.rs` | **`WorkspaceOps` trait**：adapter 需要核心提供的全部操作（见 §2），**一个方法 = 一次核心调用**，不在这里做策略 |
| `handles.rs` | 句柄表：登记/查找/释放、share 位判定、引用计数、`DELETE_ON_CLOSE`、句柄与 FileId 的绑定（W3：只存 `(workspace, FileId, access, share, delete_on_close, flags)`，**不存路径**） |
| `dirent.rs` | 目录枚举游标：每句柄**首读快照** + `marker` 续读（`.`/`..` 由 adapter 合成，不来自核心）、按折叠键排序但**不做去重**（`A` 与 `a` 都要返回） |
| `error.rs` | 错误映射：`fs_api` 错误 → NTSTATUS 语义（用 `fs-windows` 的映射表，**不重复实现**；缺失的变体在这里补并**记一条**） |

以及 `mock.rs`（**`#[cfg(test)]` 或 dev-dependency**）：一个内存后端实现 `WorkspaceOps`，用于把 §3 的判据全部跑在 trait 上。

## 2. `WorkspaceOps` 的方法清单（起点，允许按证据增删）

按"一次调用 = 一个核心动作"设：

```
create_or_open(disposition, path, kind_hint) -> OpenOutcome { file_id, kind, action, size, times, attributes, binding, replaced }
// `size`/`times` 必须是**取回时从核心读到的值**，不是打开时缓存的值，也不是 adapter 自己维护的副本。
// 否则 §3.2 的“新句柄 size == 0”在“缓存旧值”的实现下会**假绿**（读到的是替换前的缓存）。

**mock 的 `remove_at` 必须实现 `expected_id` 校验**（不符 ⇒ `NotFound`）：P3-3a 只有 mock 一个后端，
若 mock 把 `expected_id` 当“提示”不校验，那么 §3.9 的路径复用红法（删错对象）**在 mock 上不会红** —— 判据就成摆设。

**为什么 `size` 不许缓存、而 dirent 允许（§3.6 首读快照）**：缓存的合法性取决于**语义要求的可见性**。
`size`/`times` 要求“读到即当前”（NT 的 `GetFileInformation` 拿的就是当前状态）；而 dirent 的快照语义
**本身就是接口承诺**（枚举期间不重排、不新增），缓存是它的实现方式，不是偏离。

**由此得一条判据写法**：**任何断言，若其期望值可以被“缓存的旧值”满足，就必须再造一个“缓存会露馅”的输入。**
- 对 `size`：先 `SUPERSEDE`，再**通过另一个句柄**读 `size`（缓存句柄会给出旧值 ⇒ 红）；
- 对 dirent：先枚举一页 → 期间新增条目 → 用同一句柄继续枚举，**新条目不得出现**——这是**期望**，不是缺陷。
  验收报告须写明“此处不出现是快照语义，不是缓存 bug”（否则两个方向都会被误判）。
read(file_id, offset, len) -> bytes
write(file_id, offset, bytes) -> written
set_size(file_id, len)
read_dir(dir_id) -> Vec<DirEntry>          // 目录句柄在 rename 后仍有效，故按 FileId
get_info(file_id) -> NodeInfo
set_times(file_id, access, write, change, create)
set_attributes(file_id, attributes)
rename(from, to, replace) -> RenameOutcome { binding, replaced }
remove_at(parent, name, expected_id)        // 最后一个句柄关闭时一次；expected_id 不符 ⇒ NotFound
flush()                                     // workspace 级
```

**不属于 trait**（adapter 策略/记账）：`delete_pending`、`set_delete`、`close(file_id)`、share 判定、`DELETE_ON_CLOSE`、句柄引用计数。两段删除的 `remove_at` 只在最后一个句柄 `cleanup` 时调用（§3 判据 3），且 `expected_id` 必须由句柄的 `binding` 与核心返回值核对。

> 判据 3/4/5 里的 `set_delete`/`close` 均指 **adapter 句柄表操作**，不是 trait 方法；trait 只有 `remove_at` 在最后一个句柄关闭时被调用一次。

**约束**：本卡**只定义 trait 与 mock 实现**；真实后端（`Repo`/`Workspace` 适配）在 **P3-3b** 实现并重跑同一份契约测试——本卡不写真实后端，也不改 `fs-api`。

## 3. 判据（每条都要能红；红法写在测试里）

1. **句柄跨 rename 有效**：`open` → `rename(from, to)` → 用**旧句柄** `read` 仍返回原内容；`get_info` 的 `file_id` 不变。
2. **句柄跨 SUPERSEDE 有效**（接 P3-2 的裁决 (a)）：`open` → `open(SUPERSEDE)` 同路径 → 旧句柄 `read` 仍读到**旧内容**且**旧句柄 `get_info.size` 不变**（旧 inode 记录只增，B-8）；新句柄 **`size == 0`** 且读到空内容；两者 `file_id` 不同。
   **两边 `size` 都要断言**（N3）：只断言"旧的可读"或只断言"新的是空"都会漏掉一半——`SUPERSEDE` 若被实现成"截断同一 inode"，旧句柄会读到空且 `size` 变 0。
   **档位：`未验证（依赖 G2）`**（旧 FileId 可读需 G2 的按 FileId 读路径；本卡只在 mock 上判）。
3. **两段删除**（W7）：`set_delete(true)` 后 (i) 新 `open` 得到 `DELETE_PENDING`；(ii) 旧句柄 `read`/`write` 仍可用；(iii) `close` 最后一个句柄时才真正 `remove`，且**只有一次**。
4. **`DELETE_ON_CLOSE`**：`open(delete_on_close=true)` → `close` ⇒ 条目消失；若期间被 rename，删除**跟随 FileId** 而不是原路径（这条是本卡最容易写错的一条）。
5. **share 位判定**：3 desired × 3 share 全组合（9 格）表驱动；`FILE_SHARE_DELETE` 与 `set_delete`/`SUPERSEDE` 的交互单列。
6. **dirent 游标（首读快照语义）**：每个目录句柄首读时缓存排序后的完整列表，后续按 `marker` 切；期间 rename/删除**不改变已取快照**，新条目不出现；大小写不同的两个条目都返回（不按折叠去重）；`.`/`..` 各恰好一次且在最前。
7. **契约测试（mock-only）+ 真实后端延后**：§3 的 1–6 本卡在 `mock` 上全部跑通并给出覆盖表；真实 `fs-api` 后端在 **P3-3b** 重跑同一批（至少 1、2、6；1/2 依赖 P3-2 G1/G2 的按 FileId 读）。本卡**不实现真实后端**；覆盖表里 mock-only 的条目必须逐条标注，不得把"mock 绿"写成"收口"。

   **7a（C1-f）每个场景函数恰好一处实例化（mock）**：出现第二处实例化而**不是** P3-3b 的真后端 ⇒ **红**。
   这条防的是"假装跑了两个后端"——mock-only 改版后最可能出现的写法（口径反转后，**"多"永远是旧口径的残留**，而它在代码里与"有一个真后端"长得一样）。
   场景函数必须是泛型的：`fn scenario<B: WorkspaceOps>(backend: &B)`；机械核对：`grep -c 'fn scenario' …` 与 `grep -c 'scenario::<' …` 对得上。

   **7b 档位词**：本卡凡"依赖 G1/G2 才可判"的判据（判据 2 的旧 FileId 可读、判据 1 的按 FileId 读），档位一律写
   **`未验证（依赖 G2）`**——与 P3-3a 清单 J0 同档。**不得写"部分通过"**（三态里没有第四态）。
8. **错误映射不重复实现**：`fs-winfsp/src/error.rs` 只能**调用** `fs_windows` 的映射（`pub use`/函数调用）；不得出现 `match err` 分支或第二份状态码表。形状查：`grep -n 'match .*err' crates/fs-winfsp/src` 为空；`STATUS_` 只出现在调用点与测试断言，不出现在映射分支。

## 3.9 「身份 vs 名字」的判别招：**路径复用**（QA，2026-10-26；对 P3-2 G2 同样适用）

**招法**：把**旧路径指向另一个对象**，再用**旧 id** 操作。

```text
create "/a" → id1
rename "/a" → "/b"
create "/a" → id2          ← 旧路径被复用，指向不同的对象
remove_at(parent_of_b, "b", expected = id1)   ← 旧绑定已随 rename 更新；用旧身份 + 当前名字
```

**期望**：`/b` 消失（删除生效）、**`/a` 仍在且仍是 id2**（未被误删）。
**红法**：① 实现若用**打开时的旧名字 `/a`** 去删，`expected_id` 核对失败 ⇒ `NotFound`，`/b` 仍在 ⇒ 第一条断言红（删除没生效，而不是误删）；② 实现若跳过 `expected_id` 直接按名字删，会删掉 `/a`(id2) ⇒ 第二条断言红，这是**数据丢失级**错误。

**mock 的前置**：mock 必须像核心一样维护真实的 `(parent, name) -> child` 绑定，并在 rename 时更新；否则"路径复用"这个构造本身不成立，§3.9 会变成空转（不是实现通过，而是测试没造出来）。

**为什么不用单纯 rename**：rename 之后旧路径**空着**，按名字的实现会**失败**（红得似是而非）；
复用之后，按名字的实现会**成功地操作错误的对象**——那种红才指向真缺陷。

**同一招用于 `read_dir(dir_id)`**：`mkdir /d1`(2 条)、`mkdir /d2`(3 条)、`rename /d1→/d1-renamed`、`rename /d2→/d1`，
再用**旧 dir1** 枚举 ⇒ 必须返回 **2** 条（dir1 的内容），不是 dir2 的 3 条。

## 4. 明确不做（本卡范围外）

- 任何 `extern "C"`、`LoadLibrary`、`winfsp.h` 的 `use`、`#[cfg(windows)]`、`#[link]`；**CI 必须能在 Linux 上 `cargo test -p fs-winfsp`**（用 `fs-windows` 现成的源码级扫描器扩展一条）；
- 真实挂载、`winfsp-tests`、L2 双进程；
- 通知/缓存/异步 IO（P3-4 之后另卡）；
- 修改 `fs-api` 的既有面（缺面记 §5）。

## 5. 发现缺面时的处理

`fs-api` 缺某个方法时：**卡里记一行**（`缺面：<方法> | 谁需要 | 现在怎么绕 | 建议`），用最小绕法继续，**不要在本卡里改 `fs-api`**。理由：P3-2 的 G2/G3 正在改 `fs-api`/`fs-namespace` 的面，本卡与它并行，改同处必有冲突。

## 6. 交付与验收

- 分支 `p3-3a/adapter-skeleton`，基于 `origin/main`；
- 提交说明首行写**判据影响行**（沿用 `docs/process/submission-workflow.md`）；
- 每次提交带：`cargo test -p fs-winfsp`、`cargo clippy --workspace --all-targets`、以及**判据 1–6 的 mock 覆盖表**（逐条标绿/未覆盖；真实后端进度由 P3-3b 记）；
- 验收方：QA 独立跑 §3 的 1–8（含变异：把句柄表的 FileId 绑定改成路径绑定 ⇒ 判据 1 必须红）。

## 7. 前置事实（派发前已核）

| 事实 | 值 |
| --- | --- |
| `fs-windows` 的 NTSTATUS 映射 | 已在 main（P3-1 合入 `737c646`），`Kind`/`Failure` 已上移 `fs-semantics`（P3-2 G1） |
| `fs-api` 的公开面 | `Repo` / `Workspace`：`create_file`/`mkdir`/`remove`/`rename`/`truncate`/`write_at`/`read_at`/`read_dir`/`stat` |
| P3-2 的 `Rules::create_or_open` | 已在 worker 分支（`CreateOutcome { file_id, kind, action }`）；本卡 `OpenOutcome` **只加不减**，保留这三项并追加 `size/times/attributes/binding/replaced`，形状以 main 上的 ADR-009 G1 为准 |
| WinFsp SDK | 已就位（`F:/work/winfsp-sdk/DYNAMIC/{inc,lib}`），本卡不用 |

## 8. 卡外补的三条判据（QA 验收清单 + manager 追加，2026-10-26）

**C1-d（判据 7 的真判据）**：同一条变异（把句柄表的 FileId 绑定改成**路径绑定**）本卡在 **mock 上必须红**；**真实后端**在 **P3-3b** 用同一变异重跑也必须红。
只让 mock 红、real 仍绿 ⇒ real 用例没覆盖该行为（最常见：只断言 `is_ok()`）。以下三种写法算**未收口**：
① real 用例 `#[ignore]`/提前 `return`；② real 只断言 `is_ok()`；③ 场景函数在两个后端参数不同（不是"同一批"）。
覆盖表必须**来自执行**；本卡只有 mock，真实后端的覆盖表由 P3-3b 提供（不得把 mock 覆盖写成"两个实现"）。

**N1 句柄不得存绝对路径**（W3 的形状判据，2026-10-26 修订）：`struct Handle` 的字段里不出现 `Path`/`PathBuf`/绝对路径语义的 `String`。

- 允许存 `(parent_id, name)` **定位键**（P3-2 G2 的 `remove_at(parent, name, expected_id)` 可能采用它，见 ADR-009 W3 修订）；
- 但该定位键只是**提示不是身份**：必须校验 `entry.child == expected_id`，不符即 `NotFound`；
- 采用它的实现必须在注释里写明**依赖 W2（workspace 排他打开）**——一旦允许并发打开者即刻失效。

机械核对两条：
```sh
grep -A20 'struct Handle' crates/fs-winfsp/src/handles.rs | grep -cE 'path\s*:\s*(String|PathBuf)'   # 0
grep -A20 'struct Handle' crates/fs-winfsp/src/handles.rs | grep -cE '(parent|name)\s*:'            # 允许，但注释须有 W2 依据
```

红法：给 `Handle` 加 `path: PathBuf` 并**让它参与解析** ⇒ 判据 1 必须红（只加死字段不会红——`[read]` 型的坑）。
理由：path 一旦过期就指错对象；而 `(parent, name)` 在排他前提下由本进程维护，且仍需 `child == expected_id` 兜底。

**N2 `.` / `..` 只能由 adapter 合成，且各恰好一次**：mock 的 `read_dir` 返回不含 `.`/`..` ⇒ adapter 枚举结果里两者各 1 次且在最前。
红法：mock 也返回 `.` ⇒ 枚举里出现两次 ⇒ 必须红。

**B3(iii) 的写法**：`remove` 两次不报错是**正确行为**，所以"只有一次"必须靠**计数**断言
（mock 记 `remove(file_id)` 调用次数 == 1）；否则"删两次"和"删一次"都绿。

**N4 删除必须跟随 FileId（结构变异）**：把"最后一个句柄的删除"实现成按**路径**删除（测试：`open` → `rename` → `close`）⇒ 判据 4 必须红；改回按 FileId ⇒ 绿。`DELETE_ON_CLOSE` 与 `set_delete` 两条路径都必须用 FileId（不是打开时的原路径）：路径在 rename 后已失效，W3 的删除键只能是 FileId。

## 9. 已知缺面（派发时，供 §5 记录）

| 缺面 | 谁需要 | 本卡怎么绕 | 建议 |
| --- | --- | --- | --- |
| `create_or_open(disposition)` | adapter 的 Create/Open | mock 实现 | P3-2 G1 按 `CreateOutcome { file_id, kind, action }` 形状实现 |
| `read/write/set_size/get_info/remove` **按 FileId** | 句柄跨 rename / 两段删除 | mock 实现 | P3-2 G2（`read_at_id` 等） |
| `read_dir(dir_id)` | 目录句柄跨 rename 的枚举 | mock 实现 | P3-2 G2 建议补；当前 `fs-api` 只有 `read_dir(path)` |
| `set_times` / `set_attributes` | `SetBasicInfo` | mock 实现 | P3-2 G3（profile v3） |
