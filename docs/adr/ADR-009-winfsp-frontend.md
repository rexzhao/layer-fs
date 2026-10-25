# ADR-009：WinFsp 前端（P3）——边界、Windows 语义与驱动无关层

- 状态：**草案（Proposed）**——architect 起草；P3-1（L1 驱动无关层）与 P3-2（真实挂载）的设计输入
- 日期：2026-10-25
- 依据：spec §2 Phase 3、§3 分层、§6 P3 矩阵；ADR-001 D8/§5.2；ADR-003 §3.3/§3.4/§9；ADR-004 §3.2 的四条句柄问题；
  ADR-006 §5.2/§8/§9；ADR-008；`docs/reports/p3-acceptance-skeleton.md`（L1/L2/L3）
- 前置事实：本机 WinFsp 运行时 **2.1.25156**，**未装 Developer（inc/lib）**；项目 license 为 MIT OR Apache-2.0

---

## 1. 决策摘要

| # | 决策 |
| --- | --- |
| W1 | **适配层只依赖 `fs-api`**：`fs-winfsp` 不得 import `fs-overlay`/`fs-workspace`/`fs-namespace` 内部，不得直接碰 store/overlay；所有语义经 `Repo`/`Workspace`/`ReadView`。 |
| W2 | 适配层拆成两个 crate：**`fs-windows`（无 WinFsp 依赖，纯模型，跨平台可测）** + **`fs-winfsp`（FFI/回调/挂载，cfg(windows)）**。L1 判据全部落在前者。 |
| W3 | **句柄表在 adapter**：handle 存 `(workspace, FileId, desired access, share, delete-on-close, flags)`；**不存路径**。路径只在 Open/Create 时使用。 |
| W4 | **FileId = inode serial，跨 rename 稳定、跨重启对同一 commit 稳定**；WinFsp `IndexNumber` 直接用它。核心不得在 scope 内复用 serial。 |
| W5 | **原子处置**：核心新增"按 disposition 的 open/create"与"带 replace 标志的 rename"，全部在**一个 overlay 事务**内判定；adapter 不做 stat-then-op 的 TOCTOU。 |
| W6 | **共享模式：完整位规则**（desired×share 三比特），在 adapter 的句柄表上判定；不做"最小子集"。 |
| W7 | **删除：两段式**。`SetDelete(TRUE)` 只置 pending；真正删除在最后一次 `Cleanup(FspCleanupDelete)`。`SupportsPosixUnlinkRename=false`，借 FSD 的"最后一次 cleanup 才带 delete"保证。 |
| W8 | **大小写：折叠后映射**（不是拒绝）。精确字节匹配优先；折叠歧义返回 `STATUS_OBJECT_NAME_COLLISION`；枚举仍显示真实名字；不做 Unicode 规范化；8.3 **平台不支持**。 |
| W9 | **默认 cached I/O**（`FileInfoTimeout=u32::MAX`，含数据缓存，mmap 可用），`FlushAndPurgeOnCleanup=1`、`PostCleanupWhenModifiedOnly=1`；提供 `--no-cache` 挂载（无 mmap）。持久性验收点 = **Close 后重开可读**。 |
| W10 | **ACL/安全描述符：读默认 SD，写拒绝**（`PersistentAcls=0`；`SetSecurity` → `STATUS_NOT_SUPPORTED`），文档写明不做权限隔离。 |
| W11 | **范围外项都有明确 NTSTATUS**（§6 表），不得以"莫名失败"形式出现。 |
| W12 | **一挂载一 workspace**（overlay 独占）；多挂载 = 多 workspace/多进程；有打开句柄时默认拒绝卸载。 |
| W13 | **通知**：adapter 在自己的变更后发 `FspFileSystemNotify`；不做跨 workspace/底层目录的外部更改监听（不可达）。 |
| W14 | **byte-range locks 依赖 WinFsp 内核实现**（2.1 无用户态 LockControl 回调）；adapter 不实现、不干预，L2 用 `LockFileEx` 双进程验证。 |

