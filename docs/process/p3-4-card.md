# P3-4 任务卡（草案）：自写最小 WinFsp FFI + 真实挂载 + L2/L3 实测

- 状态：**草案（architect 起草，2026-10-26）——可派发**（§9.1 已由 owner 裁决 (a)；§9.2/§9.3 为本卡开工首项，按建议执行、以 L2 结果定稿）。
- **总纲（owner，2026-10-26）**：**只要保证用起来与普通文件系统（NTFS）的真实行为一致即可**，适用于以前和以后的开发内容。本卡每一处 NTFS 对照以它为准：对照结果与 ADR/卡的文本不符时，**以实测为准修订文本**，不以文本约束实测；"有意差异"只允许传输层确实无法表达的情形，并须写明证据 ref。
- 上游：ADR-009（W1–W14、§5、§6、§8、§12）；`docs/process/p3-winfsp-ffi-surface.md`（FFI 面）；P3-3a（adapter 骨架 + mock）；P3-3b（真实 `fs-api` 后端重跑契约测试）；P3-2（G1–G4）。
- 环境：WinFsp 2.1 运行时已装、驱动已加载；SDK 在 `F:/work/winfsp-sdk/DYNAMIC/{inc,lib}`；冒烟已过（`LoadLibraryW` + `FspVersion` → status 0）。
- 下游建议拆分（见 §10）：**P3-4a**（本卡：FFI + 挂载 + L2 语义）／**P3-4b**（spec §2 P3 第 5、6 条：真实 workload 与 NTFS 性能对照）。

---

## 0. 本卡的定位

P3-3a/3b 把"所有能用普通 Rust 测的语义"做完了；本卡只做**转译**（WinFsp 回调 → `WorkspaceOps`）与**只有真实挂载才能判的事**（L2/L3）。
因此本卡的判据分三类，报告必须按类计数，**L2/L3 不得充 L1 的数量**（`p3-acceptance-skeleton.md` §5）：

| 类 | 内容 | 能否在 CI 跑 |
| --- | --- | --- |
| L1 | FFI 封装层的纯 Rust 部分：状态码映射、UTF-16、FileContext 所有权、`catch_unwind`、ABI 静态断言 | 是（`cfg(windows)` 编译 + 单测） |
| L2 | 真实挂载上的 Win32 行为，含与 NTFS 对照 | 否（需驱动、挂载点） |
| L3 | 环境与生命周期：缺 SDK 编译失败、装/挂/卸/重挂、管理员、有句柄卸载 | 否 |

---

## 1. 开工前必须消掉的"未验证/待环境"清单（ADR-009 等文档里的前置）

按三态口径：**通过 / 失败（带红值）/ 空转（构造失败）**；"待环境"与"未验证"分档（骨架 §7.6）。每一项开工时逐条消，**不得把文档依据写成已验证**。

