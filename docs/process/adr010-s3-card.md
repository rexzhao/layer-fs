# ADR-010 S3 卡：管道 restore / switch（ADR-010 的最后一张卡）

- 状态：**待派**（architect 起草，2026-10-28；manager review 后派 worker）
- 依据：ADR-010 §6.2、§7.3、§10.4、§10.6、§13 S3。本卡同批修订 ADR-010 §4.4/§6.2/§7.3/§10.3/§10.4 五处，并补入 S2 卡（见 §9）
- 基线：`origin/main` **`e1632e1`**（S2 收口）。行号与该 SHA 绑定（流程 243）
- 判据形态：三态（通过 / 失败带红值 / 空转）；每条带一个单点变异；提交说明逐条写"变异 → 哪个用例红"
- **开工前需要裁决**：D8（§1.3）。R3、W1 的期望值取决于它

---

## 0. 范围

| 本期做 | 不做 |
| --- | --- |
| S2 的 G4 空转修复（§2，**先做**） | — |
| 适配器的"准入"机制：区分文件句柄与目录句柄；暂停与关闭两种状态（§3） | 变更通知 `FspFileSystemNotify`（U9） |
| 管道 `restore`（§4） | — |
| 管道 `switch`，以及挂载语境下的 `checkout <b>` 换挂（§5） | 后台挂载、热替换后端（§5.1 说明为什么不做） |
| ADR-010 §4.4 的过期段落改正（`b806270` 已让目录挂载点可被发现） | — |

## 1. 关键事实与必须先裁的一件事

### 1.1 "无句柄"在实践中几乎永远不成立

一个进程的**当前目录**本身就是一个打开的目录句柄：Windows 打开 cwd 并一直持有，所以作为某进程 cwd 的目录删不掉。由此：

- 用户在 `U:\sub` 里的 shell（cmd、PowerShell、bash）**一直**持有 U: 上的一个目录句柄；
- `fs-cli` 自己从挂载内运行时，它的 cwd 也是 U: 上的一个句柄；
- 资源管理器打开着 U: 的窗口时，也持有目录句柄（还会 `ReadDirectoryChangesW`）。

所以 ADR-010 §6.2 / §7.3 原写的"**无打开句柄**才允许 restore / switch"，等于 D3 所说"在挂载点内 `checkout` 就换挂"**永远做不到**：发起命令的那个 shell 自己就挡住了。

### 1.2 两类句柄的后果不同

| | 文件句柄（数据） | 目录句柄（cwd、资源管理器、列目录） |
| --- | --- | --- |
| restore 或 switch 之后 | 句柄底下的对象可能消失，或者内容整体换掉；进行中的写入会失去落点 | 句柄失效，或指向新视图里的同一目录。**不丢数据**，下一次列目录或 `cd` 才会发现 |
| NTFS 里的类比 | 没有（总原则下不可接受） | 卷被卸下：shell 提示路径无效，重新 `cd` 即可恢复 |

### 1.3 D8（需 owner 经 manager 裁决）

**问题**：restore / switch 的准入条件，是"无任何句柄"，还是"无**文件**句柄"？

- **(A) 无任何句柄**（ADR 现行文本）：最保守。代价是在挂载点内 `checkout` 永远会被自己的 shell 挡住；资源管理器开着时 restore 也做不了。D3 实际落空。
- **(B) 无文件句柄，目录句柄放行（推荐）**：完成后打印"N 个目录句柄（shell 的当前目录 / 资源管理器窗口）已失效，在其中执行 `cd .` 刷新"。数据安全的部分（文件句柄）照样挡；只牺牲目录视图的连续性，这一点和 NTFS 卷被卸下时一样。

本卡的判据按 **(B)** 写。若裁 (A)：R3、W1 的期望值翻转（改为拒绝），W1 的发起方式改为"cwd 不在挂载内，用 `--workspace` + 挂载点参数"，其余判据不变。

## 2. 先修 S2 的空转：G4 的仪器从未工作过

