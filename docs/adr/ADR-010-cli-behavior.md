# ADR-010：CLI 行为模式（workspace 定位、路径、挂载生命周期、多挂载、错误）

- 状态：**Accepted（设计定稿）**——architect 起草，2026-10-26；owner 已裁 D1–D7（§14）；worker 落地核对后加"落地核对完成 <日期/commit>"
- 日期：2026-10-26
- 起草：architect
- 依据：owner 2026-10-26 两条意见（"mount 后 current 应跟着变"、"应能从当前路径所属挂载点反查 workspace/branch，
  不需要 CLI 自存 `cli/current`"）、owner 裁决 D1（"去掉 cli/current，如果 cwd 不在活的挂载内，必须显式指定 workspace
  或者目标路径，就像 git 一样"）、owner 裁决 D7（"通过 cli 直接修改文件意义不大，应该去掉这些功能，我们只提供文件挂载映射功能，
  修改属于文件系统本身工作"）与总原则（**用起来与普通文件系统的真实行为一致**）；ADR-006 §2 W2、§10、§11、§13；
  ADR-009 W2 / §5.4；P3-4 收口报告（M1–M9、B-22/B-23/B-24）；backlog B-14
- 事实基线：所有代码引用以 `origin/main` **`25479a5`** 为准（行号与该 SHA 绑定，流程 243）
- 修订：本 ADR **取代 ADR-006 §13 的 CLI 部分**（API 部分不变）；不改 ADR-009

---

## 1. 背景与问题

CLI（`fs-cli`）现在用 `<store>/cli/current` 记录"当前 workspace"，文件命令（put/get/ls/…/commit）都读它；
`mount` 另有 `--workspace`，不读也不写 current。owner 认为 current 应随挂载变化，并且更根本地，应能从
**当前路径所在的挂载点**反查出 workspace，不需要 CLI 自存状态。需要一次定义清楚：

1. CLI 操作哪个 store / workspace——来源与优先级；
2. CLI 的路径怎么写（`/path`、`//path`、相对路径、挂载点内路径）；
3. `mount` / `umount` 的生命周期（含 Ctrl-C、超时、force）；
4. 挂载期间其它命令的行为，尤其 `commit`；
5. 多 workspace / 多挂载 / 多 store 的交互；
6. 错误怎么分类、怎么给出去路。

## 2. 决策摘要

| # | 决策 |
| --- | --- |
| C0 | **CLI = 生命周期管理 + 挂载服务（owner 裁决 D7）**：去掉全部文件级命令 `put/get/ls/stat/mkdir/rm/mv/cat`（含历史读 `cat/ls <commit>:<path>`）；文件的读写与浏览只经挂载后的文件系统（资源管理器、shell、任何程序）。CLI 不接受任何 workspace 内路径 |
| C1 | **去掉 `cli/current`（owner 裁决 D1）**：CLI 不保存任何"当前 workspace"状态；不读、不写、不作回落。workspace 只来自显式参数或路径发现 |
| C2 | **定位优先级**：`--workspace` 显式 > **路径发现**（cwd 在一个活的挂载内）> **无默认：报错** `no workspace specified; use --workspace <name>, or cd into a mount point`；store：`--store` > 路径发现 > `.`。路径发现给出的是 **(store, workspace) 二元组**，不与显式 `--store` 拆开混用（§4.2） |
| C3 | **`mount` 必须显式 `--workspace`**：挂载前 cwd 不可能在"这个"挂载内，路径发现不适用；现硬编码的缺省 `"main"` 是隐式默认，与 C1 同类，删除 |
| C4 | **挂载期间不打扰用户（owner 裁决 D2/D3）**：按命令类别分流（§6 表）——仓库级命令照常（CLI 进程内）；**工作区级命令经管道交给挂载进程执行**（§10）。没有一条命令因"正在挂载"而要求用户先 umount，唯一例外是 `workspace discard` 与 `gc` |
| C5 | **路径**：CLI 只接受**宿主路径**（`--store` 目录、挂载点），由 OS 解析；不接受 workspace 内路径（C0），因此 `//path`、相对路径锚点、Git Bash 路径改写等问题**整类消失**（§5） |
| C6 | **挂载记录**：一条记录、一个位置（`%LOCALAPPDATA%\layerfs\mounts\`），由挂载进程**持有不可写共享的句柄**作为存活证明，记录里写管道名；**不在 store 目录另存一份**（§9） |
| C7 | **mount 生命周期（owner 裁决 D4）**：前台进程；Ctrl-C / Ctrl-Break / 关闭控制台 = 一次正常 umount 请求（flush → 卸载 → 删记录）；`umount` 经管道（正常 / 超时报句柄数 / `--force` 只清陈旧记录并如实说明） |
| C8 | **同一 workspace 不得被两个打开者持有（owner 裁决 D5）**：两层——记录层（友好错误，指出已挂在哪里）+ 锁层（W2，真正的保证）；跨进程锁层**目前无判据**（B-14b / B-22），本 ADR 把它列为 S1 判据 |
| C9 | **错误**：按"用户下一步做什么"分类并带去路；区分 store / workspace / branch / commit 四种"不存在"；退出码分档（§8.3） |
| C10 | **`checkout <b>` = 创建或恢复名为 `<b>` 的 workspace，无副作用**（不再"切换"任何东西）；在挂载语境里（cwd 在 U: 内）`checkout <b>` 额外请求挂载进程**换挂**到 `<b>`（管道 `switch`，要求无打开句柄，§7.3） |
| C11 | **挂载进程的线程结构**：WinFsp 回调本来就在 WinFsp 自己的分发线程上（`mounted.rs:901` 起 2 个）；主线程只管生命周期；**管道服务一个独立线程**，与主线程用一条 channel 通信（§10.5，选 B） |
| C12 | **发布锁**：`Workspace` 增一把读写锁——所有视图操作取读锁，`commit/snapshot/restore/rebind` 取写锁并改为 `&self`；把 ADR-006 §9.1 第 0 步"发布期间写操作阻塞"从借用规则搬到运行时（§10.3） |

## 3. 事实基线（决定设计的四件事）

### 3.1 W2：挂载期间只有挂载进程能碰该 workspace

- `workspace_open` 急打开 overlay（`fs-workspace/src/repo.rs:482-531`，`:525` 首次查询），overlay 以
  `PRAGMA locking_mode = EXCLUSIVE` 打开（`fs-overlay/src/schema.rs:86`）；ADR-006 W2：第二个打开者得到 `WorkspaceInUse`。
- `mount` 在整个挂载期间持有该 workspace（`fs-cli/src/mountcmd.rs:210`，`Box::leak` 成 `&'static`）。
- ⇒ 挂载 `dev` 期间，CLI 里**任何**打开 `dev` 的命令都会失败；"`cd U:\` 后 `fs-cli put` 写进 U: 的 workspace"
  **在现架构下不可能**，与 workspace 由谁选定无关。
- **未验证**：跨进程的 `WorkspaceInUse` 没有判据（backlog B-14b）；P3-4 M4 报告"二次挂载无 WorkspaceInUse 闸"
  （B-22）。两者必须在 S1 对齐（§13 判据 A4），本 ADR 的分流设计**不依赖**锁层一定有效——记录层先拒（C8）。

### 3.2 `cli/current` 的现状（将被移除）与相关不一致

- 位置与读写：`fs-cli/src/main.rs:465-485`（`current_path` / `read_current` / `write_current`）；写入者只有
  `init`（`:86`）与 `checkout`（`:359`）；读者是 `with_workspace`（`:448`）、`current_branch`（`:460`）、`snapshot create`（`:273`）。
- **ADR-006 §13 原设计就是每条命令显式 `--workspace <w>`、没有 current**；current 是实现时加的便利，ADR 从未追认。
  owner 裁决 D1 = 回到原设计，并加上路径发现。
