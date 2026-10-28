# ADR-010 S1 卡：挂载记录、路径发现、Ctrl-C、管道骨架、挂载语境下的命令

- 状态：**待派**（architect 起草，2026-10-28；manager review 后派 worker）
- 依据：ADR-010（Accepted，本卡同批修订 §7.1/§7.2/§9.2/§10.2/§10.4 五处，见 §9）；S0 收口 `f2e301a`（`fs-cli` 22/22）
- 基线：`origin/main` **`bdde04c`**（行号与该 SHA 绑定，流程 243）
- 判据形态：三态（通过 / 失败带红值 / 空转）；每条带一个单点变异，提交说明逐条写"变异 → 哪个用例红"（S0 的写法）
- 分支：`adr010-s1/*`，基于 `origin/main`；提交说明首行写判据影响行

---

## 0. 范围

| 本期做 | 留给后续 |
| --- | --- |
| 挂载记录迁到 `%LOCALAPPDATA%\layerfs\mounts\`，句柄判活，`DELETE_ON_CLOSE` | — |
| 路径发现（cwd → 卷根 → 记录） | 目录挂载点若 V2 为红：记录结论，修法留 S2 |
| Ctrl-C / Ctrl-Break / 关闭控制台 = 正常卸载 | — |
| 管道骨架 + 线程结构；op **`hello` / `status` / `unmount`** | `commit` / `snapshot`（S2，需发布锁）；`restore` / `switch`（S3） |
| 删除 stop 文件与"记录消失即停止" | — |
| 挂载语境下的命令分流（ADR-010 §6 表中 S1 能做的行） | 代理 commit/snapshot/restore/switch |
| S0 剩下的 A3 / A3b，加上 S0 漏掉的 A1b | — |
| 退出码映射补全（ADR-010 §8.3） | — |
| 适配器打开句柄计数（`status.handles`、`unmount` 用） | — |

**不在本期**：`fs-workspace` / `fs-api` 的任何签名改动（发布锁是 S2）。若实现中发现需要改它们，**停卡回报**。

## 1. S0 的剩余项（先做，独立提交）

`f2e301a` 的提交说明写明 A3/A3b 未做。另外核对代码时发现一处漏项：

- **A1b（S0 漏项）**：`current_branch()`（`crates/fs-cli/src/main.rs:443-446`）在没有 `--branch` 时仍然调 `workspace_open`。ADR-010 §4.3 规定从 workspace 取分支要走 `workspace_list()`、不打开 overlay。S0 的 A1 只测了带 `--branch` 的路径，所以没发现。
- `read_current()`（`main.rs:452-454`）是 S0 留下的过渡壳，本期删掉，调用处直接用 `require_workspace()`。
- `checkout`（`main.rs:301-321`）仍然调 `workspace_open`，这就是 A3b 要改的。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A1b | `dev` 被另一进程持有（真挂载，或测试进程自己 `workspace_open` 后不关）时，`--workspace dev log` 不带 `--branch` 也成功，输出 `dev` 分支的日志 | `current_branch` 改回 `workspace_open` |
| A3 | `dev` 被挂载时，`--workspace dev commit -m x`、`restore <c>`、`snapshot create`、`workspace discard dev` 都失败：退出码 4，stderr 含常量 `fs_cli::mounted_refusal(cmd, ws, mount_point, pid)` 的输出（含挂载点与 pid）。`dev` 被一个**无记录**的进程持有时，消息是 `fs_cli::IN_USE_ELSEWHERE`（"另一个进程正打开它"），退出码同样是 4 | 渲染时不查记录（两种情形的消息相同 ⇒ 第一种红） |
| A3b | `dev` 被挂载时 `checkout dev` 成功、退出码 0、输出含 `mounted at <mp>`；`checkout nope` ⇒ 退出码 3，去路含 `branch nope --from`；cwd 在 `dev` 的挂载内执行 `checkout other` ⇒ 创建或恢复 `other`、**不动挂载**（记录与盘符不变），并打印两步提示 `umount <mp>` + `mount <mp> --workspace other` | 存在性检查改回 `workspace_open`（第一条红：`WorkspaceInUse`） |

## 2. 挂载记录（ADR-010 §9）

### 2.1 形状

- 目录：`%LOCALAPPDATA%\layerfs\mounts\`；环境变量 `LAYERFS_MOUNTS_DIR` 可以覆盖（**测试隔离用**，在 `--help` 里写明）。路径只由一个函数 `mounts_dir()` 计算。
- 文件：`<record_name(mount_point)>.mount`（`record_name` 沿用 `crates/fs-cli/src/mountcmd.rs:38`）。内容仍是 `key=value` 行，字段：
  `version=1`、`pid`、`mount_point`、`store`、`workspace`、`branch`、`pipe`、`started_at`。其中 `store` 与 `mount_point` **写入时就规范化**（`canonicalize` 后去掉 `\\?\`），比较时不再规范化第二次。
- 挂载进程打开记录的方式：`create_new(true)`、`write(true)`、`share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)`、
  `custom_flags(FILE_FLAG_DELETE_ON_CLOSE)`；写完内容后**持有句柄直到退出**。
  - **`DELETE_ON_CLOSE` 的意义**：进程不论怎么死（包括 `taskkill /F`），内核关闭句柄时都会删除文件。陈旧记录只可能来自断电或蓝屏。
  - **不给 `FILE_SHARE_WRITE`**：这是判活的依据（§2.2）。
  - **给 `FILE_SHARE_DELETE`**：读者打开一个带 delete-on-close 持有者的文件时，自己也必须声明 delete 共享；持有者这边给出这个共享，读者才能读取。
- 读者打开记录：`read(true)`、`share_mode(READ | WRITE | DELETE)`。

### 2.2 判活

- 判活的唯一实现是 `record_is_live(path) -> Liveness { Live, Stale, Gone }`：以 `write(true)` 加全共享去打开该文件。
  - 得到 `ERROR_SHARING_VIOLATION`（32）⇒ `Live`；
  - 打开成功 ⇒ `Stale`：持有者已不在，由读者删除并在 stderr 说明一次；
  - `NotFound` ⇒ `Gone`。
- 所有读记录的地方（发现、`list`、`umount`、A3 渲染、二次挂载检查）都只通过这个函数。

### 2.3 旧位置

- S1 二进制**不读** `%TEMP%\layerfs-mounts\`，也**不删**它：里面可能有旧二进制正在运行的挂载，删记录会让旧挂载自行卸载（`mountcmd.rs:238`）。
- `list` 发现该目录里有 `*.mount` 时，打印一行常量 `fs_cli::OLD_RECORDS_NOTICE`（含目录路径，说明"旧版本的记录已忽略：结束那些挂载进程后删除该目录"）。项目未上线，不做兼容。

### 2.4 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A7a | 真挂载期间，另一进程能读记录、字段齐全且已规范化；以写权限打开 ⇒ 共享冲突；`taskkill /F` 挂载进程后 **2 s 内**记录文件消失 | 去掉 `DELETE_ON_CLOSE` ⇒ 文件残留；或加上 `FILE_SHARE_WRITE` ⇒ 判活把活记录判成 `Stale` |
| A7 | 手工写一个无持有者的记录（模拟断电残留）：发现逻辑判为不在挂载语境，stderr 提示一次，文件被删除；`umount <mp> --force` 对 `Live` 记录**拒绝**（退出码 4、记录仍在），对 `Stale` 记录删除并如实说明 | 判活改为"文件存在即活" |
| A7b | `LAYERFS_MOUNTS_DIR` 未设置时 `mounts_dir()` 落在 `%LOCALAPPDATA%\layerfs\mounts`；`%TEMP%\layerfs-mounts\x.mount` 存在时 `list` 打印 `OLD_RECORDS_NOTICE`，且该文件**未被删除** | `mounts_dir()` 改回 `temp_dir()`；或 `list` 清理旧目录 |

## 3. 互斥（ADR-010 C8）

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A4 | **跨进程 W2**：子进程 A 打开 `dev` 并真读一次（证明活着）后挂起，B `workspace_open(dev)` 得到 typed `WorkspaceInUse`；A 不写记录也照样拒绝（证明锁层独立于记录层）；A 退出后 B 成功（阴性对照）。若 B **成功了** ⇒ 失败带红值，并且是 B-14b / B-22 的定论：**停卡回报**，因为 C8 的锁层不存在 | 去掉 `PRAGMA locking_mode = EXCLUSIVE`（`fs-overlay/src/schema.rs:86`） |
| A5 | 同一 `(store, workspace)` 第二次 `mount` ⇒ 退出码 4，消息含第一个挂载点，第二个盘符**未挂上**。store 用另一种写法（相对与绝对、大小写不同）时同样拒绝 | 去掉记录层检查（A4 仍会拦住，但错误文本不含挂载点 ⇒ 本条红）；或比较未规范化的原串（第二条红） |

挂载顺序（ADR-010 §7.1，不变）：版本闸 → **记录层检查** → `workspace_open`（锁层）→ 建记录 → 挂载。

## 4. 路径发现（ADR-010 §4.2 / §4.4）

### 4.1 拆分

为了让组合表能在不挂载的情况下测，发现逻辑拆成两部分：

```rust
// 纯函数：组合规则，ADR-010 §4.2 那张 7 行表
fn resolve(explicit_store: Option<&Path>, explicit_ws: Option<&str>, discovered: Option<&Record>) -> Target;
// 有副作用：只做两步查询
fn discover(cwd: &Path, volume_root: impl Fn(&Path) -> Option<(PathBuf, String /*label*/)>, records: impl Fn() -> Vec<(Record, Liveness)>) -> Option<Record>;
```

- `volume_root` 的生产实现：`GetVolumePathNameW(cwd)` 加 `GetVolumeInformationW(root)` 取卷标；卷标不等于 `"layerfs"` ⇒ `None`。
- `records` 的生产实现：扫描 `mounts_dir()`，每条都经过 `record_is_live`。
- `NO_WORKSPACE` 在本期**追加** `, or cd into a mount point`（只改 `fs_cli` 里那一个常量，A0 照旧断言常量）。

### 4.2 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A6 | `resolve` 对 §4.2 表的 7 行各一个用例（纯函数、假记录）。"`--store S` 与发现的 store 不同"必须丢弃发现结果并报 `NO_WORKSPACE` | 拆开混用（取 S 再配上发现的 workspace） |
| A6b | 真挂载 `dev` 于盘符：cwd = `<mp>\` 和 `<mp>\sub` 时，不带 `--store`/`--workspace` 执行 `status`，输出的都是 `dev`（经管道，§5）。cwd 不在挂载内时同一命令退出码 2，消息为 `NO_WORKSPACE` | `discover` 恒返回 `None` |
| A6c | 一条活记录的 `mount_point` 指向一个**不是**我们的卷（例如测试把记录写成 `C:\`）：cwd 在该卷下时不进入挂载语境 | 去掉卷标核对 |
| A6d | **V2（只记录，不阻塞本卡）**：目录挂载点 `C:\…\mnt` 下，`GetVolumePathNameW` 返回什么、A6b 是否成立。按三态记录。若为红，ADR-010 §4.4 加一句"目录挂载点不支持发现，需显式 `--store/--workspace`"，修法（`VOLUME_NAME_GUID`）留给 S2 | — |

## 5. 管道骨架与线程结构（ADR-010 §10.1 / §10.2 / §10.5）

### 5.1 线程与 channel

```text
enum Control { Stop { reason: StopReason, reply: Option<Sender<StopResult>> } }
enum StopReason { Unmount /*管道*/, Console(u32) /*CTRL_C/BREAK/CLOSE*/ }