逐行核对 `crates/fs-workspace/src/gate.rs`（`e1632e1`），发现三处问题：

1. **`DEPTH` 只增不减**（`:41-45`）：`read()` 加一，但返回的是标准库的 guard，没有任何地方在 guard 释放时减一。`:106-107` 的注释也承认减一要等"G4 到来时的 wrapper"，而这个 wrapper 没有落地。
2. **`LAYERFS_DEBUG_GATE` / `LAYERFS_GATE_STRICT` 没有任何代码读取**（全仓库 grep，只出现在 `gate.rs:48,135` 的注释里）。所以注释里"全量运行加 `LAYERFS_DEBUG_GATE=1` 没有打印任何嵌套"的测量是**空转**：报告开关根本没开。
3. 单元测试 `the_depth_follows_the_guard`（`:114-122`）只断言了"guard 内深度为 1"，**没有**断言"guard 释放后回到 0"，所以第 1 处缺陷不会让它变红。

后果：S2 的 G4 第 ② 项（"全量测试零次触发嵌套断言"）**没有被测过**。发布闸的死锁风险（有写者排队时，同一线程重复取读锁）目前没有任何判据护着。
S3 会给适配器加第二把锁（§3），锁序只会更复杂，所以本卡第一步先把它修好。

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| G4a | `read()` 返回包装后的 guard，释放时深度减一；单元测试断言：guard 内为 1、释放后为 0、连续取放 3 次后仍为 0 | 去掉减一 |
| G4b | 严格模式经测试缝打开（`set_nesting_assertion(true)`）：`fs-workspace`、`fs-api`、`fs-winfsp`、`fs-cli` **每个**集成测试二进制都打开它：每个测试函数的第一行调用公用的 `common::strict_gate()`（开关是进程级的原子量，重复调用无害）。全量运行**零 panic** | 某个经 `store()` 的公共方法外层再取一次读锁 |
| G4c | 删除注释里不存在的环境变量，或者真的去读它们。二选一，提交说明写明选了哪个；注释与代码一致 | — |

如果 G4b 打开后出现 panic，那就是 S2 遗留的真实嵌套：**停卡回报**，先修嵌套，再往下做。

## 3. 准入（适配器层）

### 3.1 为什么要有准入，而不是只在发命令前数一下句柄

"先数一下、为零再 restore"有一个窗口：数完之后、restore 拿到发布闸之前，一个 `Create/Open` 回调可能已经按**旧视图**解析出了 FileId，正准备注册句柄。
restore 之后，这个句柄就指向一个已经不存在的对象。发布闸挡不住它：它的读锁在解析完成时已经释放了，而注册句柄发生在那之后。

所以"检查"和"阻止新的打开"必须在同一把锁下完成，而且这把锁必须覆盖**解析加注册**的整个过程。

### 3.2 设计

```rust
// fs-winfsp::adapter
enum Admission { Open, Paused, Closing }
struct Gatekeeper { state: Mutex<(Admission, usize /*in_flight*/)>, changed: Condvar }

// Create / Open / GetSecurityByName 三个按路径进入的回调，整个回调期间：
let _ticket = adapter.admit()?;   // Open ⇒ in_flight += 1 后放行；Paused ⇒ 在 Condvar 上等；Closing ⇒ 立即失败
//  … 解析路径、调 ops、open_handle 注册句柄 …
// _ticket drop ⇒ in_flight -= 1，notify

// 管道线程（restore / switch）：
adapter.quiesce(kind, wait_ms) -> Result<Quiesced, HandlesOpen{files, dirs}>
//   在 state 锁内：置 Paused（restore）或 Closing（switch）→ 等 in_flight == 0（最多 wait_ms）
//   → 数文件句柄：不为零 ⇒ 恢复 Open，返回 HandlesOpen；为零 ⇒ 返回 Quiesced（drop 时恢复 Open）
```