- **`log` / `reset` / `revert` 为了取分支名而打开 overlay**：`current_branch()`（`main.rs:460`）调 `workspace_open`。
  挂载期间这三条仓库级命令因此全部失败——它们本不需要 overlay：`Repo::workspace_list()`（`repo.rs:550`）只读主库就能拿到 `branch`。
  去掉 current 后它们的分支来源见 §4.3。
- **`mount` 缺省 workspace 是硬编码 `"main"`**（`mountcmd.rs:178`）——另一个隐式默认（C3 删除）。

### 3.3 挂载记录的现状

- `%TEMP%\layerfs-mounts\<record>.mount`，内容 `pid / mount_point / workspace / store`（`mountcmd.rs:62-72,217-225`）；
  停止请求是同目录的 `.stop` 文件；挂载进程每 50 ms 轮询，**记录文件消失也视为停止请求**（`:238`）。
- 风险：`%TEMP%` 会被"存储感知 / 磁盘清理"清扫——**记录被清掉 = 挂载自行卸载**（`:238` 的 `!state.exists()`）。
  **未验证**（是否会清扫正在使用目录内的新文件取决于策略），但后果不可接受，C6 换位置。
- 陈旧：进程被 `taskkill /F` 后记录残留（M7），目前只能 `umount --force` 手清。

### 3.4 工作区级写操作要 `&mut Workspace`，且这不只是类型问题

- `Workspace::commit / snapshot / restore` 取 `&mut self`（`fs-api/src/workspace.rs:180,193,206`；`fs-workspace/src/publish.rs:61,96`、
  `workspace.rs:337`）；挂载进程持有的是 `&'static Workspace`（`mountcmd.rs:210`），被 WinFsp 分发线程并发使用。
- `&mut` 在今天**承担了一个正确性前提**：发布 = `materialise`（`publish.rs:120-125`：flush，再遍历视图并编码）→ 记录 → `adopt`
  （`workspace.rs:386`→ `fs-overlay/src/publish.rs:133-153`：**`DELETE FROM ov_entry / ov_page / ov_inode`**，`:139-141`）。
  若在 materialise 之后、adopt 之前有一次写进入 overlay，它**不在新根里，又被 adopt 删掉——静默丢写**。今天挂载进程从不调 commit，
  CLI 进程用 `&mut` 独占，所以不会发生；**代理 commit 一旦接入，这个窗口就打开**。
- ⇒ 代理不是"接一条管道"，前置是 C12 的发布锁（§10.3），其判据（L1）必须能复现上面的丢写。

## 4. workspace 定位

### 4.1 无默认（C1，owner 裁决 D1）

- CLI **不保存**"当前 workspace"。cwd 不在活的挂载内、又没有 `--workspace` 时，需要 workspace 的命令一律报错（§8.2），
  像 git 在没有工作目录时必须显式给 ref 一样。
- **为什么这比"保留 current"好**（architect 原推荐保留，owner 否决；理由记在这里供后来者）：
  - 命令的含义只由**命令行 + cwd** 决定，不由一个看不见的文件决定；脚本在任何机器、任何先后顺序下行为相同；
  - 从根上消除了"current 要不要随挂载变"这一整类问题（初稿的三条：W2 下有害、历史依赖、崩溃不一致）——没有状态就没有同步；
  - 回到 ADR-006 §13 的显式设计（§3.2）。
- 代价（接受）：未挂载时每条工作区命令（`commit/status/restore/snapshot create`）都要 `--workspace`。交互便利由路径发现承担：`cd U:\` 之后不需要任何参数。
- 遗留文件：既有 store 里的 `<store>/cli/current` **被忽略**（不读、不删、不报错）。项目未上线，不做迁移提示；
  CLI 也不再创建 `<store>/cli/` 目录。

### 4.2 优先级与路径发现（C2）

```text
(store, workspace) =
    显式：--store S 且/或 --workspace W                       （最高）
  > 发现：cwd 位于一个活的挂载 M 内 ⇒ (M.store, M.workspace)
  > 无：store = "."，workspace = 无 ⇒ 需要 workspace 的命令报错
```

组合规则（防"拼出一个不存在的组合"）：

| 显式给了 | cwd 在挂载 M 内 | 结果 |
| --- | --- | --- |
| 都没给 | 是 | `(M.store, M.workspace)` |
| 都没给 | 否 | `(".", 无)` ⇒ 报错 |
| 只给 `--store S` | 是，且 `S ≡ M.store` | `(S, M.workspace)` |
| 只给 `--store S` | 是，且 `S ≢ M.store` | **丢弃发现**：`(S, 无)` ⇒ 报错 |
| 只给 `--workspace W` | 是 | `(M.store, W)` |
| 只给 `--workspace W` | 否 | `(".", W)` |
| 两者都给 | 任意 | 显式 |

`≡` 是 store 路径规范化后相等（`std::fs::canonicalize`，去掉 `\\?\` 前缀再比较）。

`--workspace <name>` 成为**全局选项**（ADR-006 §13 的显式风格），不再只是 `mount` 的参数。

### 4.3 不需要 workspace 的命令从哪里取分支

去掉 current 后，"当前分支"也不再隐含。分支来源：**命令的显式分支参数 > 解析出的 workspace 的分支（`workspace_list()`，不开 overlay）> 报错**。

| 命令 | 形状 | 无分支、无 workspace 时 |
| --- | --- | --- |
| `log` | `log [<branch>] [limit]` | 报错（C2 的消息，并提示可给 `<branch>`） |
| `reset` | `reset <commit> [--branch <b>] [--expect <c>]` | 报错 |
| `revert` | `revert [--branch <b>]` | 报错 |
| `branch`、`stats`、`verify`、`gc`、`snapshot list/drop`、`list`、`version` | 仓库级 | 不需要 |

ADR-006 §13 原设计即 `log <b>`、`reset <b> <commit>`、`revert <b>`；这里允许省略 `<b>` 仅在 workspace 已解析时。

### 4.4 路径发现的技术方案

两步，各自可失败，失败 = "不在挂载语境"（**本身不是错误**；之后若命令需要 workspace 而没有，才按 C2 报错）：

1. **cwd → 卷根**：`GetVolumePathNameW(cwd)` 得到 `U:\` 或目录挂载点 `C:\mnt\x\`；
   再 `GetVolumeInformationW(卷根)` 核对卷标 = `"layerfs"`（`mountcmd.rs` 的 `volume()`）——防止把别家卷当成我们的。
   - **未验证（L2）**：WinFsp **目录**挂载点下 `GetVolumePathNameW` 是否返回挂载目录；`subst` 盘、映射网络盘下的行为。
     盘符挂载是标准行为。若目录挂载点不返回挂载目录，备选 `GetFinalPathNameByHandleW(cwd 句柄, VOLUME_NAME_GUID)`
     取卷 GUID，与记录里的卷 GUID 比对（记录需增一字段）。
2. **卷根 → 记录**：扫描记录目录（挂载数个位数，扫描无成本），取 `mount_point` 规范化后等于卷根的那条，
   并做**存活校验**（§9.2）；不存活 ⇒ 视为不在挂载语境，并在 stderr 提示 `umount <mp> --force` 清陈旧记录。

## 5. 路径规则（C5）

D7 之后 CLI **没有 workspace 内路径参数**。剩下的路径参数只有两类，都是宿主路径，交给 OS：

| 参数 | 例 | 解析 |
| --- | --- | --- |
| store 目录 | `--store F:\repo`、`init <path>` | OS（相对 cwd）；比较时 `canonicalize` 并去掉 `\\?\` 前缀（§4.2） |
| 挂载点 | `mount U:`、`mount C:\mnt\x`、`umount U:` | OS；记录名用 `record_name()`（`mountcmd.rs:38`） |

因此初稿 §5 讨论的三件事**不再存在**，记录在此以免被重新引入：

- `//path`：是 Git Bash 逃避路径改写的写法，从来不是 CLI 语法；没有 workspace 路径参数后无从出现；
- 相对 workspace 路径与"挂载内锚点"：无参数可锚；
- 同一文件"挂载点宿主路径 vs workspace 路径"两种拼写：只剩宿主路径一种，经文件系统访问。

