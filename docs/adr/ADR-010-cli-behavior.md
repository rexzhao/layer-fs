# ADR-010：CLI 行为模式（workspace 定位、路径、挂载生命周期、多挂载、错误）

- 状态：**草案（Proposed）**——architect 起草，2026-10-26；manager review 后交 worker。**owner 已裁 D1（2026-10-26）：去掉 `cli/current`**，本稿已按裁决修订（见文末修订记录）；§14 其余裁决点待 owner
- 日期：2026-10-26
- 起草：architect
- 依据：owner 2026-10-26 两条意见（"mount 后 current 应跟着变"、"应能从当前路径所属挂载点反查 workspace/branch，
  不需要 CLI 自存 `cli/current`"）、owner 裁决 D1（"去掉 cli/current，如果 cwd 不在活的挂载内，必须显式指定 workspace
  或者目标路径，就像 git 一样"）与总原则（**用起来与普通文件系统的真实行为一致**）；ADR-006 §2 W2、§10、§11、§13；
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
| C1 | **去掉 `cli/current`（owner 裁决 D1）**：CLI 不保存任何"当前 workspace"状态；不读、不写、不作回落。workspace 只来自显式参数或路径发现 |
| C2 | **定位优先级**：`--workspace` 显式 > **路径发现**（cwd 在一个活的挂载内）> **无默认：报错** `no workspace specified; use --workspace <name>, or cd into a mount point`；store：`--store` > 路径发现 > `.`。路径发现给出的是 **(store, workspace) 二元组**，不与显式 `--store` 拆开混用（§4.2） |
| C3 | **`mount` 必须显式 `--workspace`**：挂载前 cwd 不可能在"这个"挂载内，路径发现不适用；现硬编码的缺省 `"main"` 是隐式默认，与 C1 同类，删除 |
| C4 | **挂载期间按命令类别分流**（§6 表）：仓库级只读命令照常；文件级命令**拒绝并指路到挂载点**（挂载期的文件操作就走文件系统本身——总原则）；工作区级写命令 **S1 拒绝并指路，S2 经挂载进程代理** |
| C5 | **路径**：CLI 的 workspace 路径是 ADR-003 §3.1 的绝对路径 `/a/b`；CLI 入口唯一一处归一化：前导 `//` 折叠为 `/`、`\` 视为 `/`；识别 Git Bash 把 `/a` 改写成 `C:/…/Git/a` 的形态并给出去路；相对路径**只在挂载语境**下有锚点（§5） |
| C6 | **挂载记录**：一条记录、一个位置（`%LOCALAPPDATA%\layerfs\mounts\`），由挂载进程**持有不可写共享的句柄**作为存活证明；**不在 store 目录另存一份**（§9） |
| C7 | **mount 生命周期**：前台进程；Ctrl-C / Ctrl-Break / 关闭控制台 = 一次正常 umount 请求（flush → 卸载 → 删记录）；`umount` 语义保持（正常 / 超时 / `--force` 只清陈旧记录并如实说明） |
| C8 | **同一 workspace 不得被两个打开者持有**：两层——记录层（友好错误，指出已挂在哪里）+ 锁层（W2，真正的保证）；跨进程锁层**目前无判据**（B-14b / B-22），本 ADR 把它列为 S1 判据 |
| C9 | **错误**：按"用户下一步做什么"分类并带去路；区分 store / workspace / branch / path 四种"不存在"；退出码分档（§8.3） |
| C10 | **`checkout <b>` = 创建或恢复名为 `<b>` 的 workspace，无副作用**（不再"切换"任何东西）；不改变任何运行中的挂载；在挂载语境里"换分支" = 换挂（S2，需无打开句柄），S1 给出 `umount`+`mount` 两步（§7.3，owner 裁决 D3） |

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

### 3.4 工作区级写操作要 `&mut Workspace`

`Workspace::commit / snapshot / restore` 取 `&mut self`（`fs-api/src/workspace.rs:180,193,206`）；挂载进程持有的是
`&'static Workspace`（共享引用，回调线程并发使用）。⇒ **"挂载进程代理 commit"需要先改 API 形状**（§10.2），不是只接一条管道。

## 4. workspace 定位

### 4.1 无默认（C1，owner 裁决 D1）

- CLI **不保存**"当前 workspace"。cwd 不在活的挂载内、又没有 `--workspace` 时，需要 workspace 的命令一律报错（§8.2），
  像 git 在没有工作目录时必须显式给 ref 一样。
