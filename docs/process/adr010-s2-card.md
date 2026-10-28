# ADR-010 S2 卡：发布锁、管道 commit/snapshot、句柄语义、目录挂载点发现

- 状态：**已收口 `e1632e1`**（architect 起草，2026-10-28）。本文件当初未随 S2 合入 main，由 S3 卡的提交补入。
  **与实现的差异**：§1.2 设想 `Gate` 即 `RwLock<BaseState>`；实现为 `Gate = RwLock<()>`（`fs-workspace/src/gate.rs:23-26`）加 `BaseState` 另用 `Mutex`（`workspace.rs:49`），语义等价。
  **已知缺陷**：G4 的仪器空转（深度只增不减、环境开关无人读取），见 S3 卡 §2
- 依据：ADR-010 §3.4、§10.3、§10.4、§10.6、§13 S2（本卡同批修订 §10.3/§10.4/§4.4 三处，见 §8）
- 基线：**`c42b622`**（S1 收口）。**注意：`c42b622` 目前不在 `origin/main` 上**，而 `origin/main` 上的 `951fa78`（ADR-010 §4.4 补充）不在 `c42b622` 里。
  派卡前先把两者合到 `main`，本卡的行号以 `c42b622` 为准（流程 243）
- 判据形态：三态（通过 / 失败带红值 / 空转）；每条带一个单点变异；提交说明逐条写"变异 → 哪个用例红"
- **本期改核心 API**（`fs-workspace`、`fs-api` 的签名）——这是 S2 的定义，不是停卡条件

---

## 0. 范围

| 本期做 | 不做 |
| --- | --- |
| 发布闸（`fs-workspace`）：设计见 §1 | — |
| `commit / snapshot / restore / rebind` 改为 `&self`；`branch()` / `base_commit()` 改为返回 `String` | — |
| 关掉绕过闸的入口：`Workspace::overlay()`、`WorkspaceStore::overlay()`、`impl Deref for WorkspaceStore`、`WorkspaceStore::top_up` 改为 `pub(crate)` | — |
| 管道 op `commit` / `snapshot`；挂载语境下 CLI 的 `commit` / `snapshot create` 改走管道 | `restore` / `switch` 经管道（S3） |
| 暂停注入点 `pause_point`（与 `crash_point` 同一形态） | — |
| 句柄语义判据（真挂载） | — |
| 目录挂载点的路径发现（§5） | — |

`restore` 与 `workspace discard` 在挂载语境下**继续**按 S1 的 `mounted_refusal` 拒绝。

## 1. 发布闸：设计

### 1.1 要防的事（ADR-010 §3.4）

发布分三步：`materialise`（`crates/fs-workspace/src/publish.rs:120-125`：先 flush，再遍历视图并编码）→ 记录 → `adopt`
（`workspace.rs:386` → `fs-overlay/src/publish.rs:133-153`，其中 `:139-141` 是 `DELETE FROM ov_entry / ov_page / ov_inode`）。

如果在 materialise 之后、adopt 之前有一个 overlay 写事务提交了，这次写**不在新根里，又会被 adopt 删掉**：静默丢写。
读也有问题：`adopt_base` 先提交事务，再更新内存里的 base（`:144-151`）。两者之间的一次读会看到"空 overlay + 旧 base"。

今天这些都不会发生，因为 `&mut self` 在顶着：只有独占引用才能发布。
挂载进程持有的是 `&'static Workspace`，被 WinFsp 分发线程和管道线程共享，所以 S2 接入代理之后，这个窗口就打开了。

### 1.2 闸放在哪一层

**放在 `fs-workspace`，作为一个由 `Workspace` 和它发出的每个 `WorkspaceStore` 共享的 `Arc<Gate>`。**

```rust
pub(crate) struct BaseState { branch: String, base_commit: String, base_root: ObjectId }
pub(crate) struct Gate { base: RwLock<BaseState> }      // 闸本身就是 base 状态的锁：发布改的正是它

pub struct Workspace      { /* … */ gate: Arc<Gate>, overlay: Arc<OverlayStore>, /* 三个可变字段并入 BaseState */ }
pub struct WorkspaceStore { overlay: Arc<OverlayStore>, repo: Arc<Repo>, gate: Arc<Gate> }
```

