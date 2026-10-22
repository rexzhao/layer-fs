# ADR-002：Store 物理布局、发布顺序与恢复协议

- 状态：已接受（Accepted），适用于 P1 / Store 格式版本 1
- 日期：2026-10-22
- 决策者：manager（任务卡 T2；规格 `workspace_cas_fs_spec.md` §1 P1-2/3/7、§4、§6；调研 §2.5/§2.6/§3-1/§4.2）
- 实施者：T2（本 ADR 与 `crates/cas-store` 同步提交）
- 相关：[ADR-001 对象编码](ADR-001-object-encoding.md)（对象格式已冻结，本 ADR 不改动它）

本 ADR 记录 **Store 的磁盘布局、元数据 schema、对象发布顺序及其崩溃语义、打开时的恢复协议**。
标注"已实现"的条目有测试覆盖；标注"决策"的条目是 T3/T4 必须遵守但本卡未实现的契约；
"未实现"清单里的特性一律不存在，不得宣称。

---

## 1. 背景与问题

ADR-001 定死了**单个对象**的规范字节与身份，但没有回答：

1. 一个 Store 在磁盘上长什么样（目录、文件、元数据）；
2. 对象的元数据索引存在哪里，如何与对象文件保持一致；
3. `put` 的每一步崩溃后，磁盘上可能处于什么状态，重新打开时如何判定；
4. 什么算"已发布"，未发布的东西对读者是否可见；
5. 崩溃残留、孤儿对象如何处理；
6. schema 变更时如何拒绝而不是静默迁移。

规格 §4 对此提出硬要求：**先写完对象数据并校验，再发布索引**；每次读对象要校验身份，
坏对象报损坏而不是填零；要有可重现的故障注入点。调研 §3-1 特别指出本项目与 LayerFS 的
差异：LayerFS 用 Disposable/WAL/synchronous=OFF（只保证进程崩溃存活），而本项目 spec P1-7
要求**崩溃恢复**，因此必须走 durable 顺序。

## 2. 决策摘要

| # | 决策 | 状态 |
| --- | --- | --- |
| S1 | 布局：`<store>/store.sqlite` + `objects/` + `tmp/` | 已实现 |
| S2 | 对象文件路径 `objects/ab/cd/<oid>`，内容 = ADR-001 的规范帧，**不含任何额外头** | 已实现 |
| S3 | SQLite 元数据：`store_policy`、`object_location`；`PRAGMA user_version` 标 schema 版本 | 已实现 |
| S4 | schema 自校验：建库 DDL 原文入库，每次打开逐字比对 | 已实现 |
| S5 | 引擎 rusqlite 0.40.2 + `bundled`，锁版本，不 patch | 已实现 |
| S6 | 发布顺序：tmp 写入 → fsync 文件 → rename → 刷父目录 → 索引事务 | 已实现 |
| S7 | **索引提交即发布**：读者只认 `object_location` 里的行 | 已实现 |
| S8 | SQLite 用 WAL + `synchronous=FULL`（与磁盘上"已提交即持久"的能力对齐） | 已实现 |
| S9 | 恢复协议：schema 自校验 → 版本拒绝 → tmp 清理 → 孤儿容忍 | 已实现 |
| S10 | 孤儿对象**只读不删**（删除是 GC，P2） | 已实现 |
| S11 | 一次操作、不重试；所有失败类型化 | 已实现 |
| S12 | 只读打开拒绝一切写操作 | 已实现 |
| S13 | 并发对同一对象的 `put` 由 SQLite 单写者事务裁决，结果幂等 | 已实现 |
| S14 | 命名空间/root 表结构 | 决策（T3） |

## 3. 磁盘布局

```text
<store>/
  store.sqlite            # 元数据索引（SQLite，WAL 模式）
  store.sqlite-wal        # SQLite 运行时副产物
  store.sqlite-shm        # SQLite 运行时副产物
  objects/
    ab/
      cd/
        0123…（64 个 hex 字符，文件名即 ObjectId）
  tmp/
    3f2a…（暂存文件，崩溃后由下一次 open 清理）
```