- **为什么这比"保留 current"好**（architect 原推荐保留，owner 否决；理由记在这里供后来者）：
  - 命令的含义只由**命令行 + cwd** 决定，不由一个看不见的文件决定；脚本在任何机器、任何先后顺序下行为相同；
  - 从根上消除了"current 要不要随挂载变"这一整类问题（初稿的三条：W2 下有害、历史依赖、崩溃不一致）——没有状态就没有同步；
  - 回到 ADR-006 §13 的显式设计（§3.2）。
- 代价（接受）：未挂载时每条 workspace 命令都要 `--workspace`。交互便利由路径发现承担：`cd U:\` 之后不需要任何参数。
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
| `cat` / `ls <commit>:<path>` | 历史读，commit 已显式 | **不需要**分支或 workspace（owner 原话里的"或者目标路径"） |
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
3. cwd 在卷内的**相对位置**（`U:\docs\a` ⇒ `/docs/a`）同时得出，供 §5.3 的相对路径使用。

## 5. 路径规则（C5）

### 5.1 一种规范形

CLI 的 workspace 路径 = ADR-003 §3.1 的绝对路径：`/`、`/a`、`/a/b`。核心不做归一化（`fs-namespace/src/path.rs:1-5`）；
CLI 作为入口，在**唯一一个函数**（`cli_path()`）里做以下三条，之后原样交给核心：

| 输入 | 处理 | 理由 |
| --- | --- | --- |
| 前导 `//a/b` | 折叠为 `/a/b`（只折叠前导，内部 `//` 仍按核心规则拒绝） | Git Bash 用 `//` 逃避路径改写，MSYS 会去掉一个 `/`；在 cmd/PowerShell 里同一脚本会把 `//a` 原样交来 → 核心报 `EmptySegment`。折叠后两种 shell 行为一致 |
| `\a\b` | 视为 `/a/b` | Windows 名字不能含 `\`（fs-semantics 拒绝）；与挂载适配器在 FFI 边界的 `\`→`/`（P3-4 §6）同一规则，同一文件两种入口不该有两种拼写规则 |
| 形如 `C:/Program Files/Git/a` 且环境变量 `MSYSTEM` 已设置 | **拒绝**并提示："Git Bash 把 `/a` 改写成了 Windows 路径；用 `//a` 或 `MSYS_NO_PATHCONV=1`" | 现状是核心报 `NotAbsolute`，用户看不出原因 |

`//path` **不是 CLI 语法**，是 shell 的逃逸写法；文档与 `--help` 以 `/path` 为准，并给出 Git Bash 注记。

### 5.2 两个命名空间，不混用

| 参数 | 命名空间 | 解析者 |
| --- | --- | --- |
| `put <local> …`、`get … [local]` 的 local | **宿主路径** | OS（相对 cwd） |
| 其余 `<path>` | **workspace 路径** | `cli_path()` |
| `<commit>:<path>` 的 path | workspace 路径（历史树内） | `cli_path()` |

同一个文件在挂载时有两种访问方式：`U:\a\b`（宿主路径，经 WinFsp）与 `/a/b`（workspace 路径，经 CLI）。
**S1 不接受把挂载点内的宿主路径当 workspace 路径**（`fs-cli stat U:\a\b` 报"这是宿主路径"并给出 `/a/b`）；
自动翻译列为可选（§12 A5）。

### 5.3 相对路径

- **未挂载语境**：workspace 路径没有锚点 ⇒ 相对路径拒绝："workspace 路径是绝对的（/a/b）"。
- **挂载语境**：锚点 = cwd 在卷内的位置（§4.4 第 3 步）；`U:\docs> fs-cli cat HEAD:a.txt` ⇒ `/docs/a.txt`。
  只用于挂载语境允许的命令（§6 表中"照常"的历史读）——文件级命令在挂载语境本来就拒绝。
- `.`/`..` 段：相对路径解析时按词法处理一次（不跨越卷根），结果再交核心；绝对路径里的 `.`/`..` 仍由核心拒绝。

## 6. 挂载期间的命令分流（C4）

"挂载语境" = 解析出的 workspace 正被一个活的挂载持有（不论 cwd 是否在挂载内——`--workspace dev` 指向一个挂着的 `dev` 也算）。