- **Paused 时阻塞，Closing 时失败**：
  - restore 期间到达的打开**等待**：restore 不需要分发线程，等待不会死锁，打开方只是慢了一些；
  - switch 要卸载，而卸载会等分发线程退出。若打开在等，就会死锁，所以 switch 期间的打开**立即失败**，返回 `STATUS_VOLUME_DISMOUNTED`。这和 NTFS 卷正在卸下时的行为一致。
- **锁序**：准入 → 发布闸（`Create/Open` 先 `admit` 再进 `Rules`；restore 先 `quiesce` 再 `workspace.restore`）。G4b 的深度断言只管发布闸；准入是另一把锁，不嵌套。
- **句柄分类**：`Handle`（`crates/fs-winfsp/src/handles.rs:43-51`）增加字段 `is_dir: bool`，在 `open_dir` / `open` 时确定。`open_handles()` 拆成 `open_file_handles()` 和 `open_dir_handles()`。
  `status.handles` 改为 `{files, dirs}`（协议字段变化，`v` 不变：项目未上线，客户端与服务端是同一个二进制）。
- **`wait_ms`**：与 unmount 同一个常量 `UNMOUNT_WAIT`（10 s）。原因：应用 `CloseHandle` 之后，FSD 发给我们的 `Close` 可能晚到（缓存管理器还持有文件对象），等一会儿才能避免误报。

### 3.3 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| Q1 | 纯状态机单测：Open 时 `admit` 立即放行；Paused 时阻塞，恢复后放行；Closing 时立即失败；`quiesce` 等到 `in_flight == 0` 才返回；有文件句柄时返回 `HandlesOpen` 并把状态恢复为 Open | 任一状态迁移改错 |
| Q2 | `Handle.is_dir` 区分正确：真挂载打开一个文件和一个目录，`open_file_handles` 与 `open_dir_handles` 各增加 1；关闭后各自回落 | `is_dir` 恒为 false |

## 4. restore 经管道

### 4.1 op 与 CLI

| op | `args` | 成功 | 失败 |
| --- | --- | --- | --- |
| `restore` | `{commit, discard}` | `Restored{commit, dirs_invalidated}` | `HandlesOpen{files, dirs}`、`Failed{kind: WorkspaceDirty / NotFound / …}` |

- 管道线程执行：`quiesce(Paused)` → `workspace.restore(&commit, discard)`（发布闸写锁）→ 释放 `Quiesced`（恢复为 Open，唤醒等待中的打开）→ 回复。
- CLI：挂载语境下的 `restore <commit> [--discard]` 改走管道（S1/S2 是 `mounted_refusal`）。`dirs_invalidated > 0` 时打印 `fs_cli::DIRS_INVALIDATED` 提示。
- **内核缓存（V6）**：`FileInfoTimeout = 0`（`crates/fs-winfsp/src/mounted.rs:890`）关闭了元数据缓存。数据缓存在该设置下是否启用**未验证**，由 R1 第二步直接测。若读到旧内容 ⇒ R1 失败带红值，**停卡回报**：那样 restore 就依赖 U9（`FspFileSystemNotify` 失效缓存），不能在本卡解决。

