# Workspace CAS 文件系统：开发需求与实施规格（v0.1）

状态：供开发 Agent 执行的初始工程规格；2026-10-22  
目标平台：核心 Rust 跨平台（Windows 优先验证）；第三阶段接入 WinFsp，保留 Linux FUSE 适配可能。  
项目定位：通用内容寻址文件系统引擎，而非 Agent 插件、SVN 客户端或 Unity 专属工具。

## 0. 给开发 Agent 的执行指令

1. 严格按阶段实现，不跨阶段提前引入 WinFsp、SVN、Unity、网络服务、分布式一致性或 OS 启动盘支持。
2. 第一阶段必须产出可编译、可测试的 Rust workspace、SDK 和 CLI；不得仅提交架构空壳。
3. 所有设计决策应写入 `docs/adr/`，尤其是对象格式、身份、事务、目录语义、错误语义和兼容性。
4. 不允许把内容相同与文件身份相同混为一谈；不可变 CAS 与可变文件状态必须隔离。
5. 不允许未经测试声称“原子”、“持久化”、“零拷贝”、“完全 POSIX/NTFS 兼容”或“实时去重”。
6. 遇到影响持久化格式的歧义，先编写 ADR 和兼容测试，再编码；不自行假定后续驱动行为。
7. 每个阶段提交可运行示例、自动化测试、性能记录、已知限制与下一阶段接口检查。

## 1. 背景与目标

同一机器上的多个大型工作目录（如 Unity、编译产物、SDK、资源缓存、SVN 工作副本）含大量相同或相似文件，完整物理复制浪费空间。目标是以一个全局共享 CAS 存储内容，令多个逻辑上独立的文件系统视图共享相同的不可变数据，并在写入时通过 CoW 隔离变更。

最终目标：

- SDK/CLI 可以创建目录、文件、随机读写、删除、重命名、列目录、查询元数据。
- 相同规范内容的对象全局去重；大文件支持子文件级去重。
- 多 Workspace 共享 Base Snapshot，每个 Workspace 有独立可变 Overlay。
- Snapshot/Commit 产生稳定可读取的 Root，旧版本不被后续修改破坏。
- 可维护多个逻辑视图，支持切换/恢复到历史 Root。
- 后台封存稳定脏数据，允许不产生用户版本 Commit 就进行物理去重。
- 最终通过 WinFsp 挂载为 Windows 盘符/目录，使普通进程透明读写。

### 非目标（前三阶段都不应自行扩展）

- 不实现 SVN/Git 协议、Agent 插件、Unity 编辑器插件或远程同步。
- 不替换 Windows 启动卷、分页文件、内核文件系统；不承诺系统级透明去重现有所有磁盘。
- 不实现分布式 CAS、多机协商或跨用户安全域的透明共享。
- 不保证相同逻辑字节经不同增量编辑路径必定产生同一个文件 Root；仅相同规范 ObjectId 必须去重。
- 不承诺压缩后内容的去重率，也不对加密/高熵数据作收益保证。

## 2. 分阶段范围与交付

### Phase 1 — 持久 CAS + 不可变文件树 + 常规文件 API

交付：一个不依赖驱动的 Rust SDK、CLI、稳定且有版本号的本地 Store 格式。

P1 必须完成：

1. 全局对象命名空间：ObjectId = BLAKE3(domain_separator || canonical_bytes)，含对象类型和格式版本；读取验证对象身份。
2. `put/get/exists`、幂等插入、原子对象发布；相同对象不能产生重复物理副本（忽略合法的临时/垃圾状态）。
3. 文件系统 Namespace：不可变 `FilesystemRoot`、目录条目、稳定逻辑 FileId/InodeId、文件内容 Root、大小、基本时间戳与类型。
4. 整文件 Blob 的创建与读取；大文件的初版可用固定块/Extent Index，但必须支持有界 `read_at`，不可为随机读取加载整个文件。
5. 路径 API：`mkdir`, `create`, `open`, `read_at`, `write_at`, `truncate`, `rename`, `unlink`, `read_dir`, `stat`。P1 可将每次 SDK 写操作视为内部事务，尚不公开多 Workspace Overlay。
6. 一个事务发布新 Namespace Root；任何失败不能暴露仅包含部分更新的 Root。
7. Store 重启恢复、数据校验、schema/format 版本拒绝、只读打开、错误类型。
8. CLI：`init`, `put`, `get`, `ls`, `stat`, `mkdir`, `rm`, `mv`, `verify`, `stats`；CLI 和 SDK 使用同一核心实现。

P1 可延后：CDC、Delta 编码、Pack 压缩、Garbage Collection、Windows 属性、文件锁、WinFsp。P1 的对象格式必须保留未来扩展空间；不能宣称这些特性已经存在。