**不得重新引入 workspace 内路径参数**（例如为了"方便"加回 `cat`）：那会重新打开第二条读写路径与上述三个问题；
如需在 CLI 里查看历史，见 §14 D7 的替代（挂载一个提交）。

## 6. 挂载期间的命令分流（C4）

"挂载语境" = 解析出的 workspace 正被一个活的挂载持有（不论 cwd 是否在挂载内——`--workspace dev` 指向一个挂着的 `dev` 也算）。
CLI 从挂载记录得到挂载点 `M.mount_point` 与管道名 `M.pipe`。

| 类别 | 命令 | 挂载期间 | 实现要点 |
| --- | --- | --- | --- |
| 仓库级、只读、不需要 overlay | `log`、`branch`（列出）、`stats`、`verify`、`list`、`version`、`snapshot list` | **照常**（CLI 进程内） | `log/reset/revert` 取分支按 §4.3，走 `workspace_list()` |
| 仓库级、写主库、不碰该 overlay | `branch <new>`、`branch delete`（另一分支）、`reset`、`revert`、`snapshot drop` | **照常**（CLI 进程内）；`reset/revert` 作用于被挂载 workspace 的分支时打印：挂载侧下次 commit 会按 ADR-006 §10.3 得到 `BranchMoved` | 不新增语义 |
| 工作区级 | `status`、`commit`、`snapshot create` | **经管道交给挂载进程**（§10.4 `status/commit/snapshot`） | `status` 只读（S1）；`commit/snapshot` 需 C12 发布锁（S2） |
| 工作区级、替换视图 | `restore` | **经管道**，**仅当挂载上无打开句柄**；否则拒绝并报句柄数（§10.4 `restore`） | §6.2 |
| 生命周期 | `umount` | **经管道**（§10.4 `unmount`） | 取代 stop 文件 |
| 换挂 | `checkout <b>`（cwd 在挂载内） | **经管道**，**仅当无打开句柄**（§10.4 `switch`） | §7.3 |
| 维护 | `gc` | **拒绝**（ADR-008 §6.1、B-9：GC 期间不得有句柄是其正确性前提）；错误列出活的挂载 | 不代理：在挂载进程里跑 GC 正是 B-9 要防的事 |
| 生命周期 | `workspace discard dev` | **拒绝**：先 umount | 删一个正在服务的 workspace 等于从挂载下抽走存储（`branch delete` 在 workspace 仍在该分支上时本就拒绝） |
| 同一 workspace 再挂 | `mount V: --workspace dev` | **拒绝**：`dev 已挂载在 U:` | C8 |

### 6.1 文件操作不在 CLI 里（C0，owner 裁决 D7）

- 文件的读、写、浏览、改名、删除**只**经挂载后的文件系统。这不是"挂载期间的分流规则"，而是 CLI 的边界：未挂载时也没有文件命令。
- 由此成立的性质：workspace 内容**只有一条写路径**（WinFsp → 适配器 → `Workspace`），FSD 的 share mode、delete-pending、`FileNode` 缓存、
  大小写折叠永远由同一处裁决；初稿担心的"CLI 写路径绕过 FSD"（经管道直写 overlay 会改写被应用 deny-write 打开的文件）**从结构上不可能**。
- 管道协议因此**不含任何文件 op**（§10.4）。

### 6.2 为什么 restore 只在无句柄时代理

- restore 把视图换成另一棵树：打开句柄底下的对象可能消失或内容整体变化。NTFS 没有"文件底下换内容"的行为；总原则下不可接受。
- 无句柄时没有观察者。内核侧：`FileInfoTimeout = 0`（`mounted.rs:838`）关闭了元数据缓存；**数据缓存在 `FileInfoTimeout = 0` 时
  不启用——未验证（V6）**，S3 判据 A20 直接测"restore 后重新打开读到新内容"。
- "句柄数"来自适配器的注册表（P3-4 收口 §1.2：一挂载一 HandleTable），不是 FSD 的计数；Explorer 打开着目录窗口会持有目录句柄——
  这时 restore 被拒是**正确**的，错误信息写明"有 N 个打开的句柄（包括资源管理器窗口）"。

## 7. 生命周期

### 7.1 mount（C7）

1. 解析 store（§4.2）；`--workspace` **必须显式**（C3），缺省报 `no workspace specified; use --workspace <name>`；
2. 版本闸（B-23，现有）；
3. **记录层互斥**：存在指向同 `(store, workspace)` 的活记录 ⇒ 拒绝（C8 第一层）；
4. `workspace_open`（W2 锁，C8 第二层）；
5. 写记录并**持有其句柄**（§9.2）；挂载；前台阻塞；
6. 停止条件：管道 `unmount` 请求、**Ctrl-C / Ctrl-Break / 控制台关闭**（`SetConsoleCtrlHandler`；现状无处理，Ctrl-C 直接结束进程 ≈ M7 的残留形态）、
   `switch` 请求（停止后用新 workspace 重挂，§7.3）——三者都只是往主线程的 channel 发一条消息（§10.5）；
7. 停止序列：**卸载 → `flush` → 关闭并删除记录**（W3/W4：卸载后不再有回调，此时的 flush 覆盖挂载期全部写入；先 flush 再卸载会漏掉两者之间到达的写——S1 卡 §5.1 修订）。第二次 Ctrl-C（5 s 内）= 立即退出，
   打印"未 flush 的写入可能丢失（最后一次 flush 之前的都在）"。
   - `CTRL_CLOSE_EVENT` 系统只给约 5 s：flush 是一次 `synchronous=FULL` 提交，正常远小于此；**未验证**大 overlay 下的上界。

后台挂载（`mount --detach` / 服务化）**不在本 ADR**：涉及会话、登录、服务账户，若要另立 ADR。

### 7.2 umount

| 情形 | 行为 |
| --- | --- |
| 正常 | 管道 `unmount{wait_ms: UNMOUNT_WAIT}`（10 s，`mountcmd.rs:28` 的常量保留为唯一来源）⇒ 挂载进程 flush、卸载、删记录后回 `ok` ⇒ "unmounted" |
| 有打开句柄（M5） | 挂载进程最多等 `wait_ms` 让句柄归零；仍不为零 ⇒ 回 `HandlesOpen{handles}`，**不发停止**；CLI 报"未在 10 s 内卸载，N 个打开的句柄"，**不改任何状态**（关闭 B-26 的"持有者打印"一半：给出数量，不给进程名——WinFsp 用户态拿不到打开者进程） |
| 管道连不上，记录判活为陈旧 | "挂载进程已不在；`umount <mp> --force` 清理记录" |
| `--force` | 只删陈旧记录，并如实说明"卷可能仍挂着，结束挂载进程"（WinFsp 句柄在对方进程里——`mountcmd.rs:1-12` 的诚实原则保留）；记录判活为**活**时 `--force` 拒绝（不删活记录） |
| 无参数且 cwd 在挂载内 | 卸载该挂载（显式参数优先） |
| 控制台 Ctrl-C / Break / 关闭 | **不看句柄**，照常卸载并打印"N 个打开的句柄将失效"：用户在挂载窗口前明确要求停止；远程 `umount` 的发起方未必知道有文件开着，所以它拒绝（S1 卡 §5.3） |

### 7.3 checkout 与挂载（C10）

- 新语义：`checkout <b>` = **创建或恢复**名为 `<b>` 的 workspace（分支 `<b>`）。不存在则 `workspace_create(b, b)`；
  存在则不切换任何东西，只报告状态（若被挂载，含"挂载在 U:"）。**无副作用**：不写任何 CLI 状态（C1）。
  输出给出下一步：`workspace b ready; use --workspace b, or mount it: fs-cli mount <mp> --workspace b`。