| 类别 | 命令 | 挂载期间 | 实现要点 |
| --- | --- | --- | --- |
| 仓库级、只读、不需要 overlay | `log`、`branch`（列出）、`cat <c>:<p>`、`ls <c>:<p>`、`stats`、`verify`、`list`、`version`、`snapshot list` | **照常** | `log/reset/revert` 取分支按 §4.3，走 `workspace_list()` |
| 仓库级、写主库、不碰该 overlay | `branch <new>`、`branch delete`（另一分支）、`reset`、`revert`、`snapshot drop` | **照常**；`reset/revert` 作用于被挂载 workspace 的分支时打印警告：挂载侧下次 commit 会按 ADR-006 §10.3 得到 `BranchMoved` | 不新增语义 |
| 文件级 | `put/get/ls/stat/mkdir/rm/mv`（无 `<commit>:`） | **拒绝**：`workspace dev 挂载在 U:（pid N）——请直接在 U: 上用普通文件操作，或先 umount U:` | 总原则：挂载期的文件操作走文件系统本身；CLI 不做第二条写路径 |
| 工作区级写 | `commit`、`status`、`snapshot create`、`restore` | **S1：拒绝并指路**（`umount` 后再执行）；**S2：`commit/status/snapshot create` 经挂载进程代理**（§10）；`restore` S2 仍拒绝（§10.3） | |
| 维护 | `gc` | **拒绝**（ADR-008 §6.1、B-9）；错误信息列出活的挂载 | 已有 `StoreInUse`，补去路 |
| 生命周期 | `workspace discard dev` | **拒绝**：先 umount | 删一个正在服务的 workspace 等于从挂载下抽走存储（`branch delete` 在 workspace 仍在该分支上时本就拒绝，`main.rs` `workspace()` 注释，不另加） |
| 同一 workspace 再挂 | `mount V: --workspace dev` | **拒绝**：`dev 已挂载在 U:` | C8 |

## 7. 生命周期

### 7.1 mount（C7）

1. 解析 store（§4.2）；`--workspace` **必须显式**（C3），缺省报 `no workspace specified; use --workspace <name>`；
2. 版本闸（B-23，现有）；
3. **记录层互斥**：存在指向同 `(store, workspace)` 的活记录 ⇒ 拒绝（C8 第一层）；
4. `workspace_open`（W2 锁，C8 第二层）；
5. 写记录并**持有其句柄**（§9.2）；挂载；前台阻塞；
6. 停止条件：`umount` 请求（S1 仍为 stop 文件；S2 改为管道 `Unmount` 请求）、**Ctrl-C / Ctrl-Break / 控制台关闭**
   （`SetConsoleCtrlHandler`；现状无处理，Ctrl-C 直接结束进程 ≈ M7 的残留形态）；
7. 停止序列：`flush`（W3/W4：使挂载期全部写入持久）→ 卸载 → 关闭并删除记录。第二次 Ctrl-C（5 s 内）= 立即退出，
   打印"未 flush 的写入可能丢失（最后一次 flush 之前的都在）"。
   - `CTRL_CLOSE_EVENT` 系统只给约 5 s：flush 是一次 `synchronous=FULL` 提交，正常远小于此；**未验证**大 overlay 下的上界。

后台挂载（`mount --detach`）**不在本 ADR**：它需要服务化或分离进程，属 owner 产品决策（§14 D4）。

### 7.2 umount

| 情形 | 行为（保持现状语义） |
| --- | --- |
| 正常 | 发停止请求，等待 ≤ `UNMOUNT_WAIT`（10 s，`mountcmd.rs:28`）记录消失 ⇒ "unmounted" |
| 超时（通常是有打开句柄，M5） | 报"未在 10 s 内卸载"，**不改任何状态**；S2 起列出持有句柄数（B-26） |
| `--force` | 只删陈旧记录，并如实说明"卷可能仍挂着，结束挂载进程"（WinFsp 句柄在对方进程里——`mountcmd.rs:1-12` 的诚实原则保留） |
| 无参数且 cwd 在挂载内 | 卸载该挂载（便利；显式参数优先） |

### 7.3 checkout 与挂载（C10）