主线程                                        管道线程                                控制台处理器（系统线程）
 版本闸 → 记录层检查 → workspace_open          loop {                                   ctrl ∈ {C, BREAK}:
 建记录（持句柄）                                if SHUTTING_DOWN { break }                CONTROL.send(Stop{Console(ctrl), None}); TRUE
 SHUTTING_DOWN = false                           ConnectNamedPipe（阻塞）                ctrl == CLOSE / LOGOFF / SHUTDOWN:
 起管道线程                                      读一帧 → 分派：                            CONTROL.send(...)；
 安装控制台处理器                                  hello / status ⇒ 本线程直接回答          等 DONE，最多 CLOSE_GRACE = 4500 ms；
 mount_serving(…, |mounted| {                      unmount ⇒ §5.3                          TRUE
     let msg = rx.recv();  // 只等这一件事        写响应 → FlushFileBuffers → Disconnect
     stash(msg); Ok(()) })                     }
 // 返回即已卸载（SetMountPoint(NULL) → stop → delete）
 SHUTTING_DOWN = true
 workspace.flush()
 关闭记录句柄（DELETE_ON_CLOSE 删除文件）
 reply.send(Ok) / DONE.notify
 唤醒管道线程：以客户端身份连一次自己的管道（≤ 2 s 重试），然后 join
 退出码 0