| # | 前置（出处） | 现有依据等级 | P3-4 的验证方式 | 不成立时的后果 |
| --- | --- | --- | --- | --- |
| U1 | **rename-replace 目标有打开句柄时 FSD 不下发 Rename**（ADR-009 §5.4；P3-2 卡 G4 前提） | 头文件文档（`winfsp.h:612-617`："A file cannot be renamed if a file with the same name exists and has open handles"）；**无行为实测** | L2：打开目标 `T` 不关 → `MoveFileEx(S, T, REPLACE_EXISTING)`；断言 Win32 失败且 adapter 的 `Rename` 回调**未被调用**（回调计数 0） | 改用 SUPERSEDE/保留语义（ADR-009 §5.4 的"若实测不成立"分支），G4 判据 5 重写 |
| U2 | **SUPERSEDE 同 FileId 覆盖**（owner 裁决 (a)；ADR-009 §4.2 修订） | 已裁决；WinFsp 2.1 源码 `fsop.c:929-981` | L2：`CREATE_ALWAYS` 命中既有文件 ⇒ `Overwrite(ReplaceFileAttributes=TRUE)`、`TRUNCATE_EXISTING`/`OPEN_ALWAYS+truncate` ⇒ `FALSE`；FileIndex 前后不变；旧句柄读到 size 0；与 NTFS 对照 FileIndex | 以 NTFS 实测为准修订 ADR（总纲） |
| U3 | **share mode 与 delete-pending 由 adapter 句柄表判定**（ADR-009 W6/W7） | WinFsp-Design 文档称 FSD 已处理"file sharing、open 与 delete/rename 的冲突"；**无行为实测** | L2：双进程 9 格 share 组合 + delete-pending 新 open；同时记录 adapter 回调是否被调用 | 见 §9.2（单一来源） |
| U4 | **目录 rename 时子孙有打开句柄** | 头文件文档（`winfsp.h:615-616`：FSD 拒绝）；**无行为实测** | L2：打开 `/d/f` 不关 → rename `/d` → 断言 Win32 失败且回调未被调用 | 若会下发：adapter 必须保证子孙句柄的 binding 不失效（parent 是 FileId，ADR-009 §5.4 已覆盖）并补 L2 判据 |
| U5 | **默认 SD（`GetSecurityByName` 返回 `size=0`）被 OS 接受**（FFI 面 §2） | 无 | L2：`GetFileSecurity`/`icacls`/资源管理器属性页可读；创建/打开不因 SD 失败 | 绑定 `FspCreateSecurityDescriptor`/`FspDeleteSecurityDescriptor` 构造默认 SD（FFI 面已预留） |
| U6 | **byte-range locks 由 WinFsp 内核实现**（ADR-009 W14） | 文档 + 2.1 无用户态 LockControl 回调 | L2：双进程 `LockFileEx` 冲突、解锁、Close 时自动解锁 | 记为平台限制；若内核未实现则另卡 |
| U7 | **cached I/O × 锁/删除的一致性**（ADR-009 §13.1） | 无 | L2：锁持有期间另一句柄的 cached 读/写行为与 NTFS 对照 | 给数据库类 workload 用 `--no-cache`，记为平台限制 |
| U8 | **持久性验收点 = Close 后重开可读**（ADR-009 W9），含 mmap 写回 | 无 | L2：`write`/mmap 写 → Close → 卸载 → 重挂 → 读；cached 与 `--no-cache` 两种 | 调整 `FlushAndPurgeOnCleanup`/Flush 路径 |
| U9 | **`FspFileSystemNotify` 能让 `ReadDirectoryChangesW` 收到变更**（ADR-009 W13） | 无 | L2：监视目录 → 经挂载点创建/rename/删除 → 收到对应事件 | 记缺失；首版可不通知（FFI 面 P1 可省），但 spec 要求，P3-4b 必做 |
| U10 | **SUPERSEDE/OVERWRITE 的属性重置与 NTFS 一致**（ADR-009 §4.2 表 SUPERSEDE 行） | 无 | L2 NTFS 对照：`CREATE_ALWAYS`/`TRUNCATE_EXISTING` 后 `GetFileAttributes` | 以实测为准修订 ADR |
| U11 | **v2 构建产物拒绝 v3 夹具且字节不变**（P3-2 卡 G3 §3.2-5 发布前手工项） | 无 | 手工：保留的 v2 二进制打开 v3 夹具 | G3 拒绝方向未证 |
| U12 | **运行时版本/ABI 匹配**（FFI 面 §5.7–5.8） | 冒烟：`FspVersion` 返回 0（只证明 DLL 可加载） | L1：结构大小/字段偏移 static assert；启动时主/次版本比对 | 拒绝启动 |
| U13 | **有打开句柄时卸载被拒；强制卸载的数据语义**（ADR-009 W12、§8） | 无 | L3：打开句柄 → 正常卸载被拒；`--force` → 记录未刷数据丢失范围 | 修订 §8 |
| U14 | **8.3 短名不支持、大小写折叠行为**（ADR-009 W8、§4.1） | 文档（WinFsp NTFS-Compatibility：不支持短名） | L2：`dir /x` 无短名；折叠歧义 → `STATUS_OBJECT_NAME_COLLISION` | 修订 §4.1 |

> 维护规则：本表每消一行，提交说明的判据影响行写"U<n>：通过/失败（红值）/空转"，并在 ADR 对应处把"未验证"改成结论 + 证据 ref；**不得只改表不改 ADR**。

---

## 2. FFI 最小面（以 `p3-winfsp-ffi-surface.md` 为准，本卡只列"必须绑定"）