规则：

- `objects/` 下**只有对象文件**，文件名必须是 64 个**小写** hex 字符，且解码后与内容哈希一致
  （见 S7/§6）。任何其它名字的文件视为损坏状态。

  **该规则由 `Store::verify` 强制执行**：它遍历 `objects/` 全树，把两类条目列进
  `VerificationReport::bad_names`，`is_clean()` 因此为 false：

  1. **名字不合法**：长度不是 64、含非 hex 字符，或含大写。大写被单独拒绝，因为
     `ObjectId` 的解析接受大写，而磁盘形式按本节固定为小写——否则在大小写不敏感的文件
     系统上同一个对象会有两种拼写。
  2. **位置不合法**：名字虽是合法 id，但不在它自己的两级前缀下
     （`objects/<id[0..2]>/<id[2..4]>/<id>`）。这类文件与"名字不合法"同样危险：查找永远
     找不到它，将来的 GC 也永远扫不到它，只会静默占空间。

  正确命名且**位置正确**但索引无行的孤儿**不入此列**（§10：孤儿合法，P1 只保留不删）。

  > 第 1 条由第 1 轮覆盖率分析（A1）补齐：`StoreError::InvalidObjectName` 当初是死变体，
  > ADR 承诺了扫描而代码从不扫描 `objects/`。这里选择**实现**而非删除承诺。
  >
  > 第 2 条由同轮复验（cov-F1）补齐：初版实现只校验文件名、不校验位置，且我在提交说明里
  > 错误地声称"放错位置的合法名文件也会被报出"——实测平铺与错误前缀目录下的合法名文件
  > 都被静默放过。现已改为校验完整相对路径。
- 二级前缀取 ObjectId 的前 2 与次 2 个 hex 字符（`256×256` 个目录），用于避免单目录
  十万级条目（调研 §6-2 的风险项）。前缀目录**按需创建**，不为空 Store 预建 65536 个目录。
- `tmp/` 与 `objects/` 在**同一文件系统**上，这是 `rename` 原子性的前提；跨设备 rename
  会失败（`EXDEV`），实现不得回退到"复制再删"。
- 暂存文件名使用**随机后缀**而非对象 id，避免崩溃残留被误认为已发布对象；同时保证两个
  进程同时写同一对象时不会互相覆盖同一个暂存文件（S13）。

## 4. 元数据 schema（格式版本 1）

`PRAGMA user_version = 1`。表结构：

```sql
CREATE TABLE store_policy (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE schema_definition (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    ddl          TEXT NOT NULL,
    schema_version INTEGER NOT NULL
) STRICT;

CREATE TABLE object_location (
    oid      TEXT PRIMARY KEY NOT NULL,   -- 64 个小写 hex 字符
    role     INTEGER NOT NULL,            -- ADR-001 role tag（1..=8）
    length   INTEGER NOT NULL,            -- 规范帧总字节数
    location TEXT NOT NULL                -- 相对 <store> 的路径，"/" 分隔
) STRICT;
```

- `store_policy` 存放打开时也要校验的常量：`format_profile`（对象域串与格式版本）、
  `max_object_len`、`schema_version`。它们是**数据**而不是代码常量，以便将来版本拒绝时
  能给出精确诊断。

  **每一个键在打开时都被强制执行**，比对基准是当前构建的编译期常量：

  | 键 | 比对基准 |
  | --- | --- |
  | `format_profile` | `schema::FORMAT_PROFILE`（`wcas/object/v1`） |
  | `max_object_len` | `cas_core::MAX_OBJECT_LEN` |
  | `schema_version` | `schema::SCHEMA_VERSION` |

  任一不符（或键被删除）→ `SchemaMismatch`，拒绝打开、不修复。

  > 这条规则由 T2-QA-1 确立：`max_object_len` 曾被写入却从不读取，ADR 声称"打开时校验"
  > 而代码只校验了另外两个键。**写入库中的常量必须被强制执行**——一个只写不读的声明比
  > 没有更糟，它会误导后来的读者。`every_recorded_policy_key_is_checked_on_open` 测试
  > 逐个键验证该性质，防止将来新增 policy 键时重犯。