**为什么不放在 `fs-overlay`**：overlay 层看不见"发布"这个跨三步的过程（第 3 步在主库），而且 materialise 自己就要读 overlay。
闸若放在 overlay，发布者持写锁再去读，就会自锁；为了绕开又得给 overlay 加一套"不取闸"的内部入口，等于把同一个问题搬到更远的地方。

**为什么不放在 `fs-api` / 适配器**：写入路径不只一条（§1.4 列了三条），任何一条漏掉都会让闸失效。`fs-workspace` 是三条路径唯一的交汇点。

### 1.3 谁取读锁、谁取写锁

| 入口 | 锁 | 粒度 |
| --- | --- | --- |
| `WorkspaceStore::read` / `write`（`tree.rs:70,77`，`TreeStore` 实现）：`Rules` 的全部操作（open/create/remove_at/rename/…）都从这里走 | **读** | 一个事务一次（W3：一个操作 = 一个 overlay 事务） |
| `Workspace` 里**直接**碰 `self.overlay` 的公共方法：`security`、`read_at_id`、`write_at_id`、`truncate_id`、`create_file`、`write_at`、`truncate`、`read_at`、`content_size`、`flush`、`is_dirty`、`status`，以及新增的 `set_times` / `set_attributes`（§1.4） | **读** | 一次调用一次 |
| `Workspace` 里经 `self.store()` 间接访问的公共方法（`read_dir_id` 等） | **不取**：由 `WorkspaceStore` 去取 | — |
| `commit`、`snapshot`、`restore`、`rebind` | **写**：从 materialise 第 0 步的 flush 之前，一直持有到 `adopt` 更新完内存 base 之后 | 一次发布一次 |
| 发布内部（`materialise`、`encode` 的读回调、`record_*`、`adopt`） | **不取**：直接用 `self.overlay`，因为写锁已经在手 | — |

**规则："一条调用链只取一次"。** `std::sync::RwLock` 文档写明：同一线程在有写者等待时重复取读锁，可能死锁或 panic。所以：
- 只有上表中的公共入口取锁；内部函数一律不取；
- `Workspace::reserve_for_growth()` → `store().top_up()` 不经过 `TreeStore::write`，所以不取锁；它的调用者 `create_file` 已经取过；
- **调试断言**：`Gate` 维护一个线程局部的持有深度，`acquire` 时若深度不为 0 就 `debug_assert!` 失败。全量测试在 debug 下跑，任何嵌套取锁都会当场 panic（判据 G4）。

### 1.4 关掉绕过闸的入口

| 入口（`c42b622`） | 现在的用户 | 处置 |
| --- | --- | --- |
| `Workspace::overlay() -> &OverlayStore`（`workspace.rs:229`） | `fs-api/src/winfsp_backend.rs:252,259`（`set_times`、`set_attributes`） | 改为 `pub(crate)`；`fs-workspace` 新增 `Workspace::set_times` / `set_attributes`（取读锁），`ApiBackend` 改调它们 |
| `WorkspaceStore::overlay()`（`tree.rs:35`） | 无外部用户 | `pub(crate)` |
| `impl Deref for WorkspaceStore { Target = OverlayStore }`（`tree.rs:58-64`） | 外部经 deref 调 overlay 方法 = 不经过闸 | **删除**；内部用到的地方改成显式的 `self.overlay` |
| `WorkspaceStore::top_up`（`tree.rs:45`） | 内部 | `pub(crate)` |

关掉之后，**`fs-workspace` 之外拿不到任何不经过闸的 overlay 访问**，由编译器保证（判据 G5 再加一道源码扫描）。

### 1.5 签名

- `fs_workspace::Workspace::{commit, snapshot, restore, rebind}(&self, …)`；`adopt(&self, …)` 在已持写锁的前提下改写 `BaseState`；
- `branch()` / `base_commit()` / `base_root()` 改为返回拷贝（`String` / `ObjectId`）：不能返回借用，因为 base 在锁里；
- `fs_api::Workspace` 同步（`fs-api/src/workspace.rs:33,39,180,193,206,219`），CLI 调用处跟着改。

### 1.6 丢写窗口关闭的论证（判据 G1/G2 钉住的就是这几句）

