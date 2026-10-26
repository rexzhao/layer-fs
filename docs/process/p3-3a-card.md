# P3-3a 任务卡（adapter 骨架 `fs-winfsp` + `WorkspaceOps` trait + mock 后端；**不需要 SDK**）

- 状态：**派发**（2026-10-26，manager → architect）。
- 上游：ADR-009（WinFsp 前端）§10.3 的卡序 —— P3-3「adapter（`fs-winfsp`）+ **mock 后端**：把 handle 表、两段删除、dirent 游标、错误映射接到 fs-api 的 trait 上，用 mock 跑；**仍不需要 SDK**」。
- 本卡**只做"驱动无关的那一半"**：全部逻辑跑在 trait 上，用 mock 后端验证；**不引入 WinFsp 类型、不 `LoadLibrary`、不写 `extern "C"`**。
- 下游：**P3-4**（自写最小 FFI + 真实挂载）。SDK 已于 2026-10-26 就位（`F:/work/winfsp-sdk/`，含 `inc/winfsp/winfsp.h`、`lib/winfsp-x64.lib`；运行时 2.1 已装且驱动已加载），但本卡**不使用**。

---

## 0. 为什么先做"驱动无关的一半"

1. **P3-4 的 FFI 面越小越好**：FFI 层最容易出错且最难测（需要真实挂载、真实 UAC、真实 Win32 行为）。把"所有能用普通 Rust 测的逻辑"先做出来，FFI 层就只剩**转译**（把 WinFsp 回调翻译成对 `WorkspaceOps` 的调用）。
2. **句柄语义是本阶段最容易错的语义**（旧句柄跨 rename / SUPERSEDE / 两段删除仍有效、`DELETE_ON_CLOSE`、share 位规则），而它**完全不需要驱动**就能测。
3. 与 P3-2 的 G2/G3/G4 **不冲突**：本卡只消费 `fs-api` 的既有面；若发现缺面，**记进卡、不擅自扩 fs-api**（见 §5）。

## 1. 交付物

新 crate **`crates/fs-winfsp`**（`no FFI`、无平台依赖、`cargo test -p fs-winfsp` 在任何平台可跑）。内部至少四个模块：

| 模块 | 内容 |
| --- | --- |
| `ops.rs` | **`WorkspaceOps` trait**：adapter 需要核心提供的全部操作（见 §2），**一个方法 = 一次核心调用**，不在这里做策略 |
| `handles.rs` | 句柄表：登记/查找/释放、share 位判定、引用计数、`DELETE_ON_CLOSE`、句柄与 FileId 的绑定（W3：只存 `(workspace, FileId, access, share, delete_on_close, flags)`，**不存路径**） |
| `dirent.rs` | 目录枚举游标：`read_dir` 快照 + `marker`（`.`/`..` 由 adapter 合成，不来自核心）、大小写折叠后的排序与去重、稳定迭代 |
| `error.rs` | 错误映射：`fs_api` 错误 → NTSTATUS 语义（用 `fs-windows` 的映射表，**不重复实现**；缺失的变体在这里补并**记一条**） |

以及 `mock.rs`（**`#[cfg(test)]` 或 dev-dependency**）：一个内存后端实现 `WorkspaceOps`，用于把 §3 的判据全部跑在 trait 上。

## 2. `WorkspaceOps` 的方法清单（起点，允许按证据增删）

按"一次调用 = 一个核心动作"设：

```
open(disposition, path, requested) -> OpenOutcome { file_id, kind, size, times, attributes, delete_pending }
read(file_id, offset, len) -> bytes
write(file_id, offset, bytes) -> written
set_size(file_id, len)
read_dir(file_id, marker, want) -> Vec<DirEntry>
get_info(file_id) -> NodeInfo
set_times(file_id, access, write, change, create)
set_attributes(file_id, attributes)
set_delete(file_id, bool)
rename(from, to, replace)
flush / close(file_id)
```

**约束**：本卡**只定义 trait 与 mock 实现**；`fs-api` 侧的真实实现（`Repo`/`Workspace` 适配）留给 P3-4 与本卡的"第二个实现"（见 §3 判据 7），避免 adapter 与核心在同一提交里互相迁就。

## 3. 判据（每条都要能红；红法写在测试里）