**两处需要 owner 决策（阻塞 P3-2，不阻塞 P3-1）**：

1. **metadata 扩展**：时间戳（access/change）、属性字、默认 SD 若要与 commit/snapshot 同行，需要 inode record 的 **profile v3**（ADR-005 修订，旧根拒绝）。替代方案（adapter 侧旁路 DB）会让元数据在 commit/restore/多挂载间丢失，本 ADR 不推荐。**见 §10.1**。
2. **WinFsp 绑定的 license**：`winfsp` crate 0.13.2 是 **GPL-3.0**，与 MIT OR Apache-2.0 不兼容；MIT 的 `winfsp_wrs` 0.4.1 需能力审计；或自写最小 FFI（WinFsp 的 FLOSS exception 覆盖链接）。**见 §10.2**。

---

## 2. 边界：WinFsp 回调 → `fs-api`

| WinFsp 回调（P0/P1） | adapter 动作 | 核心调用（现有或 §9 缺口） |
| --- | --- | --- |
| `GetVolumeInfo` | 返回固定卷参数、serial、容量/空闲（策略值） | 无（可用 `Store` 元数据生成 serial） |
| `GetSecurityByName` / `GetSecurity` | 名称/句柄 → 默认 SD（或 §10.1 扩展后的 SD） | `stat` / 句柄表 |
| `Open` | 解析路径、共享检查、句柄表登记 | `stat`（拿到 FileId）+ `open`（§9-G1） |
| `Create` | disposition 表（§4.2）、共享检查、句柄登记 | `open_with_disposition`（§9-G1） |
| `Read` | 按句柄的 FileId 读；长度裁剪 | `read_at_id`（§9-G2） |
| `Write` | 按句柄写；维护大小/修改时间 | `write_at_id`（§9-G2） |
| `SetFileSize` | 截断/扩展 | `truncate_id`（§9-G2） |
| `GetFileInfo` | 句柄 → FILE_INFO（大小、时间、属性、IndexNumber） | `stat_id`（§9-G2） |
| `SetBasicInfo` | 时间/属性（受 §10.1 支持面约束） | `set_times` / `set_attributes`（§9-G3） |
| `ReadDirectory` | 目录缓存 + marker 续读 + 过滤 `.`/`..` | `read_dir`（排序保证） |
| `Rename` | `ReplaceIfExists` + 大小写规则 | `rename_with`（§9-G4） |
| `CanDelete` / `SetDelete` | 非空目录/只读检查；置/清 pending | `stat_id`（判空通过 `read_dir`） |
| `Cleanup` | 最后一个句柄时执行删除；刷缓存 | `flush` + `remove_id`（§9-G2） |
| `Flush` | 文件/卷 flush | `flush` |
| `Close` | 释放句柄；撤销锁（WinFsp 内核） | 无 |

adapter **禁止**：解析 CAS 对象、访问 `store.sqlite`/`overlay.sqlite`、在核心之外做"提交/快照"、复制路径规范化规则到核心之外的核心语义。

---

## 3. 语义矩阵（Windows 项 → 归属 / 难点 / 可判定验收）