- 新语义：`checkout <b>` = **创建或恢复**名为 `<b>` 的 workspace（分支 `<b>`）。不存在则 `workspace_create(b, b)`；
  存在则不切换任何东西，只报告状态（若被挂载，含"挂载在 U:"）。**无副作用**：不写任何 CLI 状态（C1）。
  输出给出下一步：`workspace b ready; use --workspace b, or mount it: fs-cli mount <mp> --workspace b`。
- 存在性检查走 `workspace_list()`，**不打开 overlay**（现 `main.rs:344-360` 打开它——`<b>` 正被挂载时 checkout 会失败）。
  中断发布的恢复（ADR-006 §9.3）由下一次真正的 `workspace_open` 完成，checkout 不必为此打开。
- 分支 `<b>` 不存在 ⇒ `NotFound{branch}`，去路 `fs-cli branch <b> --from main`。
- **挂载语境下"换分支"**：owner 期望"同一挂载点 checkout 另一分支"。一个挂载在其生命周期内绑定一个 workspace（W2），换分支 = **换挂**：
  - S1：`checkout <b>` 在挂载语境里**只做上面的创建/恢复**，并提示两步：`fs-cli umount U: && fs-cli mount U: --workspace <b>`；
  - S2（owner 裁决 D3）：增 `mount U: --switch <b>`，经管道请求挂载进程**换挂**：要求挂载上**无打开句柄**
    （NTFS 没有"文件底下换卷"的对应行为，有句柄时换挂会让句柄指向消失的对象——总原则下不可接受），flush → 关旧 workspace → 开新 workspace → 重新挂载同一挂载点。
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
| 路径不存在 | `NotFound{path}` | 原样（带 workspace 名） |
| workspace 被占用 | `WorkspaceInUse` | 查记录：有挂载 ⇒ "挂载在 U:（pid N）"+ §6 对应去路；无挂载 ⇒ "另一个进程正打开它（另一条 fs-cli？）" |
| 主库忙 | `Busy{waited_ms}` | 原文 + "已等待 N ms；稍后重试"（ADR-006 §11.1：不自动重试） |
| GC 拒绝 | `StoreInUse` | 原文 + 列出活的挂载 |
| commit 分叉 | `BranchMoved{expected, found}` | 原文 + ADR-006 §10.3 的出路 |
| mount：盘符被占 0xC0000035 | 已有（`mountcmd.rs`） | 保持 |
| mount：挂载点是真实卷 0xC0000022 | 已有（e545089） | 保持 |
| mount：其它 | 打印四个码 | 保持 |
| Git Bash 路径改写 | 核心报 `NotAbsolute` | §5.1 第 3 行 |

### 8.3 退出码

现状一律 1（`main.rs:60`）。规定分档，供脚本区分"重试有意义"与"改命令才行"：

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 其它失败 |
| 2 | 用法错误（参数、路径形态、**未指定 workspace**） |
| 3 | 不存在（store / workspace / branch / path / commit） |
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

- 挂载进程以 `FILE_SHARE_READ`（**不给** `FILE_SHARE_WRITE | FILE_SHARE_DELETE`）打开自己的记录文件并持有到退出。
- 读者：能读内容；判活 = 尝试以写权限打开——**共享冲突 ⇒ 活**，成功 ⇒ 陈旧（持有者已死，内核在进程结束时必关句柄，
  `taskkill /F` 也一样）。比 `pid` 校验强：不受 pid 复用影响，不需要比对进程启动时间。
- 陈旧记录由**下一个读者**清理（能以写权限打开即可删除），并在 stderr 说明；`umount --force` 仍保留作显式清理。
- 记录内容（S1）：`pid`、`mount_point`（规范化）、`store`（规范化）、`workspace`、`branch`（挂载时的值，仅展示）、
  `started_at`；若 §4.4 需要，再加 `volume_guid`。S2 加 `pipe`（管道名）。

## 10. 挂载进程代理（S2 接口草案）

### 10.1 通道

- 命名管道 `\\.\pipe\layerfs-<record>-<pid>`，名字写进记录；ACL 只允许同一用户。挂载进程服务，单请求单响应，按行 JSON。
- 取代 stop 文件：管道在即进程在，umount 走同一通道。`mountcmd.rs:1-8` 的"不用 IPC 是因为它带来要测试的协议"在只做 stop 时成立；
  一旦代理 commit 就躲不掉，那时一并迁移。