### Phase 2 — Mutable Overlay + Workspace + Snapshot

交付：多个可写 Workspace、随机写脏页、后台封存和可靠的 Root Snapshot。

P2 必须完成：

1. `workspace_create(base_root)`, `workspace_open`, `workspace_snapshot`, `workspace_discard`, `workspace_status`, `workspace_close`。
2. `WorkspaceView = BaseRoot ⊕ Overlay`：Overlay 负责创建、删除白名单/墓碑、重命名、文件大小、元数据、数据覆盖区间。
3. 按固定页面（建议 4 KiB，可配置）跟踪脏区间，正确处理重叠/乱序写、EOF、truncate、稀疏零区间；多个 Workspace 不得共享可变页。
4. 前台 `read_at` 合成最新脏数据和不可变 Base；可在文件活跃写入时读取一致的已定义状态。
5. Seal/Compact：对稳定数据生成 CAS Chunk/Extent；可在不创建业务 Snapshot 的情况下进行物理去重。CAS 不可原地修改。
6. 引入内容定义分块（参考 GEAR/FastCDC，建议初始 8/16/32 KiB），分块参数固定在 Store Policy 中，严格版本管理。
7. 大文件 `FileState -> Extent Tree -> Chunk`；支持局部读取、旧 Chunk 和 Chunk 切片复用，避免小编辑全文件读写。
8. Snapshot 创建新不可变 Root，不修改旧 Root；Root 发布需原子且经过依赖完整性验证。
9. Branch/Commit 可作为上层引用目录：`branch_create`, `branch_head`, `commit`, `reset/restore`；明确 `revert`（生成逆向提交）与 `reset`（移动引用）的区别。
10. Durable Overlay：`flush/fsync` 与后台 Seal 分离；崩溃后恢复已确认持久化状态，不把仅在内存中的数据冒充持久化数据。
11. 引用可达性和垃圾回收的最小正确实现（可以离线 GC），不得回收被 Root、活跃 Workspace 或未完成事务引用的数据。

P2 可延后：在线 Pack 重整、跨进程实时 Merge、复杂 ACL、全自动冷热数据策略。优先正确性，再优化去重率。

### Phase 3 — WinFsp 前端与真实工作负载

交付：可挂载的 Windows 用户态文件系统、安装/卸载工具、兼容性测试和性能基线。

P3 必须完成：

1. WinFsp 适配独立于核心：Open/Create、Read/Write、GetInfo、SetSize、Enumerate、Rename、Delete、Flush、Close、目录操作。
2. Windows 专属语义：路径大小写与规范化、share mode、delete-pending、ACL/安全描述符、时间戳、句柄生命周期、文件锁、hardlink/symlink 的支持范围。
3. 明确 cached I/O、memory-mapped I/O、文件通知以及 WinFsp/NT Cache Manager 的交互策略；不允许以绕过此类测试作为“兼容”。
4. 挂载多个 Workspace 到不同盘符/目录，进程间隔离；异常退出、强制卸载、重新挂载和恢复。
5. 使用 Python、Node/npm、SQLite、编译工具及 Unity 测试项目进行端到端验证。
6. 与原生 NTFS 文件系统做相同 workload 的性能对照，并同时报告物理空间节省、CPU、RAM、P50/P95/P99 延迟及写放大。

可选的后续阶段：Linux FUSE 前端、对象 Pack 优化、跨应用去重策略、OS 镜像/块设备前端；不得作为 P3 必选项。

## 3. 分层架构

```text
  Rust SDK / CLI                 WinFsp Adapter (P3)
         \                         /
          \--- Virtual FS API -----/
                    |
              Namespace Engine
                    |
           Workspace Overlay (P2)
                    |
         Immutable Content Engine
           |             |
      File/Extent      Object CAS
           \             /
             Persistent Store
```

原则：WinFsp Adapter 不拥有去重算法、不直接修改 CAS 对象、不自行管理 Commit；CAS 不知道 Workspace、Branch 或 Windows 句柄。

### 逻辑数据模型（概念，不固定磁盘编码）

```rust
struct ObjectId([u8; 32]);
struct FileId(u64);  // 实际身份须带 namespace/scope 防碰撞
struct FilesystemRoot { format: FormatId, root_inode: FileId, inode_index_root: ObjectId }
struct FileRecord { id: FileId, kind: FileKind, size: u64, content_root: Option<ObjectId>, metadata: Metadata }
struct Extent { logical_start: u64, length: u64, source: ExtentSource }
enum ExtentSource { Chunk { id: ObjectId, source_offset: u32 }, Zero }
struct Workspace { id: WorkspaceId, base_root: ObjectId, overlay_state: OverlayId }
```

