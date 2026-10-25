# P3 最小 WinFsp FFI 面（长期资产）

- 状态：草案（architect，2026-10-25）；owner 已决定**自写最小 FFI**，不用 GPL crate、不评估 `winfsp_wrs`
- 版本钉死：**WinFsp 2.1**（本机运行时 `2.1.25156`；头文件取 `winfsp/winfsp v2.1` tag，记录 commit 哈希）
- 相关：ADR-009（边界/语义）、`docs/adr/ADR-005-inode-record-v3-draft.md`

---

## 1. 分层：绑定面 vs 封装面

| 层 | crate/module | 内容 | 允许 `unsafe` | 规则 |
| --- | --- | --- | --- | --- |
| 原始绑定 | `crates/winfsp-sys` | `extern "C"` 函数、`#[repr(C)]` 结构、常量；由 **bindgen 在开发期**从 `inc/winfsp/winfsp.h` 生成后**提交入库** | 是（只读数据） | 不手写位域；生成脚本 + CI diff；版本升级只改这一层 |
| 安全封装 | `crates/fs-winfsp/src/ffi` | RAII `Volume`/`MountPoint`/`DirectoryBuffer`/`Notification`；回调 trampoline；UTF-16 与 status 转换 | 是（**唯一**模块） | 指针校验、`catch_unwind` 跨 FFI 边界、回调上下文所有权 |
| 服务实现 | `crates/fs-winfsp/src/host` | 实现 `FSP_FILE_SYSTEM_INTERFACE` 回调，调用 `fs-windows` 语义层与 `fs-api` | **否** | 不出现裸指针/`unsafe`；只经安全封装 |

架构红线（`architecture.md` §3.3）：平台 API 只允许出现在批准的单点模块——本设计把 WinFsp FFI 集中在 `winfsp-sys` + `fs-winfsp/src/ffi`，其余代码不得 `use winfsp_sys`。

**为什么用 bindgen 而不是手抄**：`FSP_FSCTL_VOLUME_PARAMS` 是 C **位域**结构（`CaseSensitiveSearch:1` 等）；手写 Rust 位布局极易错，且错误表现为"某个 flag 静默不生效"。bindgen 生成位域访问器（`set_CaseSensitiveSearch` 等），ABI 由头文件保证。生成物是数据/声明，链接与使用在 WinFsp 的 FLOSS exception 下（项目为 MIT/Apache 的 FLOSS，owner 已确认自写路线）。

---

## 2. 主机 API（`FspFileSystem*`）——按需要分级

| 函数 | 用途 | 级别 | 可否省 |
| --- | --- | --- | --- |
| `FspFileSystemCreate` | 创建卷/文件系统对象 | P0 | 不可 |
| `FspFileSystemDelete` | 销毁 | P0 | 不可 |
| `FspFileSystemSetMountPoint` | 挂盘符/目录 | P0 | 不可（UNC 需要 `SetMountPointEx`） |
| `FspFileSystemStartDispatcher` | 启动分发线程池 | P0 | 不可 |
| `FspFileSystemStopDispatcher` | 停止分发 | P0 | 不可 |
| `FspFileSystemAcquireDirectoryBuffer` | 枚举缓冲 | P0 | 不可（ReadDirectory 用） |
| `FspFileSystemAddDirInfo` | 向缓冲加一条目录项 | P0 | 不可 |
| `FspFileSystemReleaseDirectoryBuffer` | 释放缓冲 | P0 | 不可 |
| `FspFileSystemStopServiceIfNecessary` | 无句柄时优雅卸载/服务控制 | P1 | 可（首版用强制停止 + 句柄检查） |
| `FspFileSystemRemoveMountPoint` | 卸载时移除挂载点 | P1 | 可（进程退出时 OS 清理；但优雅卸载需要） |
| `FspFileSystemNotifyBegin` / `NotifyEnd` / `Notify` | 目录变更通知 | P1 | 可（首版不通知，但 spec 要求 → P3-4 必做） |
| `FspFileSystemAddNotifyInfo` | 构造通知条目 | P1 | 可（同上） |
| `FspFileSystemSetOperationGuardStrategy` | 选择操作并发/守卫策略 | P1 | 可（默认策略可用；需要并发时显式设） |
| `FspFileSystemGetOperationContext` | 取当前操作上下文（进程 id、case 标志） | P1 | 可（仅诊断/特殊语义需要） |
| `FspFileSystemSetMountPointEx` | 带选项的挂载点（UNC/网络） | P2 | 可（UNC 验收时再加） |
| `FspFileSystemSetDebugLog` / `SetDebugLogF` | 调试日志 | P2 | 可 |
| `FspVersion` | 运行时版本检查 | P1 | 可（也可读 DLL 版本；建议绑定用于启动日志/拒绝不匹配） |
| `FspFileSystemPreflight` | 预检挂载点 | P2 | 可 |
| `FspFileSystemFillDirectoryBuffer` / `ReadDirectoryBuffer` | 自管枚举缓冲 | P2 | 可（用 Acquire/Add/Release 即可） |
| `FspFileSystemSendResponse` | 异步完成 | — | **不做**（P3 全同步；无 `STATUS_PENDING`） |
| `FspFileSystemEnterOperation` / `LeaveOperation` / `SetOperation...` | 异步/手动守卫 | — | **不做** |
| `FspFileSystemResolveReparsePoints`、`FspFileSystemCanReplaceReparsePoint`、reparse/stream/EA 辅助 | 范围外 | — | **不做** |
| `FspFileSystemOp*`（OpCreate/OpRead/...） | 内部/服务用 | — | **不做** |
| `FspService*` | 作为 Windows 服务运行 | — | **不做**（P3 以控制台进程挂载；服务化另议） |
| `FspPosix*`、`FspPath*`、`FspCreateSecurityDescriptor`/`FspSetSecurityDescriptor`/`FspDeleteSecurityDescriptor` | POSIX 映射/默认 SD 构造 | P2 | **暂不做**；若 L2 证明"SD 长度 0"不被 OS 接受，再只绑定 `FspCreateSecurityDescriptor`+`FspDeleteSecurityDescriptor` 构造默认 SD |