1. overlay 的每个写事务都在一次读锁内完成（§1.3 + §1.4：外部没有别的写入口）；
2. 发布从 flush 之前取写锁，到 adopt 改完内存 base 之后才放；
3. 读写锁互斥 ⇒ 在 [flush, adopt 完成] 区间内，不会有任何写事务提交；
4. 取写锁时，已经开始的写事务必须先结束 ⇒ 它们都在 flush 之前提交，materialise 能看到它们；
5. 区间内到达的写会阻塞到 adopt 之后，落在新 base 之上的 overlay 里 ⇒ 不会被 adopt 删掉；
6. 读同理：不会看到"空 overlay + 旧 base"。∎

这个论证的前提是第 1 条。G5 钉"没有别的入口"，G4 钉"不会嵌套取锁导致死锁"，G1/G2 钉第 3、5、6 条的可观测后果。

## 2. 暂停注入点

与 `crash_point`（`publish.rs:27-42`，环境变量 `WCAS_WORKSPACE_CRASH_AT`，平时只多一次环境查询）同一形态：

```rust
pub(crate) const PAUSE_AT: &str = "WCAS_WORKSPACE_PAUSE_AT";        // 值：<point>=<ms>
pub(crate) const PAUSE_SIGNAL: &str = "WCAS_WORKSPACE_PAUSE_SIGNAL"; // 值：目录；进入暂停时在其中创建 <point>.entered
pub(crate) fn pause_point(name: &str);
```

- 点位：`after-materialise`（写锁内、adopt 之前，正是丢写窗口），`after-publish` 同理；
- 进入时**先**写 `<point>.entered` 再睡，测试轮询这个文件，从而知道"发布正停在窗口里"。这样判据是确定性的，不靠概率；
- 环境变量是**进程级**的：用到它的用例各放在**单独的集成测试文件**里（一个文件一个测试进程），或者在子进程里运行（挂载子进程本来就是子进程）。
  **不得**在与其他用例共享进程的测试文件里设置它。

## 3. 发布闸的判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| G1 | **丢写窗口（L1，确定性）**：线程 A 执行 `commit`，停在 `after-materialise`（暂停 1000 ms）。测试看到 `.entered` 之后，线程 B 用 `Rules::create_or_open` 新建 `/late` 并写入 `"x"`。断言：① B **在 A 返回之后**才返回（记录两者的完成时间）；② 两者都结束后，当前视图里 `/late` 存在且内容为 `"x"`；③ 再 `commit` 一次，新提交的树里有 `/late` | 去掉 `WorkspaceStore::write` 的读锁 ⇒ ① 红（B 在暂停期间就完成了），② 红（adopt 把 `/late` 删掉了） |
| G2 | **读也被挡住**：同样停在 `after-materialise`，线程 C 执行 `read_at_id` 或 `read_dir_id`，断言 C 在 A 返回之后才返回 | 只给写入口加锁、读入口不加 ⇒ C 在暂停期间返回 |
| G3 | **发布之前已开始的写会被包含**：B 先开一个写（`write_at_id`，内容 `"early"`），A 在 B 还没返回时调用 `commit`。断言 A 的新树里有 `"early"`（B 完成后 A 才开始物化） | 把 `commit` 的取写锁挪到 flush **之后** ⇒ 仅这一处变化时 `early` 可能漏掉；若 1000 轮都未复现 ⇒ **空转**，改用 `pause_point("before-flush")` 让顺序确定后再判 |
| G4 | **不嵌套取锁**：① 单元测试：已持读锁的线程再次 `acquire` ⇒ debug 下 panic；② `fs-workspace`、`fs-api`、`fs-winfsp`、`fs-cli` 全量测试在 debug 构建下运行，零次触发该断言 | 去掉深度断言 ⇒ ① 红；在某个经 `store()` 的公共方法外层多取一次锁 ⇒ ② 红 |
| G5 | **没有绕过闸的入口**：源码扫描测试（同 `fs-semantics/tests/dependency_boundary.rs` 的做法），断言 `fs-workspace/src` 里没有 `pub fn overlay`、没有 `impl Deref for WorkspaceStore`；并断言 `fs-api` / `fs-winfsp` / `fs-cli` 的源码里没有 `.overlay()` 调用 | 恢复 `pub fn overlay` 或 `Deref` |
| G6 | **饥饿（V10）**：两条线程持续经 `Rules` 写小文件，第三条线程循环 `commit` 20 次。记录每次 `commit` 等到写锁的时间，取最大值，带红值出档。超过 `GATE_WAIT_RED = 5 s` ⇒ 失败：`std::sync::RwLock`（Windows 上是 SRW 锁）的优先级策略由操作系统决定，这条读数决定是否需要换成公平锁 | —（度量） |
| G7 | **阻塞上界（ADR-010 A16 / V7）**：视图分别为 1k / 10k / 100k 条目时，`commit` 期间另一线程一次写的最长等待。超过 `STALL_RED = 30 s`（`IrpTimeout` 默认 300 s 的 1/10，SDK `fsctl.h:186`）⇒ 失败带红值 | —（度量） |