- 存在性检查走 `workspace_list()`，**不打开 overlay**（现 `main.rs:344-360` 打开它——`<b>` 正被挂载时 checkout 会失败）。
  中断发布的恢复（ADR-006 §9.3）由下一次真正的 `workspace_open` 完成，checkout 不必为此打开。
- 分支 `<b>` 不存在 ⇒ `NotFound{branch}`，去路 `fs-cli branch <b> --from main`。
- **挂载语境下"换分支"（owner 裁决 D3）**：cwd 在 U: 内时，`checkout <b>` 先做上面的创建/恢复，再经管道发 `switch{workspace: b}`：
  - 挂载进程检查**无打开句柄**（§6.2 同一理由：有句柄时换挂会让句柄指向消失的对象），否则回 `HandlesOpen{handles}`，CLI 报数量并给出
    "关闭文件/资源管理器窗口后重试"；
  - 无句柄：flush → 卸载 → 关闭旧 workspace → 打开 `<b>`（W2 锁；`<b>` 被别处持有 ⇒ 回 `WorkspaceInUse`，**旧 workspace 重新挂回**，
    不留下"盘符空了"的中间态）→ 以同一挂载点重挂 → 原地更新记录（`workspace`、`branch`；同一句柄，判活不间断）→ 回 `ok`；
  - 重挂失败（盘符被抢等）⇒ 回错误并保持未挂载、记录删除，CLI 如实报告"U: 已卸载，未能以 `<b>` 重挂：<原因>"。
- cwd 不在挂载内时 `checkout <b>` **从不**影响任何挂载（要换某个挂载，cd 进去，或 `umount` + `mount`）。
- "U: 现在是哪个分支"只从**挂载记录**读（单一来源）。

## 8. 错误模式（C9）

### 8.1 原则

每条错误回答两件事：**发生了什么**（带对象名与值）+ **下一步做什么**（一条可以照抄的命令或动作）。
同一 NTSTATUS / 错误类型在不同语境下去路不同时，按语境分支（P3-4 的 0x35 / 0x22 先例）。

### 8.2 分类表

| 情形 | 现状 | 规定 |
| --- | --- | --- |
| store 目录不存在 | "store directory X does not exist"（`cas-store/src/layout.rs:121`） | 同；加 "`fs-cli init X` 创建" |
| 目录存在但不是仓库 | `NotInitialised` | "X 不是 layer-fs 仓库（没有主库）" |
| 主库由别的构建写 | `SchemaMismatch` | 原文 + "本构建不迁移旧库（ADR-009 §10.1）" |
| **未指定 workspace** | 读 current；缺失时 "no current workspace; run checkout <branch>" | **`no workspace specified; use --workspace <name>, or cd into a mount point`**（逐字固定于一个常量 `NO_WORKSPACE`，A0 断言该常量；S0 尚无路径发现，常量暂不含 `, or cd into a mount point`，S1 补上——不提前承诺一条还走不通的出路） |
| workspace 不存在 | `NotFound{workspace X}` | "没有 workspace X；`fs-cli checkout <branch>` 创建"（来源只可能是 `--workspace` 或挂载记录；后者说明记录与主库不一致，按 `Inconsistent` 报） |
| branch 不存在 | `NotFound{branch}` | "没有分支 b；`fs-cli branch b --from main`" |
| commit 不存在 | `NotFound{commit}` | 原样 + "`fs-cli log <branch>` 查看可用提交" |
| workspace 被占用 | `WorkspaceInUse` | 查记录：有挂载 ⇒ "挂载在 U:（pid N）"+ §6 对应去路；无挂载 ⇒ "另一个进程正打开它（另一条 fs-cli？）" |
| 主库忙 | `Busy{waited_ms}` | 原文 + "已等待 N ms；稍后重试"（ADR-006 §11.1：不自动重试） |
| GC 拒绝 | `StoreInUse` | 原文 + 列出活的挂载 |
| commit 分叉 | `BranchMoved{expected, found}` | 原文 + ADR-006 §10.3 的出路 |
| mount：盘符被占 0xC0000035 | 已有（`mountcmd.rs`） | 保持 |
| mount：挂载点是真实卷 0xC0000022 | 已有（e545089） | 保持 |
| mount：其它 | 打印四个码 | 保持 |
| 已移除的文件命令（`put/get/ls/stat/mkdir/rm/mv/cat`） | — | 常量 `FILE_COMMAND_REMOVED`：`<cmd>: file operations go through the mounted file system; fs-cli mount <mount_point> --workspace <name>`，退出码 2 |

### 8.3 退出码

现状一律 1（`main.rs:60`）。规定分档，供脚本区分"重试有意义"与"改命令才行"：

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 其它失败 |
| 2 | 用法错误（参数、**未指定 workspace**、已移除的文件命令） |
| 3 | 不存在（store / workspace / branch / commit） |
| 4 | 被占用或忙（`WorkspaceInUse`、`Busy`、`StoreInUse`、挂载语境拒绝） |
| 5 | 冲突（`BranchMoved`、`NothingToCommit`、`WorkspaceDirty`、`AlreadyExists`） |

## 9. 挂载记录（C6；答 manager 的补充问题）

### 9.1 放哪里：一处，不在 store 目录另存

- 路径发现的方向是 **挂载点 → (store, workspace)**：查的时候**还不知道 store**，所以索引必须在一个与 store 无关、
  按用户可找到的位置。store 目录**不能**作为这个索引。
- 反方向（"store S 的 workspace dev 挂着吗"，§6 / §8 的去路需要）用**同一批记录**按 `store` 字段过滤即可（挂载数个位数）。
  在 store 里再写一份 = 同一事实两处，挂载进程崩溃时两处的清理不同步——第二份真相。