| # | 语义 | 归属 | 难点 | 验收判据（L1/L2） |
| --- | --- | --- | --- | --- |
| 1 | Create dispositions（SUPERSEDE/OPEN/CREATE/OPEN_IF/OVERWRITE/OVERWRITE_IF） | 核心原子操作 + adapter 表 | 目录/文件 × 存在性；截断时机 | L1 表驱动 6×3 全格；L2 `CreateFile` 实测 |
| 2 | Read/Write 偏移与越界 | 核心 | 越过 EOF 写扩展、读短读 | L1 与 P2 truncate 同族用例；L2 随机 IO |
| 3 | SetFileSize（截断/扩展） | 核心 | 缩容后扩展零化（已由 KI-9 钉住） | L1 缩→扩逐字节；L2 |
| 4 | Get/Set file info（时间、属性、大小） | 核心元数据 + adapter 映射 | FILETIME 100ns/1601；只读属性 | L1 转换边界（0/极大/负）；L2 `GetFileInformationByHandle` |
| 5 | 枚举（`FindFirst/Next`、marker、`.`/`..`） | adapter（缓存/游标） | 变更后游标语义、缓冲区不足 | L1 marker 续读、重命名中目录；L2 `dir /s` |
| 6 | Rename + ReplaceIfExists | 核心原子 + adapter 映射 | 条件替换、case-only、跨目录 | L1 规则表（R3–R7 + case）；L2 `MoveFileEx` |
| 7 | Delete / 两段删除 / cancel | adapter 状态机 + 核心 `remove_id` | 最后 cleanup 语义、new-open 拒绝 | L1 状态机全事件序列；L2 双进程 |
| 8 | Share mode | adapter 句柄表 | 位规则 × 现有句柄；与 delete 交互 | L1 3 desired × 3 share 全组合；L2 双进程 |
| 9 | Delete-pending 可见性 | adapter | 新 open → `STATUS_DELETE_PENDING`；枚举显示与否 | L1 状态断言；L2 |
| 10 | 句柄跨 rename / unlink | 核心 by-id + adapter FileId | rename 后旧 handle 有效 | L1 open→rename→read/write；L2 |
| 11 | FileId(IndexNumber) 稳定 | 核心 | 跨重启、同 commit 同 serial | L1 重启夹具；L2 记录/比对 |
| 12 | 时间戳（4 种） | 核心（§10.1）/ adapter 拒绝 | FILETIME 精度、访问时间不更新 | L1 精确到 100ns 的往返；L2 |
| 13 | 属性（READONLY/HIDDEN…） | 核心（§10.1） | 只读强制、隐藏不改变可见性 | L1 属性位往返；L2 只读删除被拒 |
| 14 | 安全描述符 | adapter（默认 SD） | 不做 ACL 时的默认权限 | L1 SD 序列化/长度；L2 不同用户挂载 |
| 15 | 卷信息/空闲空间 | adapter | 工作区语义"无限" | L1 值稳定、无溢出；L2 `GetDiskFreeSpaceEx` |
| 16 | 变更通知 | adapter（`FspFileSystemNotify`） | 只在 adapter 写入时发 | L2 `ReadDirectoryChangesW` 收到 |
| 17 | cached I/O / mmap / flush | adapter 策略 | 缓存与锁/删除的一致性 | L1 策略决策；L2 mmap 读写、Close→重开 |
| 18 | byte-range locks | WinFsp 内核 | 不持久、与缓存交互 | L2 `LockFileEx` 冲突 |
| 19 | 挂载/卸载/多挂载/崩溃 | adapter/CLI | 有句柄时拒绝卸载、强制卸载恢复 | L3 六项 |
| 20 | 错误映射 | adapter 纯函数 | 类型化错误穷举 | L1 `match` 无 `_` 兜底（新增错误编译失败） |

---

## 4. 大小写、规范化与名称

### 4.1 规则（W8）

1. 核心名是**原始字节、区分大小写**（ADR-001 D8）。adapter 负责 Windows 的不敏感查找。
2. **折叠算法**：每目录维护 `fold(name)` 索引（缓存，可由 `read_dir` 重建）：
   - 索引键：`LCMapStringEx(LOCALE_NAME_INVARIANT, LCMAP_UPPERCASE, ...)` 的结果（1:1 近似 NTFS upcase）；
   - 最终判定：候选集合内用 `CompareStringOrdinal(a, b, TRUE)` **再确认**，避免折叠展开（如 ß）造成假匹配；
   - 平台实现放 `fs-windows` 的 `CaseFolder` trait；L1 用 ASCII/invariant 实现，Windows 实现另测。
3. **查找顺序**：精确字节命中优先；否则折叠候选：
   - 1 个候选 → 使用它（保留其真实大小写）；
   - >1 个候选（核心存在 `A` 与 `a`）→ `STATUS_OBJECT_NAME_COLLISION`，**不猜**。