ObjectId 表示规范内容身份，FileId 表示逻辑文件身份；两个不同 FileId 可引用同一 content_root。目录绑定与 inode 内容更新可分离，以避免修改深层文件时重建整个父目录链。P1 可采用简单持久树，若改为 inode-table 索引，应先证明路径查找、硬链接和快照语义。

### API 语义（对外草案）

```rust
trait VirtualFileSystem {
    fn open(&self, path: &Path, opts: OpenOptions) -> Result<Handle>;
    fn read_at(&self, h: &Handle, offset: u64, out: &mut [u8]) -> Result<usize>;
    fn write_at(&self, h: &Handle, offset: u64, bytes: &[u8]) -> Result<usize>;
    fn truncate(&self, h: &Handle, size: u64) -> Result<()>;
    fn flush(&self, h: &Handle) -> Result<()>;
    fn stat(&self, path: &Path) -> Result<FileMetadata>;
    fn read_dir(&self, path: &Path) -> Result<Vec<DirectoryEntry>>;
    fn mkdir(&self, path: &Path) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn unlink(&self, path: &Path) -> Result<()>;
}
```

该代码仅定义所需操作形状，不是最终可直接编译的 Trait（Handle 所有权、异步与错误模型等通过 ADR 决定）。API 必须给出部分读写、短写、句柄关闭后的行为、并发写顺序和 `flush` 保证；不许由具体后端隐式决定。

## 4. 存储与事务要求

- 对象规范字节以 `domain || version || role || payload` 形成唯一身份；跨角色不能因字节巧合导致身份混淆。
- CAS 插入先完成完整数据写入和校验，后发布索引；发布 Root 前验证所有可达依赖已持久化或满足明确事务协议。
- 将对象数据与元数据索引持久化；P1 可采用 SQLite 元数据 + 独立对象文件。后续 Pack 必须提供格式升级及迁移/拒绝策略。
- Branch Head 更新使用 compare-and-swap / 带预期版本的原子事务；并发冲突返回明确错误，不静默覆盖。
- 脏 Overlay 写入日志必须可恢复；Seal 使用生成新对象 → 持久化 → 原子映射切换 → 延迟回收的顺序。
- GC 对快照、当前 Head、开放句柄、活跃 Workspace、正在 Seal 的对象保持正确的可达性保护。
- 每次读对象校验身份或依赖具有可信验证链和明确校验策略；坏对象返回数据损坏错误，不静默填零。
- 要有可重现的故障注入点：对象落盘前后、索引提交前后、Root 发布前后、Overlay flush/seal 前后、GC 标记/回收之间。

## 5. 性能策略

1. 热写路径先使用固定大小脏页，不在每次小写入时做完整 CDC。
2. 稳定内容 Seal 后进入可变长 Chunk CAS；后台工作必须有 CPU、RAM、I/O 预算和可取消机制。
3. 读取路径先查询脏页，再查询已封存 Extent / Base；支持有界区间读取和预读。
4. Chunk 索引持久化，内存只保留热点；不把全量对象索引常驻内存当作正确性前提。
5. 大对象按范围处理，不能仅因写入文件大小增长而需要完整内存副本。
6. 指标至少包括：logical_bytes、unique_canonical_bytes、stored_bytes、dirty_bytes、dedup_saved_bytes、CAS hit rate、read amplification、write amplification、seal queue、P50/P95/P99 I/O 延迟。

## 6. 验收测试矩阵

### Phase 1

- 相同内容在 100 个路径引用：只有一个相同规范内容对象，文件身份与元数据仍独立。
- 创建 100,000 个小文件并随机读、重命名、删除；重启后逐项一致。
- 1 GiB 文件随机范围读取，内存用量有界；读出字节与参考文件一致。
- 任意写入前后 Root 可读取；旧 Root 的数据不因新写入改变。
- 不同进程并发写不同路径、相同路径冲突行为符合定义。
- 每个公开操作都测试无效路径、越界范围、短读、权限与 I/O 错误。

### Phase 2

- 10 个 Workspace 同 Base，各自乱序随机写、插入、truncate、删除互不影响。
- 修改 1 GiB 文件中少量字节，新增物理内容远少于 1 GiB（测试给出实际预算而非空泛断言）。
- 未提交/未 Snapshot 的后台 Seal 不改变可见字节或生成业务 Commit。
- 重启后所有已确认 flush 的数据仍可读；故障注入覆盖封存、Root 切换、GC。
- 对照参考内存模型开展 property-based / 随机序列测试，逐字节核对文件状态。
- 对重复内容的两份不同 Workspace，待后台封存完成后验证实际物理去重，而非只统计逻辑哈希。

### Phase 3