### 4.2 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| R1 | 挂载 `dev`：写 `/f = "1"`，commit 得 `c1`；写 `/f = "2"`，commit 得 `c2`。① 打开 `/f` 读到 `"2"` 后关闭（让可能存在的缓存先装上旧内容）；② 无文件句柄时 `fs-cli restore c1` ⇒ 退出码 0；③ 重新打开 `/f` 读到 `"1"`（这一步同时测 V6）；④ `status` 的 base 是 `c1` 的根 | CLI 仍走 `mounted_refusal` ⇒ ② 红 |
| R2 | 打开 `/f` 不关，`restore c1` ⇒ 约 10 s 后退出码 4，消息含 `1 file`；视图不变（`/f` 仍是 `"2"`）；之后照常打开文件不受影响（状态已恢复为 Open）。关闭句柄后再 restore ⇒ 成功 | 不数文件句柄 ⇒ 第一步成功（红）；`HandlesOpen` 后不恢复 Open ⇒ 最后一步的打开挂住（红） |
| R3 | **（D8 = B）** 只有目录句柄：子进程 shell 的 cwd 在 `U:\`，再显式打开一个目录句柄。`restore c1` ⇒ 成功，输出含 `DIRS_INVALIDATED` 且数值 ≥ 2 | 把目录句柄也算作阻挡（拒绝 ⇒ 红） |
| R4 | **准入窗口**：`c2` 比 `c1` 多一个文件 `/g`。管道侧暂停点 `WCAS_CLI_PAUSE_AT=restore-after-quiesce=3000`（形态同 `pause_point`，在 `fs-cli` 里定义），restore 停在 quiesce 之后、`workspace.restore` 之前时，另一线程经挂载点打开 `/g`。断言：该打开**在 restore 返回之后**才返回，结果是**找不到**（恢复后的视图里没有 `/g`） | `Create/Open` 不取准入票 ⇒ 打开在暂停期间成功，拿到一个即将消失的对象的句柄 |
| R5 | 错误透传：overlay 有改动、不带 `--discard` ⇒ 退出码 5（`WorkspaceDirty`）；未知 commit ⇒ 退出码 3；带 `--discard` ⇒ 成功且改动被丢弃 | 管道把 `kind` 折掉 |

## 5. switch 经管道（`checkout <b>` 在挂载内）

### 5.1 采用"重挂"，不采用"热替换后端"

| | 热替换（不卸载，只把注册表里的 Adapter 换成新 workspace 的） | 重挂（卸载旧的，在同一挂载点挂新的）——**采用** |
| --- | --- | --- |
| 挂载点是否消失 | 不消失 | 消失一个短窗口（判据 W4 测量） |
| 内核侧状态 | 同一个卷设备继续存在：FSD 里关于旧视图的一切（关闭后仍留着的文件对象、可能存在的数据缓存，即 V6）都还在，**需要逐项论证它们失效了** | 卷设备被销毁，内核侧状态随之消失，不需要论证 |
| 新增机制 | 注册表原子替换，加上准入，加上缓存失效（依赖 U9） | 复用 S1 已经验证过的卸载和挂载路径 |

热替换省掉的只是一个短窗口，代价是要对一整套内核状态做正确性论证，而其中一项（U9）根本还没实现。所以采用重挂，把窗口作为度量出档。

### 5.2 顺序（失败原子性写在顺序里）

```text
CLI（cwd 在 U:\…）：
  checkout b  →  创建或恢复 workspace b（S1 已有）
              →  把自己的 cwd 移出该卷（SetCurrentDirectory 到 store 目录）：本进程不再持有 U: 上的目录句柄
              →  管道 switch{workspace: b}

挂载进程，管道线程：
  1. 打开 b（W2 锁）。失败（WorkspaceInUse / NotFound）⇒ 回复错误，**什么都没动**
  2. quiesce(Closing)。有文件句柄 ⇒ 关闭 b，回复 HandlesOpen，**什么都没动**
  3. 把 Switch{new: b 的 Workspace, reply} 发给主线程（Control 通道，与 unmount 同一套）

挂载进程，主线程（S1 的 mount_serving 循环改为 loop）：
  4. 卸载旧挂载（SetMountPoint(NULL) → stop → delete）
  5. 旧 workspace flush，然后关闭
  6. 用 b 在同一挂载点 mount_serving
     6a. 成功 ⇒ 原地改写记录（同一个句柄：先 SetEndOfFile(0)，再一次 WriteFile 写入新内容；字段 workspace / branch / volume 相关字段重新测量）
         ⇒ 回复 Switched{workspace, branch, dirs_invalidated} ⇒ 回到第 4 步之前的等待
     6b. 失败 ⇒ 用**旧 workspace**重挂（第 5 步只 flush、先不关闭旧 workspace，就是为了这里）
         6b-i  成功 ⇒ 关闭 b，回复 Failed{kind: RemountFailed, detail}，挂载回到 dev
         6b-ii 也失败 ⇒ 删除记录、回复 Failed{kind: Unmounted, detail}、主线程退出，退出码 1