```

- **停止顺序改为"卸载 → flush → 删记录"**（ADR-010 §7.1 原写"flush → 卸载"，本卡同批修订）。理由：先 flush 再卸载，两者之间到达的写不在 flush 覆盖范围内。卸载之后不会再有回调，此时 flush 覆盖全部写入。
- `CONTROL` 是 `OnceLock<Sender<Control>>`，`DONE` 是 `(Mutex<bool>, Condvar)`，都只在 `mountcmd` 里定义，各一处。
- `CTRL_CLOSE_EVENT` 必须**等**：处理器一返回，系统就结束进程。C/BREAK 不需要等。
- **`mount_serving`**：在 `fs-winfsp` 新增，`mount_read_only` 改为它的薄包装，现有测试不受影响。闭包拿到 `&Mounted`，它提供：
  - `path()`；
  - `open_handles() -> usize`：该挂载的适配器 HandleTable 里存活的上下文数（`registry.rs` 已是"一挂载一 Adapter 一 HandleTable"）。

  `fs-winfsp` 的这处改动是新增，不是修改；它**不属于** §0 的"停卡"条件。

### 5.2 管道

- 名字：`\\.\pipe\layerfs-<record_name>-<pid>`，写进记录的 `pipe` 字段。
- 创建参数：`PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE`，`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS`，`nMaxInstances = 1`。
- 安全描述符：SDDL `D:P(A;;GA;;;<当前用户 SID>)`。SID 由 `GetTokenInformation(TokenUser)` 取得，再 `ConvertSidToStringSidW`。**不得**传 `NULL`：默认 DACL 允许 Everyone 读。
- 帧：一行 UTF-8 JSON 加 `\n`，上限 `PIPE_FRAME_MAX = 64 KiB`。超限 ⇒ 回 `FrameTooLarge`，丢弃该连接，**继续服务下一个**。
- 每个请求带 `"v":1`。不符 ⇒ `ProtocolVersion{supported:[1]}`；JSON 不合法、缺字段或未知 op ⇒ `BadRequest{detail}`；停止序列开始之后到达的请求 ⇒ `ShuttingDown`。
- 客户端：
  - `WaitNamedPipeW(PIPE_CONNECT_WAIT = 2000 ms)`。超时则判活：`Live` ⇒ 退出码 4，消息"挂载进程正忙"；`Stale` ⇒ 按 §2.2 处理。
  - 响应等待 `PIPE_REPLY_WAIT_QUICK = 30 s`，用于 `hello` 和 `status`；`unmount` 的等待见 §5.3。
- 常量都在 `fs_cli`（lib）里各定义一处。

**依赖（需 manager 拍板）**：JSON 编解码用 **`serde` + `serde_json`**（MIT/Apache-2.0，只加在 `fs-cli`）。也可以手写一个扁平 JSON 子集，但手写解析器本身就需要一套判据，不划算。Win32 调用用 `windows-sys`（工作区已有，`cas-store/Cargo.toml:40`，0.61.2），`fs-cli` 需要的 feature 是 `Win32_Foundation`、`Win32_Security`、`Win32_Security_Authorization`、`Win32_Storage_FileSystem`、`Win32_System_Pipes`、`Win32_System_IO`、`Win32_System_Console`、`Win32_System_Threading`。

### 5.3 op（本期 3 个）

| op | 请求 | 成功响应 | 行为 |
| --- | --- | --- | --- |
| `hello` | `{}` | `{store, workspace, branch, pid, mount_point, protocol:1}` | 全部取自记录 |
| `status` | `{}` | `{workspace, branch, base_commit, dirty, handles}` | 管道线程直接调 `Workspace::status()`（只读；`&'static Workspace` 本来就被分发线程共享），`handles` 取 `mounted.open_handles()` |
| `unmount` | `{wait_ms}` | `{}`，在卸载、flush、记录删除**之后**才发 | `open_handles() > 0` 时最多等 `wait_ms`（CLI 传 `UNMOUNT_WAIT` = 10 s，`mountcmd.rs:28`，仍是唯一来源），句柄归零即发 `Stop`；到时仍不为零 ⇒ 回 `HandlesOpen{handles}`，**不发 Stop、状态不变** |