- **主机 API（P0）**：`FspFileSystemCreate`/`Delete`/`SetMountPoint`/`StartDispatcher`/`StopDispatcher`/`AcquireDirectoryBuffer`/`AddDirInfo`/`ReleaseDirectoryBuffer`；**P1**：`RemoveMountPoint`、`StopServiceIfNecessary`、`FspVersion`、`NotifyBegin/End/Notify` + `AddNotifyInfo`（通知可在 P3-4b 才启用，但绑定在本卡完成）。
- **回调（P0）**：`GetVolumeInfo`、`GetSecurityByName`、`Open`、`Create`、`Overwrite`（**P0**：SUPERSEDE/OVERWRITE 只经它下发；只凭 `ReplaceFileAttributes` 区分二者，ADR-009 §4.2 (a) 判据 1）、`Read`、`Write`、`GetFileInfo`、`ReadDirectory`、`Cleanup`、`Close`；**P1**：`SetFileSize`、`SetBasicInfo`、`Rename`、`CanDelete`、`SetDelete`（或只 `CanDelete`，见 §9.2）、`Flush`、`GetSecurity`、`SetSecurity`（返回 `STATUS_NOT_SUPPORTED`）。
- **不绑定**：异步完成、服务化、POSIX/路径辅助、EA/stream/reparse、`FspFileSystemOp*`、内核模式 API（FFI 面 §7）。
- **卷参数**：ADR-009 §8 的 flag 集合 + **FileContext 模式**（§9.3 裁定）。位域只经 bindgen 生成的访问器设置。
- **绑定生成**：bindgen 产物入库，CI 重新生成并 `git diff --exit-code`；缺 SDK 时 `build.rs` 编译失败并打印安装指引（L3）。

**FFI 层判据（L1）**
1. 回调体全部 `catch_unwind`；注入 panic → 返回 `STATUS_UNEXPECTED_IO_ERROR`，进程不崩（测试用伪回调直接调用 trampoline）。
2. `FileContext` 所有权：`Open/Create` 建立、`Close` 唯一释放；重复 Close / Close 后使用由封装层拒绝（debug assert + 计数）。
3. UTF-16：非法代理对、超长组件、NUL → 映射到 `fs-windows::path` 的拒绝码，不 panic。
4. 状态码：只能调用 `fs-windows` 的映射（P3-3a 判据 8 同款形状查）。
5. ABI：`FSP_FSCTL_VOLUME_PARAMS`/`FSP_FSCTL_FILE_INFO`/`FSP_FSCTL_DIR_INFO`/`FSP_FILE_SYSTEM_INTERFACE`（64 槽）大小与关键偏移 static assert。

---

## 3. 挂载生命周期判据（L3，带红法）

| # | 场景 | 期望 | 红法 |
| --- | --- | --- | --- |
| M1 | 缺 SDK（`WINFSP_ROOT` 指空目录）编译 | 编译失败，信息含安装指引 | 跳过编译当通过 ⇒ 红 |
| M2 | 挂盘符 → 列根目录 → 卸载 → 同盘符再挂 | 两次都成功，内容一致 | 第二次挂载失败/内容丢 |
| M3 | 挂目录挂载点（**不存在的路径**——WinFsp 以 FILE_CREATE 自建并 DELETE_ON_CLOSE 自删，判据 341；2026-10-27 依 ddca7bd 源码修订，原文"空目录…恢复"是错误假设） | 成功；卸载后挂载点目录**消失**（DELETE_ON_CLOSE） | **已存在的目录被挂载**（0xC0000035）/卸载后目录残留 |
| M4 | 同一 workspace 第二次挂载 | typed `WorkspaceInUse`，不改任何文件（W2；backlog B-14） | 两个挂载并存 |
| M5 | 有打开句柄时正常卸载 | 拒绝，打印持有者信息（如可得） | 静默卸载 |
| M6 | `--force` 卸载 | 成功；已 flush 的数据重挂可读；未刷数据的丢失范围写进报告 | 已 flush 数据丢失 |
| M7 | 进程被杀（`taskkill /F`） | OS 回收卷；重挂可读已 flush 数据；overlay 锁释放 | 锁残留导致重挂 `WorkspaceInUse` |
| M8 | 非管理员挂载/访问 | 行为写进报告（允许/拒绝都可，但必须可复现、有明确错误） | 不确定行为 |
| M9 | 运行时版本不匹配（模拟） | 拒绝启动，打印两端版本 | 静默启动 |

---

## 4. L2 语义实测（与 NTFS 对照；每条至少 3 次重复）

对照方法：同一测试程序分别跑在 **NTFS 临时目录** 与 **本 FS 挂载点**，比较 Win32 返回值/`GetLastError`/`FileIndex`/内容；差异逐条记录为"一致 / 有意差异（引用 ADR 条款）/ 缺陷"。