- `object_location.oid` 是主键，因此**同一对象只能有一行**——这就是"不产生重复物理副本"的
  索引层保证，配合 S7 之后，`put` 的幂等性由主键冲突裁决，而不是靠"先查再写"的竞态窗口。
- `location` 存相对路径而不是绝对路径，Store 目录可整体搬移。
- `STRICT` 表确保类型不被 SQLite 的动态类型悄悄放宽。

### 4.1 schema 自校验（S4）

建库时的 DDL 原文写进 `schema_definition.ddl`。每次打开都重新执行一遍"应该有的 DDL"并
与库中记录**逐字比较**；不等则拒绝打开（`SchemaMismatch`），不尝试修复、不静默迁移。
这能同时拦住：被外部工具改过结构的库、不同版本二进制误打开的库、以及 schema 常量被改写
的库。

## 5. 对象发布顺序与逐步崩溃语义（S6/S7）

```text
put(role, value):
  1. 校验：Object::new → 取 ObjectId           （纯计算，无副作用）
  2. 查索引：SELECT ... WHERE oid = ?           （命中 → 直接返回 Exists，下面全部跳过）
  3. 写 tmp/<random>：规范帧逐字节写入
  4. fsync(tmp 文件)                            ← 数据落到稳定存储
  5. rename(tmp, objects/ab/cd/<oid>)           ← 目录项原子替换
  6. 刷新父目录（objects/ab/cd）：尽力而为       ← 见 §5.1 的实际保证
  7. BEGIN IMMEDIATE; INSERT object_location; COMMIT   ← 单事务，索引提交即发布
```

**每一步崩溃后的磁盘状态与恢复结果**（`✓` = 重启后该对象可读）：

| 崩溃点 | tmp/ | objects/ | 索引 | 重启后 `get` | tmp 处理 |
| --- | --- | --- | --- | --- | --- |
| 2 之前 | — | — | — | 不存在 | — |
| 3 中途 / 4 之前 | 半截文件 | — | 无行 | 不存在 | 清理 |
| 4 之后、5 之前 | 完整文件 | — | 无行 | 不存在 | 清理 |
| 5 之后、7 之前 | — | 完整对象 | 无行 | **不存在**（孤儿） | 保留（S10） |
| 7 的事务内（未提交） | — | 完整对象 | 无行（事务回滚） | 不存在（孤儿） | 保留 |
| 7 提交后 | — | 完整对象 | 有行 | ✓ | — |

关键性质，逐一对应上面的行：

1. **数据先于索引**：任何"索引里有行"的状态，其对象文件必然已经完整写入并 fsync。
   因此读者永远不会看到指向半截文件的索引行。
2. **孤儿安全**：`objects/` 里有文件但索引无行，只意味着"上次 put 没做完"，读者看不到它，
   下次同样的 `put` 会走完整流程（或直接复用该文件，见 §5.2），**不会**被误读。
3. **索引提交是唯一定义点**：S7 所以成立，是因为 `get`/`exists` 只查索引，不扫目录。
4. **重复 put 不产生第二份文件**：第 5 步的 rename 目标是**同一路径**，重复执行只是原子
   替换成同样的字节；索引主键冲突被当作幂等成功处理（S13）。

### 5.1 关于"刷新父目录"的实际保证（必须如实陈述）

Linux/POSIX：对目录 `File::open` 后 `sync_all()`，语义明确（`fsync` 目录使 rename 的目录项
变更持久）。

Windows：没有等价的公开文档化 API。实践做法是用 `FILE_FLAG_BACKUP_SEMANTICS` 打开目录句柄
再调用 `FlushFileBuffers`。需要如实记录的要点：