**句柄与停止（ADR-010 §7.2 同批修订）**：
- 远程 `umount` 在有打开句柄时**拒绝**（与 M5 一致：发起方未必知道有文件开着）；
- 控制台 Ctrl-C/BREAK/CLOSE **不看句柄、照常卸载**：用户在挂载窗口前明确要求停止，打印"N 个打开的句柄将失效"。

这里有一个**未验证**项：有打开句柄时卸载会不会阻塞（V8）。旧流程在 M5 观察到"超时不卸载"，可能就是卸载本身被阻塞了。由 A10b 测定。

### 5.4 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A11 | `hello` 与记录逐字段一致；`v:2` ⇒ `ProtocolVersion`；65 KiB 的帧 ⇒ `FrameTooLarge`，**下一次** `hello` 照常成功；坏 JSON 与未知 op ⇒ `BadRequest`，之后照常成功 | 出错后不再 accept（第二步红） |
| A11b | 管道的 DACL 恰好一条允许 ACE，SID 为当前用户（`GetSecurityInfo` 读出比较） | 传 `NULL` 安全属性 |
| A11c | 纯函数分派：上下文 `shutting_down = true` 时任何 op 都回 `ShuttingDown` | 忽略该标志 |
| A12 | 经挂载点打开一个文件不关，`umount <mp>` ⇒ 约 10 s 后退出码 4，消息含 `1 open handle`，挂载与记录都还在；关闭句柄后再 `umount` ⇒ 成功，**命令返回时**记录已不存在（客户端在 `ok` 后立刻检查） | 先回响应再删记录（第二步红）；或超时后照样发 Stop（第一步红） |
| A14 | `status`（经管道）：挂载后先 `dirty=no`；经挂载点写一个文件后 `dirty=yes`；再多打开 N 个文件，`handles` 增加 N，关闭后回落。断言增量，不断言绝对值，因为测试框架自己也可能持有目录句柄 | `handles` 恒为 0 |
| A15s | 旧的停止信号无效：在 `mounts_dir()` 写 `<name>.stop`，或尝试删除记录，挂载**不停止**（删除会因共享冲突失败，这本身也要断言） | 保留旧的轮询 |