1. **disposition 六种 × 存在性**（ADR-009 §4.2）：返回码、`FileIndex` 是否变化、旧句柄读到什么（U2、U10）；
2. **rename**：`MoveFileEx` 普通/替换/跨目录/case-only；目标有句柄（U1）；目录子孙有句柄（U4）；
3. **share 9 格 + delete-pending**（U3）：双进程；同时记录 adapter 回调计数；
4. **两段删除**：`DeleteFile` 有/无其它句柄；`FILE_FLAG_DELETE_ON_CLOSE`；`SetFileInformationByHandle(FileDispositionInfo)` 置/清；
5. **句柄跨 rename**：打开 → rename → 读/写/`GetFileInformationByHandle`；
6. **枚举**：`FindFirstFile/NextFile` 大目录（> 一页缓冲）、枚举中 rename/删除、`.`/`..`、大小写不同的两个条目（W8）；
7. **元数据**：`SetFileTime`/`GetFileTime` 100ns 往返、1601/超界拒绝；属性 READONLY 拒写/拒删；
8. **读写边界**：EOF 短读、`offset>=size` → `ERROR_HANDLE_EOF`、稀疏写零化（KI-9 同族）；
9. **mmap/cached**（U7、U8）：`CreateFileMapping` 读写 → Close → 重开；`--no-cache` 下 mmap 被拒（W9）；
10. **锁**（U6）：`LockFileEx` 冲突/解锁/Close 自动解锁；
11. **通知**（U9）；
12. **SD**（U5）；**短名与大小写**（U14）；**卷信息** `GetDiskFreeSpaceEx`/`GetVolumeInformation` 值稳定。

每条的报告格式：`场景 | NTFS | LayerFS | 结论（一致/有意差异+ADR 条款/缺陷）| 证据 ref`。

---

## 5. 明确不做（本卡）

- 真实 workload（SQLite/git/npm/Python/Unity）与性能对照 → **P3-4b**；
- ACL 强制（B-11）、在线 GC、并发打开者（会破 W2/§5.4）、named streams、reparse、EA、服务化；
- 修改核心语义：若 L2 发现核心语义需改（如 U2），**停卡**并按判据影响行上报，不在本卡顺手改。

## 6. 交付与验收

- 分支 `p3-4a/ffi-mount`，基于 `origin/main`（**须在 G1 修订卡合入之后**：本卡依赖 (a) 的 `Overwrite` 语义）；提交说明首行写判据影响行（含 U<n> 的消项结论）；
- 每次提交附：L1 测试结果、L2/L3 用例的**执行记录**（不是"已写"）、§1 表的当前状态；
- QA 独立复跑 L2（至少 3 次）与 L3 M1–M9；变异：把 `Overwrite` 回调实现成"新建 FileId"/"保留旧内容"（按 §9.1 裁定的反面）⇒ 对应 L2 第 1 条必须红。

---

## 9. 开工前裁定（9.1 已关闭；9.2/9.3 为开工首项）

### 9.1 SUPERSEDE 的身份语义与 WinFsp 接口冲突（**已关闭：owner 2026-10-26 裁决 (a)**，修订见 ADR-009 §4.2 与 `docs/reports/supersede-a-impact.md`；以下保留为裁决依据）

**事实（接口证据，不是推测）**：
- WinFsp 2.1 用户态分发（`src/dll/fsop.c`，tag `v2.1`）：`FILE_SUPERSEDE` 与 `FILE_OVERWRITE_IF` **同走** `FspFileSystemOpCreate_FileOverwriteIf`（fsop.c:929-931）——目标存在则先调 **`Open`**（fsop.c:696-708），返回 `FILE_SUPERSEDED`/`FILE_OVERWRITTEN`；随后 FSD 发 `Overwrite` 请求，`FspFileSystemOpOverwrite` 把**同一个 FileContext** 与 `Supersede` 位作为 `ReplaceFileAttributes` 传给 `Overwrite` 回调（fsop.c:946-972；回调签名 `winfsp.h:326-347`；请求位 `fsctl.h:376-378`）；
- `Create` 回调只在**目标不存在**时调用（fsop.c:710-745 的 `Create` 分支）——即 **SUPERSEDE 是对"已打开的同一 FileContext"做覆盖**，用户态 FS 拿不到"新建 inode"的时机；
- 参考实现 MemFS 的 `Overwrite` 在同一 file node 上截断与替换属性；
- 默认 FileContext 语义是"**同一文件名在任何地方仍打开时返回同一 file node**"（`winfsp.h:172-177`）——FSD 按名字维护文件节点，旧句柄与新打开共享同一节点。