```text
→ {"op":"status"}                          ← {"ok":true,"workspace":"dev","branch":"dev","base":"…","dirty":true,"handles":3}
→ {"op":"commit","message":"…","force":false} ← {"ok":true,"commit":"…"} | {"ok":false,"error":"BranchMoved","expected":"…","found":"…"}
→ {"op":"snapshot"}                        ← {"ok":true,"root":"…"}
→ {"op":"flush"}                           ← {"ok":true}
→ {"op":"unmount","wait_ms":10000}         ← {"ok":true} | {"ok":false,"error":"Timeout","handles":2}
→ {"op":"switch","workspace":"feat"}       ← {"ok":false,"error":"HandlesOpen","handles":1}   （D3 批准时）
```

错误以核心的类型名回传，CLI 按 §8.2 渲染——**CLI 不在客户端重新判断语义**。

### 10.2 前置：API 形状

`commit / snapshot` 取 `&mut Workspace`（§3.4），挂载进程只有 `&'static Workspace` 被回调线程共享。需要其一：

- **(a) 推荐**：`Workspace` 内部互斥——`commit/snapshot(&self)`，内部取一把与回调路径相同的写锁。ADR-006 §9.1 第 0 步本来就规定
  "取 workspace 锁（本 workspace 的写操作在此期间阻塞）；flush"——(a) 只是把这把锁从 `&mut` 的借用规则搬到运行时，语义不变；
- (b) 挂载进程在代理请求时暂停分发（WinFsp 无此原语，需自己在回调入口挡）。

### 10.3 语义前置（**未验证，S2 开工前必须先定判据**）

- **commit 之后打开句柄的身份**：ADR-006 §9 的发布把 overlay 换到新 base。挂载中的句柄按 FileId 工作（ADR-009 G2）——
  发布前后同一 FileId 必须指向同一对象、读到同样内容、可继续写。FileId 是 scope serial（ADR-006 §6），**推断**发布后不变，
  但没有判据。S2 判据：`open(h) → write → 代理 commit → read(h) == 写入内容 && FileIndex 不变 → write(h) → status.dirty`。
- **restore 不代理**：restore 把视图换成另一棵树，打开句柄底下的对象会消失；NTFS 无对应行为。S2 仍要求先 umount。
- **flush 与 commit 的顺序**：commit 第 0 步已含 flush（ADR-006 §9.1）；代理不另加。

## 11. 替代方案（已否决）

| 方案 | 否决理由 |
| --- | --- |
| 保留 `cli/current` 作未挂载时的默认（architect 初稿推荐） | **owner 否决（D1）**：隐藏状态决定命令含义；§4.1 |
| mount 更新 current（owner 第 1 点） | 随 current 一并消失；初稿的否决理由（W2 下有害、历史依赖、崩溃不一致）记于 §4.1 |
| `mount` 缺省挂 `main` | 隐式默认，与 C1 同类（C3） |
| 挂载记录写进 store 目录 | §9.1：查询方向不对 + 第二份真相 |
| 只用 pid 判活 | pid 复用；需比对启动时间且仍有竞态。句柄判活（§9.2）严格更强且更简单 |
| 挂载期间 CLI 文件命令"绕过"挂载直接写 overlay | 违反 W2；即便锁允许，也是同一 workspace 的第二条写路径，挂载侧缓存（FileNode、size 缓存）不会知道 |
| 挂载期间 CLI 文件命令经管道代理 | 能做，但与"在 U: 上直接 copy"重复，且要复制一套路径/句柄语义；总原则下挂载期文件操作就该走文件系统。只代理工作区级操作 |
| 卷内放一个虚拟文件（如 `U:\.layerfs`）供反查 | 污染命名空间，`dir /a`、备份工具都会看见——违反"与普通文件系统一致" |

## 12. 影响面