## 6. 生命周期（ADR-010 §7）

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A10 | 以 `CREATE_NEW_PROCESS_GROUP` 启动挂载，向其进程组发 `CTRL_BREAK_EVENT`（`GenerateConsoleCtrlEvent`；`CTRL_C_EVENT` 发不到新进程组，二者走同一处理器）：10 s 内**退出码 0**、stdout 含 `unmounted`、盘符消失、记录消失；重挂后能读到 Ctrl-Break 之前写入的内容 | 不装处理器：进程被系统结束，退出码为 `0xC000013A` 而不是 0，且盘符残留（M7 形态）。**注意**：有 `DELETE_ON_CLOSE` 时记录不论哪种情况都会消失，所以"记录消失"**不能**用来区分，必须断言退出码与盘符 |
| A10b | **V8**：挂载中经挂载点打开一个文件不关，发 `CTRL_BREAK_EVENT`：记录进程退出所需时间。≤ 10 s ⇒ 通过；超过 ⇒ 失败并带红值（实测秒数），**停卡回报**，因为卸载会被打开句柄阻塞，需在 `fs-winfsp` 的拆卸顺序上另做处理 | — |
| A16s | 不带参数的 `umount`，cwd 在挂载内 ⇒ 卸载该挂载；cwd 不在挂载内 ⇒ 退出码 2 并给出用法 | 无参数时不走发现 |