- `FlushFileBuffers` 在目录句柄上**调用成功**（本机 Windows 11 x64 / NTFS 实测返回成功）；
- 但微软文档没有为"目录句柄 + FlushFileBuffers"这一组合给出**契约级保证**；MS-FSA §2.1.5.7
  描述的是抽象对象存储语义，且该行为**只对 NTFS 成立**（微软工程师在官方 Q&A 中确认了
  这一边界，同时指出 Win32 层未文档化此用法）；
- 因此本项目对 Windows 的宣称是：**尽力而为的目录刷新**，失败不使 `put` 失败（`rename`
  在 NTFS 上本身是元数据事务，且后续第 7 步的 SQLite 提交带 `synchronous=FULL` 会触发一次
  卷级 flush，实际形成了兜底）。

**不得宣称**：Windows 上"rename 后目录项必然在掉电后存活"。本项目只承诺
**进程崩溃**后的恢复（这是崩溃注入测试能真实覆盖的范围）；掉电/内核崩溃下的保证受限于
上述平台语义，未做测试，**不作宣称**。

### 5.2 复用已有的孤儿文件

第 3 步之前若 `objects/<path>` 已存在（上次崩溃在 5 之后 7 之前留下的孤儿），实现直接跳过
写入与 fsync，只做第 7 步。这要求先验证该文件确实是预期的对象（长度 + 身份校验），
校验失败则视为损坏，报错而不是覆盖——因为覆盖会掩盖磁盘问题。

## 6. 读取与校验

`get(oid)`：

1. 查 `object_location`；无行 → `NotFound`。
2. 读文件（长度与索引行不符 → `Corrupt`）。
3. 解码规范帧（`Object::decode`，ADR-001 §4.4 的全部规则）。
4. **重算 BLAKE3 与 `oid` 比较**（ADR-001 D12）；不符 → `IdentityMismatch`，即数据损坏。

**坏对象一律报错，绝不静默填零、绝不返回部分内容**（spec §4 末条）。P1 无条件整对象重算
摘要；"抽样校验/缓存校验"是 P2 的优化，需另立 ADR 并说明新的信任链。

`verify()`：遍历 `object_location` 全表，对每行执行上面的 2–4 步，**再遍历 `objects/`
检查文件名合法性**（§3），返回汇总（检查数、失败项列表、非法名单列表）。
它是**只读诊断**，不修复、不删除任何东西。两条检查相互独立：索引里有行但文件丢失记为
`failures`（`Corrupt`），`objects/` 里有名字不合法的文件记为 `bad_names`。

`exists(oid)`：只查索引，不读文件——语义是"是否已发布"，与 `get` 是否成功解耦。

## 7. 打开与恢复协议（S9）

`Store::open` 的固定顺序，任一步失败即拒绝，不做后续步骤：

1. 打开/创建 Store 目录结构（create 模式）；只读模式要求目录已存在。
2. 打开 SQLite 连接。
   - create：写 DDL、写 `schema_definition`、写 `store_policy`、设 `user_version`。
   - 已有库：读 `PRAGMA user_version`。**不是 1 → `UnsupportedSchemaVersion`，拒绝**。
3. schema 自校验（§4.1）：DDL 逐字比对，不符 → `SchemaMismatch`。
4. 校验 `store_policy` 的 `format_profile` 与 ADR-001 的域串一致；不符 → 拒绝。
5. `tmp/` 清理：删除其下所有文件。**这是安全的**，因为 tmp 中任何内容都从未被发布
   （§5 表：tmp 有文件 ⇒ 索引必无行）。
6. 完成。孤儿对象不做任何处理（S10）。

只读打开（`OpenMode::ReadOnly`）跳过第 1 步的创建与第 5 步的清理（不修改磁盘），并在
`put` 时返回 `ReadOnly`。

### 7.0 并发打开时的清理竞争