- **生命周期正确**：这个映射只在挂载期间为真；持久化到挂载之后反而是谎言。所以"不持久"是对的，要解决的是**陈旧**与**被误删**。
- 位置从 `%TEMP%\layerfs-mounts\` 改为 **`%LOCALAPPDATA%\layerfs\mounts\`**：同为按用户隔离，但不在临时文件清扫范围内
  （§3.3：记录被清 = 挂载自行卸载）。按用户隔离与盘符的会话可见性大体一致；**以另一用户或提升权限运行**时看不到别人的挂载，文档写明。

### 9.2 存活证明：挂载进程持有的句柄

- 挂载进程以 `FILE_SHARE_READ | FILE_SHARE_DELETE`（**不给** `FILE_SHARE_WRITE`）加 `FILE_FLAG_DELETE_ON_CLOSE` 打开自己的记录文件并持有到退出。
  `DELETE_ON_CLOSE`：进程无论怎么结束（含 `taskkill /F`），内核关句柄时删除文件——陈旧记录只剩断电/蓝屏一种来源；`FILE_SHARE_DELETE`：读者打开带 delete-on-close 持有者的文件时必须声明 delete 共享，持有者须允许（S1 卡 §2.1 修订；初稿只给 READ，读者打不开）。
- 读者：能读内容；判活 = 尝试以写权限打开——**共享冲突 ⇒ 活**，成功 ⇒ 陈旧（持有者已死，内核在进程结束时必关句柄，
  `taskkill /F` 也一样）。比 `pid` 校验强：不受 pid 复用影响，不需要比对进程启动时间。
- 陈旧记录由**下一个读者**清理（能以写权限打开即可删除），并在 stderr 说明；`umount --force` 仍保留作显式清理。
- 记录内容（S1）：`pid`、`mount_point`（规范化）、`store`（规范化）、`workspace`、`branch`（挂载时的值，仅展示）、
  `started_at`、`pipe`（管道名，§10.1）；若 §4.4 需要，再加 `volume_guid`。

## 10. 挂载进程代理（正式设计）

### 10.1 通道

- **命名管道** `\\.\pipe\layerfs-<record>-<pid>`（`<record>` 即 `record_name(mount_point)`，`mountcmd.rs:38`），名字写进挂载记录 `pipe` 字段。
- 创建：`PIPE_ACCESS_DUPLEX`、`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS`，`nMaxInstances = 1`；
  安全描述符只允许**当前用户 SID**（不用默认 DACL：默认允许 Everyone 读）。
- **一次连接一个请求**：客户端连上 → 写一行请求 → 读一行响应 → 双方关闭。不做持久会话：无会话状态 = 无会话失步。
- **服务端一次只处理一个请求**（`nMaxInstances = 1`，§10.5）。忙时客户端 `WaitNamedPipeW` 等待，超时见 §10.2。
- 取代 stop 文件与"记录消失即停止"（`mountcmd.rs:238`）：两者都删除——停止只有一个入口（channel，§10.5），不再有两种互相竞争的信号。

### 10.2 帧、版本、超时

- 帧：**一行 UTF-8 JSON + `\n`**；单帧上限 `PIPE_FRAME_MAX = 64 KiB`（超限 ⇒ 服务端回 `FrameTooLarge` 并关闭）。
- 每个请求带 `"v": 1`；服务端不支持 ⇒ `{"v":1,"ok":false,"error":{"kind":"ProtocolVersion","supported":[1]}}`。
  CLI 与挂载进程是同一个二进制，版本不符只在"升级二进制时有旧挂载在跑"时出现；错误信息给出"umount 后用新版本重挂"。
- 超时（常量各一处，名字即语义）：

| 常量 | 值 | 用途 |
| --- | --- | --- |
| `PIPE_CONNECT_WAIT` | 2 s | `WaitNamedPipeW`；超时 ⇒ 判活：活 ⇒ `Busy`（"挂载进程正在处理另一个请求"）；陈旧 ⇒ 按 §7.2 |
| `PIPE_REPLY_WAIT_QUICK` | 30 s | `hello/status` |
| 无上限 | — | `commit/snapshot/restore/switch/unmount`：它们是原子的，客户端放弃等待不改变结果。客户端被 Ctrl-C 时打印"请求已发出，结果未知；用 `fs-cli status` 查看"并退出 |

### 10.3 发布锁（C12，API 改动）

- `fs_workspace::Workspace` 增 `publish: RwLock<()>`，并把发布会改写的字段（`branch`、`base_commit`、`base_root`）收进同一把锁保护的结构：

```rust
struct BaseState { branch: String, base_commit: String, base_root: ObjectId }
pub struct Workspace { /* … */ base: RwLock<BaseState>, /* 取代三个可变字段 */ }

// 视图操作（read_at / write_at / write_at_id / truncate / create / mkdir / remove / remove_at / rename / stat / read_dir / flush / status …）
fn op(&self, …) { let _base = self.base.read(); … }
// 发布与替换（签名由 &mut self 改为 &self）
pub fn commit(&self, msg: &str, force: bool) -> … { let mut base = self.base.write(); /* materialise → record → adopt */ }
pub fn snapshot(&self) -> …   // 同上
pub fn restore(&self, commit: &str, discard: bool) -> …
pub fn rebind(&self, branch: &str) -> …
```

- **锁序**（防死锁，一处写明、处处照做）：`base` 读写锁 → overlay 连接互斥 → `store` 互斥（`workspace.rs:30`）。**只有公共入口取 `base`**；
  内部函数（`materialise` 的 `encode` 回调里调的 `overlay.read_at` 等）不再取——否则写锁持有者重入读锁即自锁。
- **为什么读也取读锁**：`adopt_base` 是"删 overlay 行 + 改 meta"一个事务、再改内存里的 base（`fs-overlay/src/publish.rs:144-151`）；
  事务提交与内存更新之间若有读，会看到"空 overlay + 旧 base"。读锁让读者只能看到发布前或发布后。
- **阻塞代价**：commit 持写锁期间（materialise 遍历整棵视图 + 编码改动），所有文件操作在 WinFsp 分发线程上等待。上界约束：
  必须远小于 WinFsp 的 `IrpTimeout`（`VolumeParams` 置零 ⇒ 默认 `FspFsctlIrpTimeoutDefault = 300000` ms，SDK `fsctl.h:184-186,202`），
  超过则内核取消挂起的 IRP。S2 判据 A16 记录"commit 期间文件操作的最长等待"与视图大小的关系（带红值出档，不设通过线前先测）。
- `fs-api::Workspace` 同步改为 `&self`（`fs-api/src/workspace.rs:180,193,206`）；CLI 进程内（未挂载语境）调用不受影响。

### 10.4 op 规范

**op 只有 7 个，且没有文件 op**（C0）：初稿的 `flush` 已删除——没有任何 CLI 命令映射到它（`commit` 第 0 步自带 flush，`unmount` 的停止序列也 flush）。

所有响应：成功 `{"v":1,"ok":true, …}`；失败 `{"v":1,"ok":false,"error":{"kind":"<类型名>","message":"<人读>", …字段}}`。
`kind` 取核心错误的类型名（`WorkspaceError` / `ApiError` 变体名）或下表的代理自有错误；**CLI 按 `kind` 渲染 §8.2 的去路，不在客户端重新判断语义**。

| op | 请求 | 成功响应 | 代理自有错误 |
| --- | --- | --- | --- |
| `hello` | `{}` | `{store, workspace, branch, pid, mount_point, protocol:1}` | — |
| `status` | `{}` | `{workspace, branch, base_commit, dirty, handles}` | — |
| `snapshot` | `{}` | `{root}` | — |
| `commit` | `{message, force}` | `{commit, root}` | — |
| `restore` | `{commit, discard}` | `{}` | `HandlesOpen{handles}` |
| `switch` | `{workspace}` | `{workspace, branch}` | `HandlesOpen{handles}`；`WorkspaceInUse`（已挂回原 workspace） |
| `unmount` | `{wait_ms}` | `{}`（响应在卸载、flush 与删记录**之后**才发） | `HandlesOpen{handles}`（等满 `wait_ms` 句柄仍不为零；未发停止，状态不变） |

通用代理错误：`ProtocolVersion{supported}`、`FrameTooLarge`、`BadRequest{detail}`、`ShuttingDown`（停止序列已开始后到达的请求）。
核心错误照传，例：`{"kind":"BranchMoved","branch":"dev","expected":"…","actual":"…"}`、`{"kind":"NothingToCommit"}`。

### 10.5 挂载进程的线程结构（C11；答"A / B / C"）

**事实**：WinFsp 回调**不在**主线程上——`FspFileSystemStartDispatcher(fs, 2)`（`mounted.rs:901`）起两条 WinFsp 自己的分发线程，
回调在它们上面同步执行；主线程目前只在探针闭包里轮询 stop 文件（`mountcmd.rs:234-245`）。所以"单线程轮询会阻塞文件操作"的前提不成立；
真正的问题是**管道 I/O 放在哪条线程、停止信号怎么汇合**。

**选 B**：

```text
主线程（生命周期所有者）           管道线程（一个）                    控制台处理器（系统线程）
  open workspace（W2）               loop:                                on Ctrl-C/Break/Close:
  写记录 + 持句柄                       ConnectNamedPipe（阻塞）              tx.send(Stop{reason})
  mount（分发线程开始服务）             读一行 → 分派：                       return TRUE
  loop: match rx.recv():                 hello/status/flush/snapshot/commit/restore
    Stop{reason, reply} ⇒ 跳出            ⇒ 本线程直接调 Workspace（C12 锁保护）
    Switch{ws, reply}  ⇒ 停→换→重挂      unmount/switch ⇒ tx.send(…, reply_tx)，等 reply_rx
  停止序列：flush → 卸载 → 删记录        写一行响应，断开
  reply.send(ok)                        收到 ShuttingDown 标志 ⇒ 对新请求回 ShuttingDown