1. **句柄跨 rename 有效**：`open` → `rename(from, to)` → 用**旧句柄** `read` 仍返回原内容；`get_info` 的 `file_id` 不变。
2. **句柄跨 SUPERSEDE 有效**（接 P3-2 的裁决 (a)）：`open` → `open(SUPERSEDE)` 同路径 → 旧句柄 `read` 仍读到**旧内容**；新句柄读到空内容；两者 `file_id` 不同。
3. **两段删除**（W7）：`set_delete(true)` 后 (i) 新 `open` 得到 `DELETE_PENDING`；(ii) 旧句柄 `read`/`write` 仍可用；(iii) `close` 最后一个句柄时才真正 `remove`，且**只有一次**。
4. **`DELETE_ON_CLOSE`**：`open(delete_on_close=true)` → `close` ⇒ 条目消失；若期间被 rename，删除**跟随 FileId** 而不是原路径（这条是本卡最容易写错的一条）。
5. **share 位判定**：3 desired × 3 share 全组合（9 格）表驱动；`FILE_SHARE_DELETE` 与 `set_delete`/`SUPERSEDE` 的交互单列。
6. **dirent 游标**：`marker` 之后的条目稳定返回；枚举期间发生 rename/删除时**不重复、不遗漏已返回项**（语义写在卡里，允许"快照语义"）。
7. **两个实现跑同一批判据**：§3 的 1–6 必须能在 `mock` 与"真实 `fs-api` 后端"上各跑一遍（真实后端可以先只覆盖 1、2、6）。**只跑 mock 的判据不算收口**——mock 是我们自己写的，它和 adapter 可以一起错。
8. **错误映射不重复实现**：`fs-windows` 已有的 NTSTATUS 映射必须被 `pub use` 使用；`fs-winfsp` 里**不得**再出现一份 `match` 出来的状态码表（形状查：`grep -c 'STATUS_' crates/fs-winfsp/src` 只能出现在调用点，不能出现在映射分支里）。

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
- 每次提交带：`cargo test -p fs-winfsp`、`cargo clippy --workspace --all-targets`、以及**判据 7 的"两个实现"进度**（哪几条已在真实后端上跑过）；
- 验收方：QA 独立跑 §3 的 1–8（含变异：把句柄表的 FileId 绑定改成路径绑定 ⇒ 判据 1 必须红）。

## 7. 前置事实（派发前已核）

| 事实 | 值 |
| --- | --- |
| `fs-windows` 的 NTSTATUS 映射 | 已在 main（P3-1 合入 `737c646`），`Kind`/`Failure` 已上移 `fs-semantics`（P3-2 G1） |
| `fs-api` 的公开面 | `Repo` / `Workspace`：`create_file`/`mkdir`/`remove`/`rename`/`truncate`/`write_at`/`read_at`/`read_dir`/`stat` |
| P3-2 的 `Rules::create_or_open` | 已在 worker 分支（`CreateOutcome { file_id, kind, action }`），**本卡按它的形状写 trait**，若形状有变以 main 上的为准 |
| WinFsp SDK | 已就位（`F:/work/winfsp-sdk/DYNAMIC/{inc,lib}`），本卡不用 |

## 8. 卡外补的三条判据（QA 验收清单 + manager 追加，2026-10-26）

**C1-d（判据 7 的真判据）**：同一条变异（把句柄表的 FileId 绑定改成**路径绑定**）必须在 **mock 与真实后端上都红**。
只让 mock 红、real 仍绿 ⇒ real 用例没覆盖该行为（最常见：只断言 `is_ok()`）。以下三种写法算**未收口**：
① real 用例 `#[ignore]`/提前 `return`；② real 只断言 `is_ok()`；③ 场景函数在两个后端参数不同（不是"同一批"）。
覆盖表必须**来自执行**，mock-only 的逐条标出。

**N1 句柄里不得有路径**（W3 的形状判据）：`struct Handle` 的字段里不出现 `Path`/`PathBuf`/路径语义的 `String`。
红法：给 `Handle` 加 `path: PathBuf` ⇒ 必须红。理由：一旦存路径，`rename` 后旧句柄指向旧路径——**判据 1 只抓行为，抓不到"为什么错"**，N1 抓结构端。

**N2 `.` / `..` 只能由 adapter 合成，且各恰好一次**：mock 的 `read_dir` 返回不含 `.`/`..` ⇒ adapter 枚举结果里两者各 1 次且在最前。
红法：mock 也返回 `.` ⇒ 枚举里出现两次 ⇒ 必须红。

**B3(iii) 的写法**：`remove` 两次不报错是**正确行为**，所以"只有一次"必须靠**计数**断言
（mock 记 `remove(file_id)` 调用次数 == 1）；否则"删两次"和"删一次"都绿。