两个进程同时打开同一个 Store 是正常情况，第 5 步因此必须容忍竞争：
另一个打开者可能已经把某个暂存文件删掉、或正把它 rename 走。实现把
`NotFound`/`PermissionDenied` 视为"已被别人清理"，不报错；只有清理后仍无法处理的
错误才向上传递。目标是**"tmp/ 最终为空"**，而非"由我亲手删掉每一个"。
（这一条是实现过程中真实发现的竞态：并发打开时有约 15% 概率失败，修复后 50 次全量
回归零失败。）

### 7.1 为什么孤儿只读不删（S10）

`objects/` 中有文件而索引无行，无从判断它是"崩溃残留"还是"依赖某条已发布路径但索引被
外部损坏"。P1 没有引用关系表（那是 T3 的命名空间），也就没有安全的可达性分析。删错一个
对象会静默破坏数据；留着一个孤儿只浪费空间。因此 P1 明确选择**只保留**，把删除推迟到
有引用关系的 GC（P2.11）。

## 8. 并发（S13）

- SQLite 侧：单写者，`BEGIN IMMEDIATE`，`busy_timeout` 保持 0——**不设自动重试与忙等**
  （D10/ADR-001 §6）。并发写冲突返回类型化 `Busy`，由调用方决定是否重做。
- 对象文件侧：同一对象并发 `put` 的两个进程各自写**不同**的 tmp 文件，rename 到同一目标
  路径；后者替换前者，但两者字节相同（同一 ObjectId 意味着同一规范帧）。
- 索引侧：主键 `oid` 保证只有一行；插入冲突按"已存在"处理，结果仍是幂等成功。
- **本卡不承诺**跨进程并发 `put` 的性能或公平性，只承诺**结果正确且幂等**。

### 8.1 并发写同一对象时的输赢处理（实现事实）

本卡实现期间，30–50 次全量回归中并发用例有约 15% 失败，查明的两个真实原因是：

1. **rename 竞争**：赢家先进、输家的 rename 在 Windows 上会拿到
   `NotFound`（暂存文件已被处理）或 `PermissionDenied`（目标正被赢家短暂持有）。
2. **清理竞争**：两个打开者同时清 `tmp/`，一方删掉的文件另一方正要删（见 §7.0）。

处理方式：出现这两个错误时，**不重试写操作**，而是读取目标文件并校验其字节是否恰好
等于本次要写入的规范帧（§5.2 的孤儿校验逻辑）；相等即视为 `AdoptedOrphan`（幂等成功），
不等则报 `Corrupt`。读校验本身允许有界重试（≤50 次、每次 2 ms），因为它是**不可变文件
的读**，不是带副作用的操作重放。

这不是"失败后重试"，而是把"并发赢家已经完成了我要做的事"识别为成功。真正的 I/O 故障
仍然原样上报。

## 9. 选型理由

| 选择 | 理由 | 放弃的替代方案 |
| --- | --- | --- |
| rusqlite 0.40.2 + `bundled` | 纯 Rust 绑定 + 随包编译 SQLite，版本可控、跨平台一致、不依赖系统库 | 系统 SQLite（版本漂移）、自研存储引擎（P1 无必要） |
| SQLite 存元数据 + 独立对象文件 | 大对象不进数据库，库体积可控；对象文件便于外部工具检查 | 全 pack 进 SQLite（LayerFS 做法，多一个 pack 引擎） |
| WAL + `synchronous=FULL` | 提交即持久，符合 P1-7 的崩溃恢复要求 | `OFF`（进程崩溃可存活但掉电丢已提交事务，与规格不符） |
| 二级前缀目录 | 避免单目录十万级条目 | 单层目录、平铺 |

关于 `synchronous=FULL` 的代价：每次索引提交多一次 fsync。P1 的 `put` 本来就要 fsync 一次
对象文件，索引再来一次属于可接受的开销；性能数据属 T5，本 ADR 不含任何测量结论。

## 10. unsafe 与平台代码边界

`cas-store` 声明 `#![deny(unsafe_code)]`，并**恰好有一处**豁免：