---

## 3. 回调面（`FSP_FILE_SYSTEM_INTERFACE`，64 槽）

只实现需要的回调，其余置 `NULL`（WinFsp 对未实现项返回默认错误）。**结构体必须整表定义**（大小 = 64 指针，有 static assert）。

| 回调 | 级别 | 说明 |
| --- | --- | --- |
| `GetVolumeInfo` | P0 | 卷参数/容量/空闲 |
| `GetSecurityByName` | P0 | 名称 → 属性 + 默认 SD（`size=0` 先试） |
| `Open` | P0 | 打开；返回 FileContext |
| `Create` | P0 | disposition（§4.2 表） |
| `Read` | P0 | FileId 读 |
| `Write` | P0 | FileId 写 |
| `GetFileInfo` | P0 | 句柄信息 |
| `ReadDirectory` | P0 | 枚举/续读 |
| `Cleanup` | P0 | 两段删除/刷缓存 |
| `Close` | P0 | 释放 FileContext |
| `SetFileSize` | P1 | 截断/扩展 |
| `SetBasicInfo` | P1 | 时间/属性 |
| `Rename` | P1 | ReplaceIfExists + case 规则 |
| `CanDelete` | P1 | 非空/只读检查 |
| `SetDelete` | P1 | 置/清 pending |
| `Flush` | P1 | 文件/卷 flush |
| `GetSecurity` | P1 | 句柄 SD（默认） |
| `SetSecurity` | P1 | **返回 `STATUS_NOT_SUPPORTED`** |
| `GetDirInfoByName` | P2 | 目录项查询加速（可省） |
| `Overwrite` / `OverwriteEx` | P2 | 可省（用 Create/truncate 组合） |
| `SetVolumeLabel` / `Control` / `GetEa` / `SetEa` / `GetStreamInfo` / reparse / `CreateEx` | — | **NULL**（范围外，OS 得到默认拒绝） |
| `DispatcherStopped` | P2 | 可选：记录停止 |
| `Reserved[31]` | — | 全 NULL |

---

## 4. 类型与常量（bindgen 生成）

- 结构：`FSP_FILE_SYSTEM`、`FSP_FILE_SYSTEM_INTERFACE`、`FSP_FSCTL_VOLUME_PARAMS`（位域）、`FSP_FSCTL_FILE_INFO`、`FSP_FSCTL_DIR_INFO`、
  `FSP_FSCTL_VOLUME_INFO`、`FSP_FSCTL_NOTIFY_INFO`、`FSP_FILE_SYSTEM_OPERATION_CONTEXT`、`SECURITY_DESCRIPTOR`；