**冲突**：ADR-009 §4.2 写"SUPERSEDE = 解除旧名字绑定并创建新 inode（新 FileId）；旧 inode 保留、旧句柄读旧内容"，并据此有 B-8、P3-3a 判据 2、P3-2 G1 SUPERSEDE 行、ADR §5.4 可见性表第一行。经 WinFsp 接口，该语义**无法被触发**：旧句柄与覆盖后的文件是同一个 FSD 节点，读到的是**覆盖后**的内容。

**选项**：
- **(a) 采用 WinFsp/NTFS 语义（推荐）**：SUPERSEDE = **同一 FileId** 上截断为 0 + 属性按 `ReplaceFileAttributes` 替换；与 OVERWRITE 的区别只在属性是替换还是合并。核心的 G1"SUPERSEDE 新 FileId"改为"保留 FileId"；ADR §4.2 SUPERSEDE 行、§5.4 可见性表 SUPERSEDE 行、B-8、P3-3a 判据 2 全部改写；"旧 inode 只增保留"仅剩 `remove_at` 的 tombstone 一类（B-10 范围缩小）。
- **(b) 保留"新 FileId"语义**：adapter 在 `Overwrite(Supersede=1)` 里调用核心的"替换为新 inode"，但 FSD 节点仍是同一个；旧句柄的 FileContext 需要被重新指向新 FileId（否则旧句柄读旧 inode，与 FSD 认为"同一文件"矛盾）。**复杂且与 FSD 模型冲突，不推荐。**
- 无论选哪个，L2 用 NTFS 对照确认 `CREATE_ALWAYS` 命中既有文件时 `FileIndex` 是否变化（U2/U10），**以实测为准**（ADR §4.2 已写此条）。

**判据影响**：选 (a) ⇒ `改变`（P3-2 G1 SUPERSEDE 行、P3-3a 判据 2、ADR-009 §4.2/§5.4、B-8）——若 G1 已按"新 FileId"实现并合入，需停卡评估。

### 9.2 share/delete-pending 的单一来源（**阻塞**）

WinFsp 的设计目标之一是让用户态 FS **不必**处理 share 与 open/delete/rename 冲突（WinFsp-Design）。若 FSD 已做 `IoCheckShareAccess` 与 delete-pending 拒绝，而 adapter（W6/W7）再判一次，两套规则只要有一处不一致就会出现"FSD 放行、adapter 拒绝"（或反之）的不可解释失败。

**建议**：生产路径**以 FSD 为准**，adapter 的 share/delete 模型只作**断言/诊断**（发现与 FSD 结论不一致时记录，不改变结果），并以 U3 的 L2 结果定稿；`fs-windows` 的 share/delete 模块保留为 L1 规则与 mock 的规格。若 L2 证明 FSD 不做某项检查，再把该项提升为 adapter 强制。需同步修订 ADR-009 W6/W7 的措辞（"在 adapter 句柄表上判定" → "以 FSD 为准，adapter 镜像校验"）。

### 9.3 FileContext 模式（设计裁定）

- 默认模式：FileContext = **file node**（同名打开共享）——与"句柄表按每次打开记录 desired/share/delete_on_close"不匹配；
- `UmFileContextIsUserContext2`：FileContext = **每次打开的描述符**；
- `UmFileContextIsFullContext`：两者都有（UserContext = 节点，UserContext2 = 描述符）。

**建议**：`UmFileContextIsFullContext`——节点（UserContext）存 `FileId` 与按节点的状态（引用计数、delete-pending 镜像），描述符（UserContext2）存每次打开的 `desired/share/delete_on_close/binding`。这与 ADR-009 W3（句柄存 binding）和 §9.2（以 FSD 为准、节点级镜像）同时相容。需要 ADR-009 §8 卷参数补这一项。

---

## 10. 与 ADR-009 §11 卡序的差异（文档一致性）

ADR-009 §11 的"交付顺序"仍是起草时的编号（P3-3 = 正式 adapter、P3-4 = 端到端与基线），与当前卡序（P3-3a mock → P3-3b 真实后端 → P3-4 FFI + 挂载）不一致。建议：
- P3-4 拆为 **P3-4a**（本卡：FFI + 挂载 + L2 语义 + L3 生命周期）与 **P3-4b**（spec §2 P3 第 5、6 条：SQLite WAL ACID/kill 后 `integrity_check`、git/npm/Python/构建工具、Unity 导入、NTFS 性能对照 P50/P95/P99、CPU/RAM、写放大、空间节省）；
- 合入本卡时同步更新 ADR-009 §11。