| 位置 | 内容 | 理由 |
| --- | --- | --- |
| `src/durability.rs` 的 `platform::sync_directory`（仅 `cfg(windows)`） | 一次 `FlushFileBuffers` 调用 | Win32 没有任何能刷目录的安全 API |

约束：

1. 该函数带 `#[allow(unsafe_code)]`，紧邻 `// SAFETY:` 注释说明句柄有效期与调用契约；
2. 平台专属 import（`std::os::windows::*`、`windows_sys::*`）**只出现在这一个模块**，
   由 `cfg` 门控；
3. 其余所有生产代码不含 `unsafe`，也不含平台分支——`grep` 验证结果见 §12；
4. 若将来要在另一平台实现目录刷写，同样收拢在这个模块内，不放宽 `deny`。

Windows 侧的 `FlushFileBuffers` 来自 `windows-sys 0.61.2`（微软官方 Rust 绑定），
而非手写 `extern` 块，便于审计与跟随上游签名修正。

## 11. 未实现特性（明确清单）

| 特性 | 计划阶段 | 说明 |
| --- | --- | --- |
| 命名空间 / inode / root 表 | T3 | 本卡只建 `store_policy`/`schema_definition`/`object_location` |
| 目录查询与路径 API | T3/T4 | 无 |
| CLI 命令面 | T4 | `fs-cli` 仍为占位 |
| 引用计数 GC / 孤儿回收 | P2 | S10：P1 只保留 |
| Pack 打包 / 压缩 | P2 或以后 | P1 一对象一文件 |
| CDC / delta | P2 | 无 |
| 掉电/内核崩溃持久性 | 未定 | §5.1：未测试，不作宣称 |
| Schema 迁移工具 | 未定 | 当前只拒绝不迁移 |
| 对象文件的在线校验/自愈 | 未定 | `verify()` 是离线只读诊断 |
| 跨进程并发性能与公平性 | 未定 | 仅承诺正确与幂等 |

## 12. 正确性风险与已知限制

1. **Windows 目录刷新无契约**：见 §5.1。这是本 ADR 最重要的诚实边界。
2. **单对象 ≤ 16 MiB**（ADR-001），因此 `put` 的临时内存上界为 16 MiB；P1 不做流式 `put`
   （整对象已在调用方内存中）。
3. **`objects/` 与 `tmp/` 必须同一文件系统**：由 `<store>` 目录结构天然保证，但用户把
   `objects` 做成挂载点/符号链接时可能破坏该前提，实现不检测（未实现的检查）。
4. **文件数密度**：十万级小对象会产生十万级文件（2 级前缀缓解目录项规模，不减少 inode
   数量）。这是 spec §6 P1-2 验收项，规模实测属 T5。
5. **`verify()` 是全表线性扫描**，大 Store 上耗时为线性，无增量校验。
6. **SQLite 提交与对象 rename 不是同一个原子单元**；正确性来自 §5 的顺序（数据先于索引），
   而不是分布式事务。任何"先建索引行、后写文件"的实现都是错误的。
7. **崩溃注入只覆盖进程强杀**，不覆盖掉电、磁盘满、写保护介质。磁盘满走
   `io::ErrorKind::StorageFull`/`ErrorKind::Other` 的类型化路径，未做专项故障注入。
8. **并发写同一对象的幂等判定依赖"字节必然相同"**：`put` 在 rename 竞争时通过读回目标
   文件校验来实现幂等（§8.1）。若该读在 50 次、每次 2 ms 的窗口内仍失败，会报 `Io`
   而非成功——宁可报错也不声称成功。

## 13. 测试清单（对照验收标准）

见提交说明与 `crates/cas-store/tests/`。本卡覆盖：

| 验收项 | 测试文件 |
| --- | --- |
| put 幂等（重复、并发） | `tests/store.rs` |
| get 验证（坏对象 → 类型化损坏错误） | `tests/store.rs` |
| exists / verify | `tests/store.rs` |
| schema 版本拒绝、schema 自校验、只读打开 | `tests/store.rs` |
| 重启恢复 + tmp 清理 | `tests/store.rs` |
| 崩溃注入（真实强杀进程） | `tests/crash.rs`（子进程 + 真实强杀） |
| 错误类型穷举 | `crates/cas-store/src/error.rs` 单元测试 + 各测试中的 `matches!` 断言 |