- WinFsp 同一文件的多句柄读写、共享冲突、删除中句柄、重命名、truncate、mmap/缓存读写等测试。
- SQLite WAL 模式的事务与崩溃恢复测试；不能以“程序能启动”代替 ACID 验证。
- Python、npm、构建工具大量小文件/随机 I/O 压力测试。
- Unity 测试项目在独立 Workspace 中导入资源、修改资源、重启 Editor、重复导入，项目状态正确。
- 多挂载同时工作，异常退出后不会让其他 Workspace 丢失已提交内容。

## 7. 交付工程结构建议

```text
crates/
  cas-core/          # ObjectId、canonical codec、哈希、只读对象
  cas-store/         # 持久化、索引、事务、校验
  fs-content/        # Blob/Chunk/Extent 与读取
  fs-namespace/      # 目录、inode、Root
  fs-overlay/        # P2：脏页、可变操作、Seal
  fs-workspace/      # P2：Snapshot/Branch/Commit
  fs-api/            # 对外稳定语义
  fs-winfsp/         # P3：Windows 适配
  fs-cli/            # 命令行
docs/adr/
tests/
benchmarks/
```

可根据依赖图合并 crate，但保持模块边界；禁止循环依赖。先设计对象编码版本和恢复协议，再大规模编码。

## 8. 各阶段 Agent 提交格式

每一阶段的 PR / 提交说明至少包含：

1. 实现范围与明确未实现列表。
2. 公共 API 示例，含完整运行命令。
3. 对象格式或事务格式变化及兼容策略。
4. 自动化测试名称、执行结果、故障注入结果。
5. 正确性风险与已知限制。
6. 性能对照的数据集、机器环境、原始结果和重复次数；不以单次最佳值宣称提升。
7. 后续 Agent 接手说明、下一个待完成的最小任务。

## 9. 推荐开发任务拆分

先依赖顺序实施，非无关模块不要并行写入相同文件：

1. ADR-001：规范对象编码、ObjectId 和文件身份。
2. CAS Store 的 put/get/verify、重启恢复及故障测试。
3. Directory/Inode/File Root 与目录查找。
4. SDK/CLI 的首批文件操作及事务 Root 更新。
5. P1 大目录与随机 I/O 基准、性能报告。
6. ADR-002：Overlay 页状态、读写可见性和 flush 合约。
7. 脏页 Overlay + 合成读取 + 多 Workspace。
8. CDC/Extent Builder + Seal + Snapshot。
9. Overlay 恢复、GC、故障注入和 P2 性能报告。
10. ADR-003：Windows 操作语义、WinFsp Adapter 合约。
11. WinFsp 挂载 + Windows 兼容测试 + Unity 工作负载。

## 10. 参考材料与借鉴边界

- LayerFS（思路参考，并非要求直接移植）：https://github.com/Ephemeral-AI-Lab/layerfs
- LayerFS 内容对象：https://github.com/Ephemeral-AI-Lab/layerfs/tree/main/core/crates/layerfs-content
- LayerFS Root 实现：https://github.com/Ephemeral-AI-Lab/layerfs/blob/main/core/crates/layerfs-content/src/filesystem/root.rs
- LayerFS 文件映射：https://github.com/Ephemeral-AI-Lab/layerfs/tree/main/core/crates/layerfs-content/src/file/mapping
- WinFsp：https://winfsp.dev/doc/
- Windows Data Deduplication：https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand
- OpenZFS Dedup：https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Data%20Storage/Deduplication.html

借鉴思想和算法时必须独立检查许可、版权、现行实现与适用平台；本规格不授权把上游仓库代码或协议直接复制为本项目实现。

## 11. 需要负责人确认的开放决策

在第一阶段的 ADR 中明确，不阻塞基础原型：

- Store 后端优先 SQLite + Object Files，还是 Pack 从第一天实现？（建议前者。）
- P1 是否仅保证单机多进程、一个本地 Store？（建议是。）
- 目录的路径大小写语义使用区分大小写的规范核心，Windows 前端再做映射，还是核心统一不区分大小写？（建议前者，但必须处理冲突。）
- P1 的 `write_at` 是每调用一事务，还是显式 `begin/commit`？（建议先简单事务，再由 P2 引入长期 Workspace。）
- Snapshot / Branch 的保留政策与 GC Root 集合由哪个模块管理？
- 物理去重安全域是否限于同一可信本地用户/Store？（建议第一版限于同一可信域。）

---

**第一阶段完成定义（DoD）：** 在不安装文件系统驱动的机器上，SDK 和 CLI 能持久管理真实大小的虚拟文件树，完成随机范围读写、全局相同对象去重、原子 Root 发布与崩溃恢复测试，并有可复现的性能报告。只有达到此条件才进入第二阶段。