4. **枚举**：返回核心里的真实名字；**不合并**大小写不同的两个条目。上层可能看到"两个只差大小写的名字"，但任何折叠查找都是确定的拒绝，不产生"两个都能打开"的不确定状态（ADR-001 §5.2 的要求）。
5. **case-only rename**：
   - 目标折叠名不存在 → 调核心 rename（真实改名，保留新拼写）；
   - 目标是同一文件（逐字节相同）→ 成功 no-op；
   - 目标折叠名存在但字节不同（另一文件）→ `STATUS_OBJECT_NAME_COLLISION`，**绝不**用替换语义覆盖（否则会丢另一个文件）。
6. **Unicode 规范化：不做**。与 NTFS 一致，NFC/NFD 是不同名字；文档写明。
7. **尾点/尾空格**：最后一段以 `.` 或 ` ` 结尾 → `STATUS_OBJECT_NAME_INVALID`（不裁剪）。理由：Win32 常规路径本就不能稳定创建这类名字；静默裁剪会造出别名。
8. **非法字符**：`\ / : * ? " < > |`、NUL、控制字符（<0x20）→ `STATUS_OBJECT_NAME_INVALID`；`:` 同时覆盖 ADS（见 §6）。
9. **保留设备名**：`CON`、`PRN`、`AUX`、`NUL`、`COM1-9`、`LPT1-9`（含带扩展名形式）→ `STATUS_OBJECT_NAME_INVALID`（防御性；通常 OS 会先拦截）。
10. **8.3 短名：不支持**。WinFsp 本身不支持短名（官方 NTFS 兼容性文档明列）；`FindFirstFile` 的短名字段为空；依赖短名的工具在 P3 不支持范围内。**不为它造适配**。
11. **长度**：`min(255 UTF-16 code units, 255 UTF-8 bytes)`（核心 `name_max=255` 按字节）。超限 → `STATUS_OBJECT_NAME_INVALID`；非 ASCII 名会比 NTFS 更短，文档写明。
12. 路径解析：WinFsp 给的是卷内相对路径；拒绝 `.`/`..` 段（Win32 已在到达 FS 前规范化，防御性保留）；拒绝空段/重复分隔符；`\\?\` 前缀由 OS 处理，若出现则剥离。

### 4.2 Create disposition 表（NT → 核心动作 / 状态）

| disposition \ 存在 | 不存在 | 文件 | 目录（请求文件/目录） |
| --- | --- | --- | --- |
| SUPERSEDE | 创建 | 截断为 0（保留 FileId） | `STATUS_FILE_IS_A_DIRECTORY` |
| OPEN | `OBJECT_NAME_NOT_FOUND` | 打开 | 打开 |
| CREATE | 创建 | `OBJECT_NAME_COLLISION` | `OBJECT_NAME_COLLISION` |
| OPEN_IF | 创建 | 打开 | 打开 |
| OVERWRITE | `OBJECT_NAME_NOT_FOUND` | 截断为 0 | `STATUS_FILE_IS_A_DIRECTORY` |
| OVERWRITE_IF | 创建 | 截断为 0 | `STATUS_FILE_IS_A_DIRECTORY` |

目录请求（`FILE_DIRECTORY_FILE`）与上表交叉：对"文件"目标一律 `STATUS_NOT_A_DIRECTORY`；对"目录"目标的覆盖类 disposition 一律 `STATUS_FILE_IS_A_DIRECTORY`。**整表由 L1 穷举，不得缺格**。

---

## 5. 句柄、FileId、共享与删除

### 5.1 句柄（W3/W4）

- `Handle { id: u64, file: FileId, desired: DesiredAccess, share: ShareMode, delete_on_close: bool, ... }`；路径不存。
- 所有读写/截断/查询走 **FileId**（§9-G2），因此 rename 后旧句柄继续有效；unlink 由 W7 推迟到最后句柄。
- `IndexNumber` = `FileId.serial()`。核心不得在 scope 内复用 serial（已有 `serial_next` 单调预留；在 §9-G2 的 `open` 返回值里显式带出）。
- FileId **不跨 commit 变化**：同一 commit 的同一 inode 在重启后 serial 相同；`restore`/`rebase` 会改变 workspace 的根，挂载期间不允许（CLI 拒绝）。

### 5.2 Share mode（W6）

对同一 FileId 的每个现有句柄 H 与新请求 N：

```text
conflict ⇔ (H.desired & N.share) != 0 || (N.desired & H.share) != 0
```

任一项冲突 → `STATUS_SHARING_VIOLATION`。`DELETE` 访问在 desired/share 中各占一位；`FILE_SHARE_DELETE` 参与同一规则。目录打开同理（`FILE_LIST_DIRECTORY` 视为读）。**L1 穷举 3×3 位组合 × 已有句柄集合**，缺格即红。

### 5.3 删除两段式（W7）

状态机（每 FileId）：`Present → DeletePending(by) → (cancel) Present | (last cleanup) Deleted`。

- `CanDelete`：目录非空 → `STATUS_DIRECTORY_NOT_EMPTY`；只读属性 → `STATUS_ACCESS_DENIED`；否则成功（不删）。
- `SetDelete(TRUE)`：需要 `DELETE` 访问与 `FILE_SHARE_DELETE` 兼容；置 `DeletePending`；**不调用核心 remove**。
- 新 `Open/Create` 命中 pending → `STATUS_DELETE_PENDING`（无论 share）。
- `SetDelete(FALSE)`：允许清 pending（同一 FileId，持 DELETE 访问即可；文档写明）。
- `Cleanup`：若本 handle 是最后一个且带 `FspCleanupDelete` → `remove_id`（目录空检查在内核保证之外，adapter 再检查一次）；否则只释放句柄。
- `Close`：释放句柄资源；不得在 Close 里做删除（Cleanup 已决定）。
- 目录删除：`CanDelete` 判空；枚举/删除竞争由 adapter 的句柄互斥 + 核心原子操作收口。

---

## 6. 范围外清单与拒绝语义（W11）

| 项 | 决定 | 拒绝语义（NTSTATUS） |
| --- | --- | --- |
| mmap / section | **支持，仅 cached 模式**；`--no-cache` 挂载下不支持 | `--no-cache` 下 `STATUS_NOT_SUPPORTED`（`CreateFileMapping` 失败） |
| byte-range locks | **WinFsp 内核提供**，adapter 不实现 | 若 FSD 转发（2.1 未见回调），后续卡补；L2 以 `LockFileEx` 验证 |
| symlink / reparse point | 不支持（不声明 `reparse_points`） | `STATUS_NOT_SUPPORTED`（创建/读取 reparse 一律） |
| ADS / named streams | 不支持（不声明 `named_streams`）；名中含 `:` 即非法 | `STATUS_OBJECT_NAME_INVALID` |
| 安全描述符写（`SetSecurity`） | 不支持（`PersistentAcls=0`） | `STATUS_NOT_SUPPORTED`；`GetSecurity` 返回默认 SD |
| ACL 强制 | 不做（文档写明以挂载进程 token 为准） | 不适用 |
| hardlink | 平台不支持（不声明） | OS 返回 `ERROR_NOT_SUPPORTED`（卷能力位未置） |
| 8.3 短名 | 平台不支持 | 无短名（字段为空） |
| sparse / compressed / encrypted | 不支持（不声明对应卷能力） | `STATUS_INVALID_DEVICE_REQUEST`（FSCTL 未实现） |
| 配额 / 变更日志 / USN / 重解析索引 | 不支持 | `STATUS_NOT_SUPPORTED` / `STATUS_INVALID_DEVICE_REQUEST` |
| `FILE_OPEN_BY_FILE_ID` | WinFsp 不支持 | `STATUS_NOT_SUPPORTED` |
| 卷标签写 | 不支持 | `STATUS_ACCESS_DENIED` |
| EA（扩展属性） | 不支持 | `STATUS_NOT_SUPPORTED` |
| oplock | 不启用 | 不适用（性能影响记录） |
| paging file | 不支持 | 不适用（OS 拒绝） |
| 大小写敏感目录（`SL_CASE_SENSITIVE`） | 平台不支持 | 不适用 |

---

## 7. 驱动无关层（L1）：`fs-windows`，无 WinFsp 依赖

| 模块 | 内容 | 判据（缺一即红） |
| --- | --- | --- |
| `path` | UTF-16→UTF-8、非法字符、尾点/空格、保留名、长度、`.`/`..` | 反例逐条拒绝；合法长路径通过 |
| `case` | `CaseFolder` trait + `CaseIndex`；精确优先/折叠歧义/冲突分类；case-only rename 分类 | 3 类结果表；折叠歧义必为 `COLLISION` |
| `share` | desired×share 位规则；`conflict()` | 3×3×已有句柄集合穷举 |
| `delete` | `DeleteState` 状态机 | open/set/cancel/cleanup 事件序列全表 |
| `disposition` | §4.2 表 + 目录交叉 | 6×3 全格 |
| `time` | FILETIME↔Unix ns（100ns、1601、无符号范围、负 ns） | 边界值往返；越界拒绝 |
| `error` | `ApiError`/`WorkspaceError`→NTSTATUS **穷举 match（禁止 `_`）** | 新增错误变体编译失败；表驱动每变体非零 |
| `dirent` | 枚举模型、marker 编解码、`.`/`..` 过滤、缓冲不足 | marker 续读不重不漏；改名中目录 |
| `attrs` | Windows 属性字 ↔ 核心属性（§10.1） | 位往返；只读强制 |
| `volume` | 卷参数/容量/空闲策略 | 稳定值、不溢出 |

这些测试在**任何平台**跑（`fs-winfsp` 的 FFI 部分 `cfg(windows)`），CI 不装驱动即可执行；L2/L3 另行计账，不得充数。

---

## 8. 挂载、缓存与生命周期

- CLI：`fs-winfsp mount --store <dir> --workspace <name> [--no-cache] <盘符或目录>`；一个进程一个挂载、一个 workspace（overlay 独占，二次挂载 → `WorkspaceInUse`）。
- 卷参数：`CaseSensitiveSearch=false`、`CasePreservedNames=true`、`UnicodeOnDisk=true`、`PersistentAcls=false`、`ReparsePoints=false`、`NamedStreams=false`、`ExtendedAttributes=false`、`PostCleanupWhenModifiedOnly=true`、`FlushAndPurgeOnCleanup=true`、`SectorSize=4096`、`SectorsPerAllocationUnit=8`、`MaxComponentLength=255`、`FileSystemName="LayerFS"`。
- 缓存：默认 `FileInfoTimeout=u32::MAX`（含数据缓存；mmap 需要）；`DirInfoTimeout`/`VolumeInfoTimeout` 同值；`--no-cache` 时设 0。
- 持久性承诺（W9）：**Close 后重开可读**。`Flush`/`FlushFileBuffers` 触发 `Workspace::flush`；adapter 在 `Cleanup` 刷一次缓存（`FlushAndPurgeOnCleanup`）。不承诺"每次 Write 即时落盘"——cached 模式下由 OS 缓存决定；`--no-cache` 模式下每次 Write 已是 overlay 事务。
- 通知：adapter 在 Create/Write/SetSize/Rename/Delete 后调用 `FspFileSystemNotify`（目录级）；因为 workspace 独占，不存在"底层目录被外部改"的场景。
- 卸载：默认有打开句柄时拒绝（`FspFileSystemStopServiceIfNecessary` + 句柄表检查）；`--force` 允许强制卸载，未刷缓存的数据按 NTFS 强拆同语义丢失（记录）。
- 崩溃：进程被杀 → OS 关闭 WinFsp 卷；overlay 的每个操作是事务，已 flush 的数据可重开；缓存中未刷数据丢失（文档写明）。

---

## 9. 核心侧缺口（P3 前置，按依赖顺序）

| # | 缺口 | 内容 | 归属 |
| --- | --- | --- | --- |
| G1 | 原子 open/create | `OpenDisposition`（§4.2）+ 目录/文件 + 一个 overlay 事务；返回 `{stat, created}` | fs-overlay/fs-api |
| G2 | 按 FileId 的操作 | `stat_id` / `read_at_id` / `write_at_id` / `truncate_id` / `remove_id` / `open_id`（返回 FileId 的 open） | fs-overlay/fs-api |
| G3 | 元数据写 | `set_times`（creation/write，若 §10.1 扩展则 access/change）、`set_attributes`；`stat` 暴露 FileId | fs-namespace（profile v3，若批准） |
| G4 | 条件 rename | `rename(..., replace: bool)` 原子；替换/不替换语义同 ADR-003 R3–R7 | fs-namespace/fs-api |
| G5 | 枚举游标 | 可选 `read_dir_after(path, after)`；否则 adapter 缓存整目录（保持"一次枚举期间一致"） | fs-overlay（可选） |
| G6 | 通知挂钩 | 无需核心；adapter 从自己的调用点发 | — |

G1/G2/G4 是**必须**（否则 adapter 只能 TOCTOU 或无法保证 rename 后句柄）；G3 取决于 §10.1；G5 可选。

---

## 10. 需要 owner 决策的前置

### 10.1 元数据：profile v3（时间/属性/SD 是否随快照走）

- **问题**：core 现只有 `created_ns`/`modified_ns`，无 access/change、属性字、SD。Windows 程序会读/写它们（`SetFileTime`、`SetFileAttributes`、`SetSecurityInfo`）。
- **选项 A（推荐）**：ADR-005 inode record v3 增加 `access_ns`、`change_ns`、`attributes: u32`、`security: bytes(≤64KiB, 自相对 SD)`；旧根拒绝（与 P2 的 v1/v2 拒绝同策）。代价：格式版本 + 实现 + 迁移判据；收益：元数据随 commit/snapshot/多挂载一致。
- **选项 B**：adapter 侧旁路 DB（按 FileId）；**不推荐**：commit/restore/rebuild 后丢失、与其他 workspace 不一致、GC/verify 看不见。
- **选项 C**：全部拒绝写（只读时间/属性/SD）；P3 兼容性差（安装器/编辑器会失败）。
- 若选 A：`attributes` 只强制 `READONLY`；`HIDDEN/SYSTEM/ARCHIVE` 存储但只报告；SD 仅存取不强制（与 W10 一致）；`access/change` 由 adapter 在读写/元数据变更时更新或按策略保留（写明）。

### 10.2 WinFsp 绑定与 license

- `winfsp` crate **0.13.2+winfsp-2.1**：API 完整、维护活跃，但 **GPL-3.0**；项目是 MIT OR Apache-2.0，直接依赖会使分发整体受 GPL 约束。
- `winfsp_wrs` **0.4.1**：MIT，但功能面（安全、reparse、通知、异步）需审计，维护活跃度低。
- **自写最小 FFI**（`winfsp-sys` 风格，基于 inc/winfsp.h，仅实现 P0 回调）：WinFsp 的 FLOSS exception 允许 FLOSS 链接平台 DLL；工作量约 1–2 天 + unsafe 审查，可控。
- **建议**：先做 **P3-0 spike**（1 天）：用 MIT `winfsp_wrs` 跑通 `memfs` 风格最小挂载；若不满足（安全/通知/锁），转自写 FFI；**不使用 GPL crate**，除非 owner 明确同意将项目改为 GPL 兼容分发。

---

## 11. 交付顺序与卡

1. **P3-1（L1，无驱动）**：`fs-windows` 十个模块 + 穷举测试 + API 错误映射；CI 跨平台。产出可独立验收，不依赖 SDK。
2. **P3-0（spike，1 天，需 SDK）**：绑定评估 + 最小挂载（Open/Read/Write/GetInfo/Enumerate/Close）跑通；决定 §10.2。
3. **P3-2（核心缺口）**：G1/G2/G4（+G3 若 owner 批准）；每条带 L1 规则测试。
4. **P3-3（正式 adapter）**：完整回调、共享/删除/大小写/枚举/通知/缓存/挂载生命周期；winfsp-tests + L2 六项。
5. **P3-4（端到端与基线）**：SQLite/git/npm/Python/Unity；NTFS 对照（延迟 P50/P95/P99、CPU、RAM、写放大、空间节省）。

---

## 12. 前置条件清单（请 owner/qclaw 处理）

| 项 | 具体 | 校验 |
| --- | --- | --- |
| WinFsp SDK | 安装 **WinFsp 2.1（本机运行时 2.1.25156）**，MSI 选 **Developer** 组件（或 `msiexec /i winfsp-2.1.25156.msi ADDLOCAL=ALL`）；需要管理员 | `%ProgramFiles(x86)%\WinFsp\inc\winfsp\winfsp.h`、`lib\winfsp-x64.lib` 存在；`sc query winfsp` 为 RUNNING |
| 版本匹配 | 编译/运行都用 2.1.x；升级需同时升级运行时与头文件 | `winfsp-x64.dll` 版本 = 2.1.25156（或同 minor） |
| 工具链 | Rust ≥1.85，`x86_64-pc-windows-msvc`；VS 2022 Build Tools + Windows SDK（已具备） | `cargo build` 能链接 `.lib` |
| license 决策 | §10.2（GPL crate / MIT crate / 自写 FFI） | owner 书面选择 |
| CI | L1 任意 OS；L2/L3 需 Windows VM（管理员、可装驱动、可挂盘符/目录） | 一条 L2 smoke 挂载通过 |
| 测试资产 | WinFsp 官方 `winfsp-tests`（同 2.1 版本）+ 自建双进程 share/lock 用例 | 可执行 |
| 卸载/重挂 | 测试机允许创建/删除盘符；管理员权限 | L3 六项 |

---

## 13. 风险与未决

1. **cached I/O × 锁/删除**：WinFsp 缓存可能让其他句柄的读绕过 FS 层锁检查（Windows 自身语义 vs 用户态 FS）。L2 必测"锁持有期间另一句柄写/读"；若不一致，提供 `--no-cache` 给数据库类 workload，并记录为**平台限制**而非 bug。
2. **mmap 与私有写**：cached 模式下 `CreateFileMapping` 的写回时机由 OS 决定；验收用"Close 后重开"，不用"write 后立即读另一进程"。
3. **metadata profile v3**：若 owner 选 C（拒绝写），P3 兼容性下降；ADR 与卡必须写明拒绝的 NTSTATUS，避免"莫名失败"。
4. **name 长度**：核心 255 字节 vs Windows 255 UTF-16；CJK 名字受限，需在兼容性声明中列出。
5. **WinFsp 版本漂移**：2.1 无用户态 LockControl；若升级改变回调面，锁/通知行为需重测。
6. **性能**：单 workspace 串行 overlay 事务 + 每写跨进程；NTFS 对照必须分"冷/热缓存、direct/cached"报告，禁止混比。

---

## 14. 与 ADR-001 D8 的一致性声明

本 ADR 不改变核心语义：核心仍逐字节区分大小写、不做规范化。Windows 的大小写不敏感、路径别名、8.3 全部在 `fs-windows`/`fs-winfsp` 层处理；折叠歧义按 §4.1 拒绝而非猜测。ADR-004 §3.2 的四条句柄问题（rename 后身份、unlink 后句柄、生命周期、锁/share 交互）在此逐条落地：W3/W4/W5/W7/W6。