| 位置 | 变化 | 期 |
| --- | --- | --- |
| `fs-cli/src/main.rs:460` `current_branch` | 改走 `workspace_list()` | S0 |
| `fs-cli/src/main.rs:465-485` `current_path/read_current/write_current` | **删除**；`init`（`:86`）与 `checkout`（`:359`）不再写；`with_workspace`（`:448`）、`snapshot create`（`:273`）改用解析器（§4.2） | S0 |
| `fs-cli/src/main.rs:344-360` `checkout` | 存在性改走 `workspace_list()`；无副作用；输出下一步 | S0 |
| `fs-cli/src/main.rs` `log/reset/revert` | 分支来源按 §4.3（加 `<branch>` / `--branch`） | S0 |
| 新增全局 `--workspace` 与解析器（§4.2 的显式部分；发现部分 S1 接入同一解析器） | 去掉 current 后它是唯一来源，必须与删除同批 | S0 |
| 消息常量 `NO_WORKSPACE` | S0：`no workspace specified; use --workspace <name>`；S1 接入路径发现时**同一常量**追加 `, or cd into a mount point` | S0/S1 |
| `fs-cli/src/mountcmd.rs:178` | 删除缺省 `"main"`，`--workspace` 必填 | S0 |
| `fs-cli/tests/cli.rs`、`cli_defaults.rs` | 依赖 current 的用例改为显式 `--workspace`；`cli_defaults.rs::init_leaves_a_workspace_ready_to_use`（`init` 后不 checkout 直接 `put/ls`）**判据翻转**：无 `--workspace` 必须报 C2 消息；`cli.rs::failures_are_reported` 里删除 `cli/current` 的那段（`:195`）随之改写 | S0 |
| `WorkspaceInUse` / `StoreInUse` 渲染 | 查记录，带挂载点与 pid | S0 |
| `fs-cli/src/mountcmd.rs:62-72` 记录目录 | `%LOCALAPPDATA%\layerfs\mounts\` + 句柄判活 | S1 |
| 新增 `cli_path()` | §5.1 三条 | S1 |
| 路径发现（§4.4） | Win32 两个调用 + 记录扫描 | S1 |
| §6 分流 | 每条命令一个类别常量，表驱动 | S1 |
| Ctrl-C 处理（§7.1） | `SetConsoleCtrlHandler` | S1 |
| 退出码（§8.3） | | S1 |
| `fs-api` `Workspace::commit/snapshot(&self)` | §10.2 (a) | S2 |
| 管道服务与客户端（§10.1） | | S2 |
| ADR-006 §13 CLI 段 | 标"已由 ADR-010 取代" | 本 ADR 合入时（已在本分支） |
| A5（可选）：挂载点内宿主路径自动翻译为 workspace 路径 | | 不排期 |

## 13. 分期与判据

判据形态沿用 criteria-and-acceptance：三态（通过 / 失败带红值 / 空转）、每条带能把它弄红的单点变异。

### S0（可马上派 worker：去掉 current + 修三处不一致 + 去路）

**判据影响：改变**——`cli_defaults.rs::init_leaves_a_workspace_ready_to_use` 由"`init` 后直接 `put/ls` 成功"变为"无 `--workspace` 必须失败并给出 C2 消息"；其余依赖 current 的用例改为显式（不改判据，只改调用形状）。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A0 | 夹具 store 里**故意放一个** `cli/current`（内容 `main`），cwd 不在挂载内、不给 `--workspace`：`put/get/ls/stat/commit/status` 全部失败，stderr **逐字**含常量 `NO_WORKSPACE`（§8.2；断言引用常量，不在测试里另写字面量），退出码 2 | 恢复读取 current（任一条命令）；或改写消息 |
| A0b | `init` 与 `checkout` 之后 store 里**不存在** `cli/` 目录 | 恢复 `write_current` |
| A1 | workspace `dev` 被另一进程持有时，`log dev`、`reset <c> --branch dev --expect <c>`、`revert --branch dev`、`--workspace dev log` 成功（取分支不打开 overlay） | 分支来源改回 `workspace_open` |
| A2 | `mount <mp>` 不带 `--workspace` ⇒ 失败，消息含 `--workspace`，**且未挂载**（盘符仍空、无记录） | 缺省改回 `"main"` |
| A3 | 撞 `WorkspaceInUse` 且有该 workspace 的活挂载时，错误含挂载点与 pid | 渲染不查记录 |
| A3b | `checkout <b>` 在 `<b>` 正被另一进程持有时成功并报告其状态；`checkout nope`（分支不存在）⇒ `NotFound` 且去路含 `branch nope --from` | 存在性改回 `workspace_open` |

### S1

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A4 | **跨进程 W2**：进程 A 持有 `dev`（真读一次证明活着），进程 B `workspace_open(dev)` 得到 typed `WorkspaceInUse`；**删除挂载记录后**仍拒绝（证明锁层独立于记录层）；A 退出后 B 成功（阴性对照） | 去掉 `locking_mode=EXCLUSIVE`；或把 open 改成不触发首次加锁 |
| A5 | 同一 `(store, workspace)` 第二次 mount 被拒，错误指出第一个挂载点（关闭 B-22） | 去掉记录层检查（此时 A4 仍拦，但错误文本不含挂载点 ⇒ 本条红） |
| A6 | §4.2 组合表 7 行各一用例；"`--store S` 与发现的 store 不同"必须丢弃发现并报 C2 消息 | 拆开混用（取 S 加 M.workspace） |
| A7 | 陈旧记录（持有进程已死）⇒ 不进入挂载语境（需要 workspace 的命令报 C2 消息），stderr 提示陈旧记录；记录被下一个读者清理 | 判活改为"文件存在即活" |
| A8 | §6 表每类至少一条命令的用例（照常 / 拒绝带去路 / 拒绝） | 任一类别常量改错 |
| A9 | `cli_path()`：`//a/b`≡`/a/b`、`\a\b`≡`/a/b`、`/a//b` 拒绝、MSYS 改写形态拒绝且提示含 `MSYS_NO_PATHCONV` | 任一条去掉 |
| A10 | Ctrl-C 后：卷卸载、记录删除、Ctrl-C 前的写入在重新挂载后可读 | 去掉控制台处理器（行为退化为 M7 残留） |