`GATE_WAIT_RED` 与 `STALL_RED` 各在测试里定义一处常量。

## 4. 管道 commit / snapshot

### 4.1 协议（ADR-010 §10.4，同批修订）

| op | 请求 `args` | 成功 | 失败 |
| --- | --- | --- | --- |
| `commit` | `{message, force}` | `Committed{commit, root}` | `Failed{kind, detail, fields}` |
| `snapshot` | `{}` | `Snapshotted{root}` | `Failed{kind, detail, fields}` |

- **`Failed` 改为带类型**：S1 的 `Failed{detail}` 只有一句话，CLI 没法据此给出退出码。改为 `Failed{kind, detail, fields}`：
  - `kind` 是核心错误的变体名（`BranchMoved`、`NothingToCommit`、`Busy`、…）；
  - `fields` 是该变体的字段（例如 `BranchMoved` 的 `branch / expected / actual`）。
  - CLI 按 `kind` 查 S1 的 `exit_code` 表（同一张表、同一个函数，只是输入从错误值换成了 `kind` 名），并按 ADR-010 §8.2 渲染去路。
- **`commit` 不经过主线程**（回答 worker 的第 2 个定位点）：它不停止挂载，所以由管道线程直接调 `workspace.commit(&self)`，写锁由闸提供。它不走 `unmount` 那套"发 Control → 等 DONE"的握手，**也不需要**。
- 客户端等待响应**不设上限**（ADR-010 §10.2）。客户端被 Ctrl-C 打断时打印常量 `fs_cli::COMMIT_OUTCOME_UNKNOWN`（"请求已发出，结果未知；用 `fs-cli status` 查看"），退出码 1。
- 管道仍是单实例：commit 期间到达的其他请求等 `PIPE_CONNECT_WAIT`（2 s）后得到"挂载进程正忙"，退出码 4。

### 4.2 与停止的交互

- commit 进行中收到 Ctrl-C / Break：主线程收到 Stop → 卸载（分发线程停止）→ `workspace.flush()` 取读锁，**等 commit 放掉写锁** → 删记录 → 唤醒管道线程。
  管道线程此时正在写 commit 的响应，写完后在下一轮循环看到 `SHUTTING_DOWN` 并退出。⇒ commit 要么完整完成，要么根本没开始，不会出现半个。
- `CTRL_CLOSE_EVENT` 只有约 4.5 s：若 commit 超过这个时间，进程会在发布中途被系统结束。这种情况由 ADR-006 §9.3 的恢复兜底：下次打开时，按 `pending_root` 判断第 3 步提交了没有。**不另加机制**，判据 P4 验证恢复确实发生。
- 远程 `unmount` 在 commit 进行中：拿不到管道实例 ⇒ 等 2 s 后返回"正忙"，退出码 4。

### 4.3 CLI

| 命令（目标 workspace 正被挂载） | S1 | S2 |
| --- | --- | --- |
| `commit -m <msg> [--force]` | `mounted_refusal` | **经管道** |
| `snapshot create` | `mounted_refusal` | **经管道** |
| `restore`、`workspace discard` | `mounted_refusal` | 不变（`restore` 留到 S3） |