```

- **为什么不是 A（在主线程轮询里查管道）**：要么非阻塞/重叠 I/O 加轮询（每个请求多 ≤ 50 ms 延迟，且实现比一条阻塞线程更复杂），
  要么阻塞 `ConnectNamedPipe` 让主线程收不到停止信号。它省掉的只是一条线程，而这条线程的共享状态只有 `&'static Workspace`
  （本来就 `Sync`，被分发线程共享）与一条 channel。
- **为什么不是 C（async）**：WinFsp 回调是同步的（我们同步返回状态，不用 `STATUS_PENDING`），async 运行时只能包住管道这一端；
  多一个运行时依赖，换来的并发（同时服务多个管道请求）恰恰是我们**不要**的——发布本来就要串行。
- **正确性论证只需三条**：① 文件操作与代理操作的互斥由 C12 的锁保证，与线程从哪来无关；② 停止只有一个入口（channel），
  不存在 stop 文件与记录消失两路信号的竞争；③ 管道线程不持有任何跨请求状态，一个请求失败不影响下一个。
- **退出**：主线程完成停止序列后关闭管道句柄（使阻塞中的 `ConnectNamedPipe` 以错误返回），join 管道线程。

### 10.6 句柄语义（S2 判据的依据）

- **commit/snapshot 后 FileId 不变**：物化把每个可达 inode 的 serial 原样编码进新根（`materialize.rs:181,224-226`），`adopt` 后视图 = 新根，
  同一 serial 仍可达 ⇒ 同一 FileId（ADR-009 G2 按 FileId 读写）。**这是从代码推出的结论，判据 A13 直接验证**。
- 不可达的行（`remove_at` 的 tombstone）不进新根，`adopt` 后消失；它们按 ADR-009 §5.4 本来就没有打开句柄（两段删除在最后一个 Cleanup 才 `remove_at`），
  rename-replace 有句柄的保留路径已由 P3-4 U1 证明不会发生（FSD 拒绝）⇒ **commit 不会让任何打开句柄失去对象**。
- delete-pending 中的文件（已标删除、句柄未关）在 commit 时仍有名字 ⇒ 进入新根；最后一个 Cleanup 时照常 `remove_at` ⇒ 下一次 commit 才消失。与 NTFS 一致（删除挂起期间名字仍在）。
- 适配器侧的句柄缓存（size 等）在 commit 前后**内容不变**，无需失效。restore/switch 才改变内容，故要求无句柄（§6.2、§7.3）。

## 11. 替代方案（已否决）

| 方案 | 否决理由 |
| --- | --- |
| 保留 `cli/current` 作未挂载时的默认（architect 初稿推荐） | **owner 否决（D1）**：隐藏状态决定命令含义；§4.1 |
| mount 更新 current（owner 第 1 点） | 随 current 一并消失；初稿的否决理由（W2 下有害、历史依赖、崩溃不一致）记于 §4.1 |
| `mount` 缺省挂 `main` | 隐式默认，与 C1 同类（C3） |
| 挂载记录写进 store 目录 | §9.1：查询方向不对 + 第二份真相 |
| 只用 pid 判活 | pid 复用；需比对启动时间且仍有竞态。句柄判活（§9.2）严格更强且更简单 |
| 挂载期间 CLI 文件命令"绕过"挂载直接写 overlay | 违反 W2；即便锁允许，也是同一 workspace 的第二条写路径，挂载侧缓存（FileNode、size 缓存）不会知道 |
| 挂载期间 CLI 文件命令经管道代理 | 绕过 FSD（share、delete-pending、`FileNode` 缓存），造出第二条写路径 |
| 挂载期间 CLI 文件命令经挂载点执行（上一稿方案） | 可行且语义一致，但 **owner 裁决 D7：CLI 不做文件操作**——用户直接在挂载点上用任何工具即可，CLI 再包一层没有增量 |
| 保留历史读 `cat/ls <commit>:<path>` | D7 一并去掉；需要看历史时挂载一个提交（§14 D7 注） |
| 挂载期间文件/工作区命令一律拒绝、要求先 umount（初稿 S1 方案） | **owner 否决（D2/D3）**：打扰用户；代理与经挂载点执行都可做到语义一致 |
| 挂载进程事件循环 A（主线程轮询管道）/ C（async） | §10.5 |
| 代理期间暂停 WinFsp 分发（不加锁） | WinFsp 无暂停原语，需在每个回调入口自己挡——就是一把锁，只是放在了更远、更难论证的位置 |
| 卷内放一个虚拟文件（如 `U:\.layerfs`）供反查 | 污染命名空间，`dir /a`、备份工具都会看见——违反"与普通文件系统一致" |

## 12. 影响面