### S2（需 owner 裁决 D2；开工前先补 §10.3 的判据）

A11 代理 commit 的句柄身份判据（§10.3）；A12 `unmount` 经管道且超时返回句柄数；A13（若 D3 批准）有句柄时 `switch` 拒绝、无句柄时换挂成功且记录更新。

## 14. 需要 owner 裁决

| # | 问题 | 推荐 |
| --- | --- | --- |
| D1 | `cli/current` 的去留 | **已裁（owner 2026-10-26）：去掉**；cwd 不在挂载内时必须显式 |
| D2 | 挂载期间的 `commit/status/snapshot`：S2 代理是否进入当前阶段，还是以 S1"拒绝并指路"收口 | 先 S1 收口；S2 在 §10.3 判据写出后再排 |
| D3 | 挂载语境下 `checkout <b>` = 换挂（无句柄时）？还是永远要求 `umount`+`mount` | S1 两步；S2 随 D2 一起定 |
| D4 | 后台挂载（`mount --detach` / 服务化） | 不在本 ADR；若要，另立 ADR（涉及会话、登录、服务账户） |
| D5 | 退出码分档（§8.3）是否作为对外承诺 | 接受（项目未上线，现在定成本最低） |
| D6 | `init` 是否仍顺带创建同名 workspace `main`（不写任何 CLI 状态） | **保留**：它是真实对象而非隐藏状态，使 `init` 之后 `--workspace main put …` 立即可用。manager 转述为"init 只建 store + main branch"——若意在连 workspace 也不建，则 `init` 后需先 `checkout main`；请确认 |

## 15. 未验证清单

| # | 项 | 验证于 |
| --- | --- | --- |
| V1 | 跨进程 `WorkspaceInUse`（B-14b）与 M4"二次挂载无闸"（B-22）的矛盾 | S1 A4/A5 |
| V2 | WinFsp 目录挂载点下 `GetVolumePathNameW` 的返回值；`subst`/网络盘 | S1 L2 |
| V3 | `%TEMP%` 清扫是否会删除活记录（决定 §9.1 迁移的紧迫度，不决定方向） | 不必验证：方向已定 |
| V4 | `CTRL_CLOSE_EVENT` 的约 5 s 内 flush 是否总能完成（大 overlay） | S1 A10 加大 overlay 变体 |
| V5 | 代理 commit 前后 FileId / 句柄有效性 | S2 A11 |

## 修订记录

- 2026-10-26 初稿 `6d733f8`：保留 `cli/current` 作未挂载默认（architect 推荐）。
- 2026-10-26 按 owner 裁决 D1 修订：**去掉 `cli/current`**（C1）；workspace 来源 = `--workspace` > 路径发现 > 报错（C2，消息逐字固定）；
  `mount` 必须显式 `--workspace`（C3，删除缺省 `"main"`）；`checkout` 改为无副作用的创建/恢复（C10、§7.3）；新增 §4.3 分支来源；
  S0 判据增 A0/A0b/A3b、改写 A1/A2，判据影响 = 改变（`cli_defaults.rs::init_leaves_a_workspace_ready_to_use`）；新增 D6。