### 4.4 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| P1 | 挂载 `dev`，经挂载点写一个文件，然后在挂载内执行 `fs-cli commit -m m1`：退出码 0，输出 `dev: <id>`；`log dev` 的第一条就是它；`status` 变为 `dirty=no`。`snapshot create` 同样经管道，返回的 root 出现在 `snapshot list` 里 | CLI 在挂载语境下仍走 `mounted_refusal` |
| P2 | 错误透传：没有改动时 `commit` ⇒ 退出码 5，消息为 `NothingToCommit` 的渲染；另一进程 `reset --branch dev` 改了分支头之后再 `commit` ⇒ 退出码 5，消息含 expected 与 actual；带 `--force` ⇒ 成功 | 管道把所有错误都折成 `Failed{detail}`（`kind` 丢失）⇒ 退出码变成 1 |
| P3 | **正忙**：挂载子进程以 `WCAS_WORKSPACE_PAUSE_AT=after-materialise=3000` 启动。commit 停在暂停里时执行 `status`：约 2 s 后退出码 4，消息为"正忙"；commit 返回后 `status` 正常 | 管道改为多实例（`status` 能连上，接着在闸上等到 commit 结束后成功返回，"正忙"没有出现） |
| P4 | **停止期间的 commit**：同样暂停 3000 ms；commit 停在暂停里时向挂载进程组发 `CTRL_BREAK_EVENT`。断言：进程以退出码 0 结束；commit 客户端拿到 `Committed`；`log dev` 里有该提交；重新挂载后视图里的内容与提交一致 | 停止序列不等闸、直接退出（例如卸载后跳过 flush 就 `process::exit`）⇒ 客户端拿不到响应（结果未知）。**若这个变异下 `log` 里仍然有提交**（被恢复补上了），只能说明恢复生效，不能说明等待生效：判据只认"客户端拿到 `Committed`" |
| P5 | `Failed` 的 `kind` 到退出码的映射，复用 S1 `exit_code` 的同一张表（单元测试：对每个 `kind` 名，结果都与对应错误值走 `exit_code` 相同） | 管道侧另写一张表 |

## 5. 句柄语义（真挂载，ADR-010 §10.6）

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| H1 | 经挂载点 `CreateFileW` 打开 `/f` 得到句柄 `h`，`WriteFile(h, "a")`。记下 `GetFileInformationByHandle(h)` 的 `nFileIndexHigh/Low`，然后经管道 commit。断言：`ReadFile(h)` 从偏移 0 读到 `"a"`；`FileIndex` 不变；`WriteFile(h, "b")` 之后 `status` 为 `dirty=yes`；第二次 commit 成功，且其树中 `/f` 的内容为 `"b"` | 在 `materialise` 里给 serial 加一层一致的重编号（inode 记录与子项引用用同一张映射：`materialize.rs:224-226` 与 `:269`；树仍然自洽，`verify` 照样通过）⇒ `FileIndex` 改变 |
| H2 | commit 之前打开目录句柄 `d`（`FindFirstFileW` 或 `FILE_LIST_DIRECTORY`）；commit 之后用 `d` 列目录，结果与 commit 前相同 | —（读数核对，三态） |
| H3 | commit 之后新建的文件，其 `FileIndex` 与 commit 之前出现过的所有 `FileIndex` 都不同 | `adopt` 时重置 `serial_next`（新文件会复用旧 serial） |
| H4 | **delete-pending 跨 commit**：打开 `/g` 并设删除处置（`FILE_DISPOSITION_INFO`），句柄先不关，commit。断言：该提交的树里有 `/g`（删除挂起期间名字仍在，与 NTFS 一致）；关闭句柄后视图里没有 `/g`；下一次 commit 的树里也没有 | —（语义读数，三态） |

H1 到 H4 都用 `fs-api` 的 `ReadView` 读取提交的树，不经过 CLI（ADR-010 D7：CLI 不读文件）。

## 6. 目录挂载点的路径发现（A6d 的修法）

S1 的读数（`c42b622`）：`GetVolumePathNameW` 在 FSD 卷上失败（1005）；目录挂载点取到的根是宿主卷，卷标不是 `layerfs`。

**方案：不用 GUID，改用挂载进程自己量出来的 NT 路径。**