| 位置 | 变化 | 期 |
| --- | --- | --- |
| `fs-cli/src/main.rs:117-187` 文件命令 `put/get/ls/cat/stat/mkdir/rm/mv` 的分支，及只为它们存在的辅助函数（`read_all`、`print_entries`、`print_stat`、`split_commit`、`one_or_two`、`two` 中不再被用到的那些） | **删除**；命令名改为返回 `FILE_COMMAND_REMOVED`（§8.2）；`USAGE`（`main.rs:19-52`）同步删去 | S0 |
| `fs-cli/src/main.rs:460` `current_branch` | 改走 `workspace_list()` | S0 |
| `fs-cli/src/main.rs:465-485` `current_path/read_current/write_current` | **删除**；`init`（`:86`）与 `checkout`（`:359`）不再写；`with_workspace`（`:448`）、`snapshot create`（`:273`）改用解析器（§4.2） | S0 |
| `fs-cli/src/main.rs:344-360` `checkout` | 存在性改走 `workspace_list()`；无副作用；输出下一步 | S0 |
| `fs-cli/src/main.rs` `log/reset/revert` | 分支来源按 §4.3（加 `<branch>` / `--branch`） | S0 |
| 新增全局 `--workspace` 与解析器（§4.2 的显式部分；发现部分 S1 接入同一解析器） | 去掉 current 后它是唯一来源，必须与删除同批 | S0 |
| 消息常量 `NO_WORKSPACE` | S0：`no workspace specified; use --workspace <name>`；S1 接入路径发现时**同一常量**追加 `, or cd into a mount point` | S0/S1 |
| `fs-cli/src/mountcmd.rs:178` | 删除缺省 `"main"`，`--workspace` 必填 | S0 |
| `fs-cli/tests/cli.rs`、`cli_defaults.rs` | 见 §13 S0 的"测试处置表" | S0 |
| `WorkspaceInUse` / `StoreInUse` 渲染 | 查记录，带挂载点与 pid | S0 |
| `fs-cli/src/mountcmd.rs:62-72` 记录目录 | `%LOCALAPPDATA%\layerfs\mounts\` + 句柄判活 | S1 |
| 路径发现（§4.4） | Win32 两个调用 + 记录扫描 | S1 |
| §6 分流 | 每条命令一个类别常量，表驱动 | S1 |
| Ctrl-C 处理（§7.1） | `SetConsoleCtrlHandler` | S1 |
| 退出码（§8.3） | | S1 |
| `fs-cli/src/mountcmd.rs:217-245` stop 文件与"记录消失即停止" | 删除；改为 channel + 管道 `unmount`（§10.1、§10.5） | S1 |
| 管道服务（`hello/status/unmount`）与客户端；管道线程 + channel | §10 | S1 |
| `fs-workspace::Workspace` 发布锁、`BaseState`、`commit/snapshot/restore/rebind(&self)`；`fs-api` 同步 | §10.3 | S2 |
| 管道 `commit/snapshot` | §10.4 | S2 |
| 适配器暴露打开句柄数（注册表计数） | `status.handles`、`unmount` 的 `HandlesOpen{handles}` 用 | S1 |
| 管道 `restore/switch` | §6.2、§7.3 | S3 |
| ADR-006 §13 CLI 段 | 标"已由 ADR-010 取代" | 本 ADR 合入时（已在本分支） |
| ADR-006 §13 的 `ls|get|stat --at <commit|root>`（I1 验收入口） | 已随 D7 去掉；I1 的验收由 fs-api 层的 `ReadView` 测试承担（不经 CLI） | 本 ADR 合入时记录 |

## 13. 分期与判据

判据形态沿用 criteria-and-acceptance：三态（通过 / 失败带红值 / 空转）、每条带能把它弄红的单点变异。

### S0（可马上派 worker：CLI 收窄——去掉文件命令与 current、修三处不一致、去路）

**判据影响：改变**——
- `cli_defaults.rs::init_leaves_a_workspace_ready_to_use`：原判据"`init` 后直接 `put/ls` 成功"**作废**（命令已不存在），由 A0c 取代；
- `cli.rs::the_full_command_sequence_round_trips`：原判据含 `put/get/ls/cat` 往返，**改写**为"内容经 fs-api 写入、经 CLI 生命周期命令发布、经 fs-api `ReadView` 读回"（见下表）；
- 其余依赖 current 或文件命令**做准备**的用例：准备步骤改为 fs-api 库调用，被测的 CLI 行为判据不变。

测试处置表（逐个用例，不得漏项；"准备"指只为构造场景而调用的文件命令）：

| 用例（`fs-cli/tests/`） | 原来用文件命令做什么 | 处置 |
| --- | --- | --- |
| `cli_defaults.rs::init_leaves_a_workspace_ready_to_use` | 被测行为本身 | **作废**，由 A0c + A0 取代（判据改变） |
| `cli_defaults.rs::a_taken_drive_letter_says_what_to_do` | 不用 | 加 `--workspace main`（A2） |
| `cli.rs::the_full_command_sequence_round_trips` | 往返的一半 | 改写：fs-api 写 → `fs-cli commit/log/status/restore` → fs-api `ReadView` 读（判据改变：读写两端从 CLI 换到库） |
| `cli.rs::failures_are_reported` | 部分失败形态（不存在的路径等） | 保留 workspace/branch/commit 三种不存在；路径不存在一项**删除**（命令已无） |
| `cli.rs::restore_refuses_to_drop_uncommitted_work` | 准备脏改动 | 准备改用 fs-api |
| `cli.rs::a_branch_can_start_from_a_commit` | 准备内容、验证内容 | 准备与验证改用 fs-api |
| `cli.rs::gc_dry_run_and_real_run_are_different_commands` | 准备垃圾 | 准备改用 fs-api |
| `cli.rs::an_impossible_repack_threshold_is_reported_and_not_acted_on` | 准备 | 准备改用 fs-api |
| `cli.rs::dropping_a_snapshot_frees_only_what_nothing_else_reaches` | 准备、`cat` 验证 | 准备与验证改用 fs-api |
| `cli.rs::deleting_a_branch_keeps_its_objects_alive` | 准备、`cat` 验证 | 同上 |
| `cli.rs::verify_deep_checks_snapshot_roots` | 准备 | 准备改用 fs-api |

处置的约束：**被测的 CLI 行为判据一条不少**（gc 的两种模式、repack 阈值拒绝、snapshot drop 的可达性、branch delete 保留对象、verify --deep、restore 拒绝脏改动）；
准备与验证换成库调用后，每条仍须能被它原来的变异弄红（提交说明逐条列"变异 → 红"）。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A0 | 夹具 store 里**故意放一个** `cli/current`（内容 `main`），cwd 不在挂载内、不给 `--workspace`：`commit/status/restore/snapshot create` 全部失败，stderr 含常量 `NO_WORKSPACE`（§8.2；断言引用常量，不在测试里另写字面量），退出码 2 | 恢复读取 current（任一条命令）；或改写消息 |
| A0b | `init` 与 `checkout` 之后 store 里**不存在** `cli/` 目录 | 恢复 `write_current` |
| A0c | `put/get/ls/stat/mkdir/rm/mv/cat` 每一个都失败，stderr 含常量 `FILE_COMMAND_REMOVED`、退出码 2，且**未打开 store**（对一个不存在的 `--store` 也给同一消息，而不是"store 不存在"） | 恢复任一文件命令；或在判命令之前先打开 store |
| A1 | workspace `dev` 被另一进程持有时，`log dev`、`reset <c> --branch dev --expect <c>`、`revert --branch dev`、`--workspace dev log` 成功（取分支不打开 overlay） | 分支来源改回 `workspace_open` |
| A2 | `mount <mp>` 不带 `--workspace` ⇒ 失败，消息含 `--workspace`，**且未挂载**（盘符仍空、无记录） | 缺省改回 `"main"` |
| A3 | 撞 `WorkspaceInUse` 且有该 workspace 的活挂载时，错误含挂载点与 pid | 渲染不查记录 |
| A3b | `checkout <b>` 在 `<b>` 正被另一进程持有时成功并报告其状态；`checkout nope`（分支不存在）⇒ `NotFound` 且去路含 `branch nope --from` | 存在性改回 `workspace_open` |

### S1（依赖 S0；不改核心，只动 CLI 与挂载进程）

范围：记录迁移与句柄判活（§9）、路径发现（§4.4）、Ctrl-C（§7.1）、退出码（§8.3）、适配器句柄数、**管道骨架**（线程结构 §10.5、
帧/版本/超时 §10.2、op `hello/status/unmount`）。`status` 进入 S1：它只读，经管道不需要发布锁。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A4 | **跨进程 W2**：进程 A 持有 `dev`（真读一次证明活着），进程 B `workspace_open(dev)` 得到 typed `WorkspaceInUse`；**删除挂载记录后**仍拒绝（锁层独立于记录层）；A 退出后 B 成功（阴性对照） | 去掉 `locking_mode=EXCLUSIVE`；或把 open 改成不触发首次加锁 |
| A5 | 同一 `(store, workspace)` 第二次 mount 被拒，错误指出第一个挂载点（关闭 B-22） | 去掉记录层检查（A4 仍拦，但错误文本不含挂载点 ⇒ 本条红） |
| A6 | §4.2 组合表 7 行各一用例；"`--store S` 与发现的 store 不同"必须丢弃发现并报 C2 消息 | 拆开混用（取 S 加 M.workspace） |
| A7 | 陈旧记录（持有进程已死，含 `taskkill /F`）⇒ 不进入挂载语境，stderr 提示；记录被下一个读者清理；`umount --force` 对**活**记录拒绝 | 判活改为"文件存在即活" |
| A8 | §6 表每一行至少一条命令的用例 | 任一类别常量改错 |
| A10 | Ctrl-C 后：卷卸载、记录删除、Ctrl-C 前的写入在重新挂载后可读 | 去掉控制台处理器（退化为 M7 残留） |
| A11 | 管道：`hello` 返回与记录一致；`v:2` 请求 ⇒ `ProtocolVersion`；超限帧 ⇒ `FrameTooLarge` 且服务端继续服务下一个请求；`unmount` 的响应在记录删除之后到达 | 响应提前发送；或帧错误后不再 accept |
| A12 | 有打开句柄时 `umount` ⇒ `HandlesOpen{handles≥1}`，卷仍挂、记录仍在；关闭句柄后再 `umount` 成功（细化见 S1 卡 §5.4） | 超时后仍发停止 |
| A14 | `status` 经管道：挂载中写一个文件后 `status.dirty = true`、`handles` 与实际一致 | — （读值核对，三态带红值） |

### S2（依赖 S1；改核心 API）

范围：发布锁与签名（§10.3）、op `commit/snapshot`、句柄语义（§10.6）。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A15 | **L1 丢写复现**（不需要挂载）：N 条线程各自对**自己的文件**写递增序号（每次写成功返回后记下该序号），同时另一线程循环 `commit`；全部停止后，**当前视图**（= 最后一次 commit 的树 ⊕ overlay）里每个文件的内容 == 该文件最后一次成功返回的序号；再 commit 一次后从历史 `cat` 读到同样的值 | 去掉写锁（commit 不取 `base.write()`）⇒ 必须出现丢写；若 1000 轮内未出现 ⇒ **空转**，须加注入点（materialise 与 adopt 之间的 `crash_point` 换成可配置的停顿）后重跑 |
| A16 | commit 期间文件操作的最长等待 vs 视图大小（1k/10k/100k 条目）记录出档，带红值；与 `IrpTimeout` 默认值对照 | — （度量） |
| A17 | **句柄身份（L2，真挂载）**：`open(h)` → `write(h,"a")` → 管道 `commit` → `read(h) == "a"` 且 `FileIndex(h)` 不变 → `write(h,"b")` → `status.dirty` → 第二次 `commit` 成功；目录句柄在 commit 前后 `ReadDirectory` 结果一致 | `adopt` 后重新分配 serial（模拟"身份不稳"）|
| A18 | `commit` 经管道的 `BranchMoved` / `NothingToCommit` 原样到达 CLI，渲染 §8.2 去路 | 客户端自行判断语义 |

### S3（依赖 S2；替换视图类）

范围：op `restore/switch`（§6.2、§7.3；句柄数已在 S1）。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A19 | 有句柄时 `restore`/`switch` ⇒ `HandlesOpen{handles}` 且视图/挂载不变 | 去掉句柄检查 |
| A20 | 无句柄时 `restore` 后重新打开文件读到**新**内容（同时验证 V6：`FileInfoTimeout=0` 下无数据缓存） | — （若读到旧内容 ⇒ 失败带红值，S3 停卡，设计回到"需 `FspFileSystemNotify` 失效"，依赖 U9） |
| A21 | `switch` 到被别处持有的 workspace ⇒ `WorkspaceInUse`，且原 workspace 已挂回、记录未变 | 失败时不挂回 |
| A22 | `switch` 成功后记录 `workspace/branch` 更新，判活不间断（另一进程在 switch 全程判活均为"活"） | 换挂时关闭并重开记录句柄 |

### 依赖总览

```text
S0 ──► S1 ──► S2 ──► S3
        │       └─ 依赖 C12 发布锁（fs-workspace + fs-api 签名）
        └─ 管道骨架在 S1（不是 S0）：S0 只做"去掉 current"这一件语义变化，判据影响已是"改变"，不再夹带新机制