- 基础类型：`NTSTATUS`、`ULONG/UINT32/UINT64/BOOLEAN/PWSTR/PVOID/SIZE_T`（优先用 `windows-sys` 的别名，避免重复定义）；
- 常量：`STATUS_*`（本 ADR 表用到的子集）、`FspCleanupDelete/SetArchiveBit/SetLastAccessTime/SetLastWriteTime/SetChangeTime`、
  `FSP_FSCTL_VOLUME_PARAMS` 的 flag 常量、`FSP_FSCTL_VOLUME_PARAMS` 的结构版本常量；
- **位域只经生成的访问器读写**；不得用 transmute 或手写 offset。

---

## 5. 安全封装的不变量（`fs-winfsp/src/ffi`）

1. 每个 `extern "system"` 回调体首尾包 `catch_unwind`；panic → 记日志并返回 `STATUS_UNEXPECTED_IO_ERROR`，**绝不跨 FFI 展开**。
2. `FileContext` = `Box<Handle>` 的裸指针；`Create/Open` 建立所有权，`Close` 唯一释放点。`Cleanup` 不释放。
3. 所有 `PWSTR` 在使用前转成 `&[u16]` 并校验长度上界（`MAX_PATH`/组件上限）；不保存指针跨回调。
4. 所有 `*mut/*const` 在使用前 `is_null` 检查；输出参数写前检查容量。
5. 目录缓冲：`Acquire → AddDirInfo* → Release` 在同一个栈帧内配对；错误路径也必须 Release（RAII）。
6. `Volume` 的 Drop 顺序：StopDispatcher → RemoveMountPoint（若挂载）→ Delete；顺序错会崩。
7. 运行时版本：启动时 `FspVersion()`（或 DLL 版本）与编译期 2.1 比较；主/次不匹配 → 拒绝启动并打印两端版本。
8. ABI 检查：`build.rs` 用 `static_assertions` 断言关键结构大小/关键字段偏移（与 C 头文件的 `FSP_FSCTL_STATIC_ASSERT` 对齐）。

---

## 6. 构建与版本钉死

- `build.rs` 查找顺序：`WINFSP_ROOT` 环境变量 → `%ProgramFiles(x86)%\WinFsp` → `%ProgramFiles%\WinFsp`；需要 `inc/winfsp/winfsp.h` 与 `lib/winfsp-x64.lib`。
  **缺失即编译失败并打印安装指引**（L3 要求：不得跳过编译当通过）。
- 链接：`winfsp-x64.lib`（x64）；运行依赖 `winfsp-x64.dll`（≥2.1）。
- 绑定再生成：`tools/gen-winfsp-bindings.ps1`（或 bash + clang）→ `crates/winfsp-sys/src/bindings.rs`；CI 任务重新生成并 `git diff --exit-code`，防止头文件与绑定漂移。
- 升级流程：改 tag/commit → 重新生成 → 全量 L1/L2 → 记录 changelog（含 `FSP_FILE_SYSTEM_INTERFACE` 大小/新回调）。

---

## 7. 明确不绑定的面（长期边界）

- `FspService*`（服务化）、`FspPosix*`（POSIX 映射）、`FspPath*`、EA/stream/reparse 辅助、`FspFileSystemOp*`、异步操作 API、
  `FspFileSystemSendResponse`、内核模式 API（`FspFsctl*`）、`FspDebugLog*` 的变体（除 `SetDebugLog`）。
- 这些一旦需要，必须**新卡 + 本节更新**；不得在业务代码里临时 `extern` 一个未登记的函数。

---

## 8. 与 P3 卡序的关系

建议把 **P3-3a（adapter 骨架 + `WorkspaceOps` trait + mock 后端）放在 P3-2 之前**（或与其并行）。理由：

- adapter 是 `fs-api` 的**唯一消费者**，先写它能让 G1/G2/G4 的形状由使用方定义，避免核心先冻结一个"看起来合理但用起来别扭"的 API；
- mock 后端下可跑完 L1 全部语义与 adapter 胶水测试，**不需要 SDK**；P3-2 只需实现 trait。
- 约束：trait 保持最小（只含 adapter 需要的操作），不得把 `fs-api` 拆成"为 adapter 定制"的碎片；trait 变更由 P3-2 裁决。