- 挂载成功后，挂载进程对自己的挂载根以 `FILE_FLAG_BACKUP_SEMANTICS` 打开一个句柄，调 `GetFinalPathNameByHandleW(h, VOLUME_NAME_NT)`，
  得到形如 `\Device\…\` 的前缀，写进记录的新字段 `volume_nt`。
- CLI 对 cwd 做同样的调用，再与每条活记录的 `volume_nt` 做前缀匹配。**必须按路径分量边界匹配**：`\Device\Volume{1}` 不能匹配 `\Device\Volume{12}\x`。
- 匹配成功就进入挂载语境。顺序：先按 `volume_nt` 匹配，失败再走 S1 的"两条路取根 + 卷标"。

**为什么不用 `VOLUME_NAME_GUID`**：卷 GUID 只有在卷向 mount manager 注册之后才有，WinFsp 的目录挂载点是否注册**未验证**。
而 NT 路径是挂载进程对自己挂载根测出来的值，CLI 用同一个 API 对 cwd 再测一次，两边不需要任何关于 WinFsp 内部的假设，只需要这个 API 在该卷上能工作（V11）。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A6e | 目录挂载点 `…\mnt` 上挂载 `dev`：cwd 为 `…\mnt` 和 `…\mnt\sub` 时，不带任何选项执行 `status`，都显示 `dev`。盘符挂载照常（A6b 不回退） | 记录不写 `volume_nt` |
| A6f | 前缀匹配按分量边界：两条假记录的 `volume_nt` 分别为 `\Device\X1` 与 `\Device\X12`，cwd 的 NT 路径为 `\Device\X12\a`，只匹配后者（纯函数单测） | 改成按字符串前缀匹配 |
| A6g | **V11**：`GetFinalPathNameByHandleW(VOLUME_NAME_NT)` 在 WinFsp 盘符卷和目录挂载卷上的返回值，按三态记录。若在目录挂载卷上失败 ⇒ A6e 记为失败并带红值；ADR-010 §4.4 维持"目录挂载点不支持发现"，本项**不阻塞**本卡的其余部分 | — |

## 7. 未验证项

| # | 项 | 判据 |
| --- | --- | --- |
| V5 | commit 前后 FileId 与句柄有效性（ADR-010 §10.6 是从代码推出的） | H1–H3 |
| V7 | 写锁持有时长相对 `IrpTimeout` 的余量 | G7 |
| V10 | `std::sync::RwLock`（SRW）下 commit 会不会被持续的读写饿死 | G6 |
| V11 | `GetFinalPathNameByHandleW(VOLUME_NAME_NT)` 在 WinFsp 卷上可用 | A6g |

**停卡条件**：G1 或 G2 在加了闸之后仍然红（说明 §1.4 漏了入口）；G4 的第 ② 项触发断言且不能通过调整取锁位置消除；H1 的 `FileIndex` 在**未施加变异**时就变了（ADR-010 §10.6 的推论不成立，代理 commit 的前提就没了）。

## 8. ADR-010 同批修订（判据影响：无（澄清））

1. §10.3：写明闸在 `fs-workspace`、由 `Workspace` 与 `WorkspaceStore` 共享；补 §1.3 的取锁表、§1.4 的入口处置、"一条调用链只取一次"及调试断言；
2. §10.4：`Failed` 改为 `Failed{kind, detail, fields}`；新增 `Committed` / `Snapshotted`；`commit` 不走 Control 握手；
3. §4.4：目录挂载点改为按 `volume_nt` 匹配。根据 A6g 的结果二选一：成立则撤掉 `951fa78` 那段"不支持"，不成立则保留并注明 V11 的读数。

## 9. 提交建议

1. `Gate`、`BaseState`、签名改为 `&self`、关掉入口（§1.4）、`set_times` / `set_attributes` 下沉；G4、G5。
   此时调用方全部改完，**行为不变**，全量测试保持绿；
2. `pause_point`；G1、G2、G3；
3. G6、G7 的度量出档；
4. 管道 `commit` / `snapshot`、带类型的 `Failed`、CLI 路由；P1、P2、P3、P5；
5. 停止交互：P4；
6. 句柄语义：H1–H4；
7. `volume_nt` 与前缀匹配：A6e–A6g；ADR §4.4 的修订随这一步一起提交。

G1 或 G2 红了就停在第 2 步回报。