```

## 14. owner 裁决记录（2026-10-26，全部已裁）

| # | 问题 | 裁决 |
| --- | --- | --- |
| D1 | `cli/current` 的去留 | **去掉**；cwd 不在挂载内时必须显式（C1） |
| D2 | 挂载期间的工作区级命令 | **代理**：CLI 把必要的工作交给挂载进程，尽量不打扰用户（C4、§10） |
| D3 | 挂载期间 status 等只读操作、挂载语境下 checkout | **同样代理**；checkout = 无句柄时换挂（C10、§7.3） |
| D4 | Ctrl-C | **视为正常 umount**（C7） |
| D5 | 二次挂载 | **两层防护**：记录层先拒 + W2 锁层兜底（C8） |
| D6 | `init` 是否顺带建 workspace `main` | **保留**（真实对象，非隐藏状态） |
| D7 | CLI 的文件级命令 | **全部去掉**（`put/get/ls/stat/mkdir/rm/mv/cat`）；CLI 只管生命周期与挂载服务，文件操作属于挂载后的文件系统（C0） |

D7 的后果（记录，不另需裁决）：CLI 不再能读历史文件。需要查看某个提交的内容时：`restore` 到一个 workspace 后挂载它；若将来要"不动 workspace 直接看历史"，形态应是**只读挂载一个提交**（`mount <mp> --at <commit>`），仍经文件系统——不加回 `cat`（§5 末段）。不排期。

注：本稿初版的 D4（后台挂载）与 D5（退出码分档）与 manager 转达时的编号不同。后台挂载已移出本 ADR（§7.1）；
**退出码分档（§8.3）按 architect 推荐采纳，待 owner 追认**——它是对脚本的对外承诺，若 owner 不同意，只回退 §8.3 一节，其余不受影响。

## 15. 未验证清单

| # | 项 | 验证于 |
| --- | --- | --- |
| V1 | 跨进程 `WorkspaceInUse`（B-14b）与 M4"二次挂载无闸"（B-22）的矛盾 | S1 A4/A5 |
| V2 | WinFsp 目录挂载点下 `GetVolumePathNameW` 的返回值；`subst`/网络盘 | S1 L2 |
| V3 | `%TEMP%` 清扫是否会删除活记录（决定 §9.1 迁移的紧迫度，不决定方向） | 不必验证：方向已定 |
| V4 | `CTRL_CLOSE_EVENT` 的约 5 s 内 flush 是否总能完成（大 overlay） | S1 A10 加大 overlay 变体 |
| V5 | 代理 commit 前后 FileId / 句柄有效性（§10.6 从代码推出） | S2 A17 |
| V6 | `FileInfoTimeout = 0` 时 WinFsp 不启用数据缓存（决定 restore 无句柄代理是否安全） | S3 A20 |
| V7 | commit 持写锁的最长时长相对 `IrpTimeout`（5 min 默认）的余量 | S2 A16 |

## 修订记录

- 2026-10-26 初稿 `6d733f8`：保留 `cli/current` 作未挂载默认（architect 推荐）。
- 2026-10-26 按 owner 裁决 D1 修订：**去掉 `cli/current`**（C1）；workspace 来源 = `--workspace` > 路径发现 > 报错（C2，消息逐字固定）；
  `mount` 必须显式 `--workspace`（C3，删除缺省 `"main"`）；`checkout` 改为无副作用的创建/恢复（C10、§7.3）；新增 §4.3 分支来源；
  S0 判据增 A0/A0b/A3b、改写 A1/A2，判据影响 = 改变（`cli_defaults.rs::init_leaves_a_workspace_ready_to_use`）；新增 D6。
- 2026-10-26 终稿（Accepted）：owner 裁决 D2–D6。挂载期不再拒绝——文件级命令经挂载点执行（§6.1），工作区级经管道代理（§10）；
  §10 由草案升为正式设计：管道通道/帧/版本/超时、8 个 op、发布锁 C12（`&mut` → `&self` + `RwLock`，并写明它防的丢写窗口 §3.4）、
  线程结构 C11（选 B）、句柄语义 §10.6；分期改为 S0–S3（管道骨架在 S1），判据 A4–A22。
- 2026-10-26 按 owner 裁决 D7 修订：**去掉全部文件级命令**（C0）；§5 收为"只剩宿主路径"，初稿的 `//path`/相对路径/双拼写问题整类消失；
  §6.1 改为"文件操作不在 CLI 里"；管道 op 去掉 `flush`（7 个，无文件 op）；S0 增 A0c 与逐用例的测试处置表，删 A9、A13a；新增 D7。

- 2026-10-28 随 S1 卡（`docs/process/adr010-s1-card.md`）澄清：停止顺序改为"卸载 → flush → 删记录"（§7.1）；远程 umount 有句柄时 `HandlesOpen` 且不改状态、控制台停止不看句柄（§7.2）；记录以 `DELETE_ON_CLOSE` + `READ|DELETE` 共享打开（§9.2）；`unmount` 错误统一名为 `HandlesOpen`（§10.4）。判据影响：无（澄清）。

### §4.4 补充：目录挂载点不支持路径发现（A6d 读数，2026-10-28）

目录挂载点的卷根是宿主卷（如 `F:\`），卷标不是 `layerfs` ⇒ `discover` 永远不认。
用户 `cd` 进目录挂载点后 `status` 等命令仍需显式 `--store`/`--workspace`。
修法（`VOLUME_NAME_GUID` 匹配挂载记录的卷 GUID）留 S2。