### 13.1 崩溃测试的实现方式

**序列只有一份。** `Store::put` 与崩溃测试调用同一个入口：

```text
Store::put(role, value)                    // 生产
  = put_observed(role, value, NoObservation)

Store::put_observed(role, value, observer) // 唯一的序列实现
  observer.step(Staged) ... Committed
```

`PublishStep` 枚举是序列的唯一描述；`put_observed` 在每一步之后回调 observer。生产传
`NoObservation`（空实现），崩溃测试传"观察到点名步骤就 abort"的闭包。因此 staging /
rename / 目录刷写 / 索引提交的**顺序变更会自动被崩溃测试覆盖**，不存在"测试副本与生产
实现漂移"的可能。

步骤与故障点（`crash-points` feature 内的 `CrashPoint` 一一映射到 `PublishStep`）：

| 故障点 | 对应步骤 | 崩溃时磁盘状态 |
| --- | --- | --- |
| `staged` | `Staged` | tmp/ 有未 sync 的半截文件 |
| `synced` | `StagedSynced` | tmp/ 有完整文件 |
| `renamed` | `Renamed` | objects/ 有文件，目录项可能未刷 |
| `dirsynced` | `DirectorySynced` | objects/ 有文件且目录已刷，**索引未提交** |
| `committed` | `Committed` | **索引已提交，对象已发布** |

`dirsynced` 与 `committed` 分别是 spec §4-8 要求的"索引提交前/后"两个故障点。

其余机制：

- 子进程是测试二进制自身（`crash_child` 测试 + 仅在该子进程环境中的变量）；
- 子进程在被点名的步骤调用 `std::process::abort`——真实进程死亡，不跑析构；
- 父进程读到子进程在 stderr 上的 `STEP <点>` 信号后 `wait`，并断言**退出状态非成功**
  ——若子进程正常退出，说明崩溃点没有生效，测试直接失败。用 stderr 是因为 `abort`
  不跑析构、stdout 的缓冲内容会丢失；
- 父进程随后重新打开 Store 并校验：发布状态、`tmp/` 为空、`verify()` 干净；
- `the_crash_path_observes_the_production_sequence` 直接对 `Store::put_observed` 记录
  实际步骤序列并断言其等于上表顺序，且每个 `CrashPoint` 都能映射到序列中的一步——这是
  "单一序列来源"的机器可验证证据。

**feature gate。** 崩溃钩子编译在 `crash-points` feature 下（默认关闭）：

```toml
[features]
crash-points = []
[dev-dependencies]
cas-store = { path = ".", features = ["crash-points"] }
```

自引用使 `cargo test -p cas-store` 仍跑全部崩溃测试，而**外部默认构建完全不含该模块**
（`cargo doc -p cas-store --no-deps` 不生成 `CrashPoint`，加 `--features crash-points`
才生成——已实测双向验证）。

**变体验证**：为确认测试不是恒真，曾把 `renamed` 崩溃点临时移到 rename **之前**，
`crash_after_the_rename_leaves_an_invisible_orphan` 立即失败；恢复后重新通过。

**稳定性验证**：并发用例曾因 §7.0/§8.1 的两个竞态在并行回归中约 15% 概率失败；修复后
连续 **50 次**全量 `cargo test --workspace` 零失败。observer 重构后重跑 30 次全量，
结果记录在提交说明中。

## 14. 下一步

- **T3**：按 §10 增加命名空间表，起草 ADR-003（命名空间 schema 与 root 发布）；
  `filesystem-root` 的值布局届时定死。
- **T4**：CLI 命令面，与 SDK 共用同一核心。
- **T5**：规模与性能（10 万小文件、1 GiB 随机读、内存上界），按 spec §8-6 给原始数据。