```

- **唯一会导致"什么都没挂"的路径是 6b-ii**：新旧两个挂载都失败。现实中的触发条件是"窗口内盘符被别的进程抢走"，并且原 workspace 也重挂不上。CLI 如实报告。
- **记录在整个过程中一直是 Live**：记录文件的句柄由主线程持有，跨越卸载和重挂都不关闭。读者在改写的瞬间可能读到空内容或半截内容，所以**读者遇到解析失败时，等 50 ms 重试一次**（`record_is_live` 的语义不变）。
- 管道名包含 pid，switch 前后不变。

### 5.3 判据

| # | 判据 | 变异（必须红） |
| --- | --- | --- |
| W1 | **（D8 = B）** 挂载 `dev` 于 `U:`，`feat` 分支里有一个 `dev` 没有的文件 `/only-feat`。子进程 bash 的 cwd 为 `U:\`，执行 `fs-cli checkout feat` ⇒ 退出码 0；随后 `U:\only-feat` 存在；`status` 显示 `feat`；记录的 `workspace/branch` 是 `feat`；另一线程在整个 switch 期间每 10 ms 调一次 `record_is_live`，**从未**得到 `Gone` 或 `Stale` | 改成删记录再重建 ⇒ 轮询看到 `Gone` |
| W2 | 打开 `U:\f` 不关，`checkout feat` ⇒ 约 10 s 后退出码 4，消息含 `1 file`；`U:` 仍是 `dev`；另一进程随后能 `workspace_open(feat)`（b 的锁已经释放） | 不数文件句柄；或 `HandlesOpen` 后不关闭 b（最后一步得到 `WorkspaceInUse`） |
| W3 | `feat` 被另一进程持有：`checkout feat` ⇒ 退出码 4（`WorkspaceInUse`）；**挂载从未中断**：另一线程每 10 ms 查询 `U:\` 是否存在，从未见到不存在 | 把"打开 b"挪到卸载之后 ⇒ 轮询见到中断 |
| W4 | **窗口度量**：W1 的成功 switch 中，`U:\` 不可见的最长连续时间。超过 `SWITCH_GAP_RED = 2000 ms` ⇒ 失败带红值 | —（度量） |
| W5 | **Closing 期间的打开立即失败**：管道侧暂停点 `switch-after-quiesce=3000`，暂停期间经 `U:` 打开一个文件，**1 s 内**返回失败（`ERROR_NOT_READY` 或 Win32 对 `STATUS_VOLUME_DISMOUNTED` 的映射，按实测值写入判据） | Closing 改为阻塞 ⇒ 打开挂住超过 1 s；暂停结束后卸载死锁 |
| W6 | **6b 回退**：故障注入点 `WCAS_CLI_FAIL_AT=switch-mount-new`（形态同 `crash_point`，在 `fs-cli`）让新挂载失败 ⇒ 退出码 1，消息含 `RemountFailed`；`U:` 重新服务 `dev`（`status` 为 `dev`，挂载前写入的内容仍在）；`feat` 的锁已释放 | 去掉回退 ⇒ `U:` 不存在 |
| W7 | 不在挂载内执行 `checkout feat` ⇒ 只创建或恢复，**不触发 switch**（S1 A3b 的回归） | 发现逻辑被绕过、总是 switch |
| W8 | **V13（只记录）**：W1 之后，cwd 停在 `U:\sub` 的子进程 shell（bash、cmd、PowerShell 各一个）的下一条命令（`ls` / `dir` / `Get-ChildItem`）结果，以及 `cd .` 之后的结果。按三态记录，写进 ADR-010 §7.3 的用户说明 | — |

## 6. 目录挂载点：补量 V12

worker 量到：`SetCurrentDirectory` 对目录挂载点返回 267（`ERROR_DIRECTORY`）；bash 的 `cd` 可以进去（`b806270` 的判据就是这样测的）。cmd 与 PowerShell 还没量。

| # | 判据 | 变异 |
| --- | --- | --- |
| V12 | 目录挂载点 `…\mnt`：分别用 `cmd /c "cd /d …\mnt && fs-cli status"`、`powershell -Command "Set-Location …\mnt; fs-cli status"`、`bash -c "cd …\mnt && fs-cli status"` 执行，记录每个的退出码与输出（三态）。结果写进 ADR-010 §4.4：哪些 shell 下可以在目录挂载点内使用路径发现 | — |

## 7. 未验证项

| # | 项 | 判据 |
| --- | --- | --- |
| V6 | `FileInfoTimeout = 0` 时没有数据缓存 | R1 第 ③ 步（红 ⇒ 停卡） |
| V12 | cmd / PowerShell 能否 `cd` 进目录挂载点 | V12 |
| V13 | 重挂之后，shell 留在旧卷上的 cwd 会怎样 | W8 |
| V14 | 应用 `CloseHandle` 之后 `Close` 回调的延迟（决定 `wait_ms` 是否够用） | R2、W2 的读数（记录从关闭到计数归零的时间） |

## 8. 停卡条件

1. G4b 打开严格模式后出现 panic：S2 遗留的真实嵌套；
2. R1 第 ③ 步读到旧内容：V6 不成立，restore 依赖 U9；
3. W5 出现死锁：Closing 下仍有回调阻塞在准入上，卸载等不到分发线程退出。

## 9. ADR-010 修订

第 2、3、5 项**已随本卡提交**（判据影响：无（澄清））；第 1、4 项依赖 D8，**在 D8 裁决后、第 1 步开工前**由 architect 补交（判据影响：改变）。

1. **§6.2 / §7.3**：准入条件从"无打开句柄"改为 D8 的裁决结果。若为 B：把文件句柄与目录句柄分开写，目录句柄放行并提示；补上准入机制（§3）与 switch 的顺序（§5.2）。
   **判据影响：改变**——ADR §13 原 A19 的"有句柄时 restore/switch 拒绝"在 B 下改为"有**文件**句柄时拒绝"；
2. **§4.4**：删掉 `951fa78` 那段"目录挂载点不支持路径发现"（`b806270` 已经用 reparse point 判定让它可以工作），改写为实际方法，并补上 V12 的读数；
3. **§10.3**：按**实际实现**写闸的落位：`Gate` 是 `RwLock<()>`，`BaseState` 另用 `Mutex`（`crates/fs-workspace/src/gate.rs:23-26`、`workspace.rs:49`），与 S2 卡 §1.2 原设想的"`RwLock<BaseState>`"不同；外加 G4 的修复说明；
4. **§10.4**：`restore` / `switch` 的请求与响应（§4.1、§5.2），`HandlesOpen{files, dirs}`，`status.handles` 改为 `{files, dirs}`，`RemountFailed` 与 `Unmounted` 两个 kind；
5. **补入 S2 卡**：`docs/process/adr010-s2-card.md` 当初没有随 S2 合入 main（`66b7c84` 停留在 `arch/adr010-s2` 分支上），本卡所在提交把它补进来，状态行改为"已收口 `e1632e1`"，并注明 §1.2 与实现的差异。

## 10. 提交建议

1. G4a / G4b / G4c（闸的仪器）。**红了就停**；
2. `Handle.is_dir`、准入状态机，三个按路径进入的回调取准入票；Q1、Q2；
3. 管道 `restore` 与 CLI 路由；R1–R5。**R1 第 ③ 步红了就停**；
4. 主线程循环化与 `switch` 的顺序；W1–W3、W5、W6；
5. 度量与读数：W4、W8、V12；
6. 度量读数（V12）写进 ADR-010 §4.4。D8 的结果在第 1 步开工之前就要确定，§9 第 1、4 项随裁决由 architect 补交。