## 7. 挂载语境下其余命令（ADR-010 §6 表，S1 能做的行）

| 命令（目标 workspace 正被挂载） | S1 行为 | 判据 |
| --- | --- | --- |
| `log`、`branch`、`stats`、`verify`、`list`、`version`、`snapshot list/drop`、`reset`、`revert` | 照常（CLI 进程内） | A8：每条命令一个用例 |
| `status` | 经管道 | A6b / A14 |
| `commit`、`snapshot create`、`restore`、`workspace discard` | 拒绝，`mounted_refusal`，退出码 4 | A3 |
| `checkout` | 成功；在挂载内时打印两步提示 | A3b |
| `gc` | `StoreInUse` 的渲染列出该 store 的活挂载（挂载点、pid），退出码 4 | A8 |
| `mount` 同一 workspace | 拒绝 | A5 |
| `umount`（无参数） | 卸载 cwd 所在的挂载 | A16s |

`list` 的新输出：每条记录一行，含 `mount_point  pid  store  workspace  branch  live|stale(removed)`。

## 8. 退出码（ADR-010 §8.3）

S0 引入了 `Failure{code}` 和 `usage/not_found/in_use`。本期把映射收进**一个函数**：`exit_code(&dyn Error) -> u8`，按 ADR-010 §8.3 的表逐个错误种类映射；`main` 只调这一个函数。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| A17s | 表驱动单测，覆盖 §8.3 表中每个错误种类：`NotFound{store/workspace/branch/commit}`⇒3、`WorkspaceInUse`/`Busy`/`StoreInUse`/挂载拒绝⇒4、`BranchMoved`/`NothingToCommit`/`WorkspaceDirty`/`AlreadyExists`⇒5、用法错误⇒2、其余⇒1 | 任改一行映射 |

## 9. ADR-010 同批修订（本卡所在提交一起改，判据影响：无（澄清））

1. §7.1 第 7 步：停止顺序 "flush → 卸载 → 删记录" 改为 **"卸载 → flush → 删记录"**，并写明理由（§5.1）；
2. §7.2：远程 `umount` 有句柄时回 `HandlesOpen{handles}` 且**不改状态**；控制台停止不看句柄。原写法 `Timeout{handles}` 统一改名为 `HandlesOpen`；
3. §9.2：记录以 `DELETE_ON_CLOSE` 打开，共享模式为 `READ | DELETE`（原写只有 `READ`：读者无法打开带 delete-on-close 持有者的文件）；
4. §10.2：JSON 库的选择（待 manager 拍板后写入）；
5. §10.4：`unmount` 的错误从 `Timeout{handles}` 改为 `HandlesOpen{handles}`，与 restore/switch 同名。

## 10. 未验证项（本卡测定）

| # | 项 | 判据 |
| --- | --- | --- |
| V1 | 跨进程 `WorkspaceInUse`（B-14b / B-22） | A4（红 ⇒ 停卡） |
| V2 | 目录挂载点下的 `GetVolumePathNameW` | A6d（只记录） |
| V8 | 有打开句柄时卸载是否阻塞 | A10b（红 ⇒ 停卡） |
| V9 | `DELETE_ON_CLOSE` 加上述共享模式下，读者能读、写者被拒、进程被杀后文件消失 | A7a |

## 11. 提交建议（可调整）

1. S0 剩余：A1b / A3 / A3b，删 `read_current` 过渡壳；
2. 记录：`mounts_dir`、`record_is_live`、`DELETE_ON_CLOSE`、旧目录提示（A7a / A7 / A7b）；
3. 互斥：A4 / A5；
4. `fs-winfsp::mount_serving` 与 `open_handles`；
5. 管道与线程结构，删 stop 文件（A11–A11c、A12、A14、A15s）；
6. 控制台处理器（A10 / A10b、A16s）；
7. 发现（A6–A6d），`NO_WORKSPACE` 追加；
8. 退出码集中（A17s）、`list` 新输出、A8。

每个提交单独绿。A4 或 A10b 为红时停在那一步回报，不往下做。
