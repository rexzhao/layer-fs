# P3-1 任务卡（`fs-windows`：驱动无关的 Windows 语义模型，L1 层）

> 状态：**已派**（2026-10-24）。依据：**ADR-009**（`docs/adr/ADR-009-winfsp-frontend.md`，main `3fc88f1`）+ QA 的
> `docs/reports/p3-acceptance-skeleton.md`（L1/L2/L3 分层）。
> **本卡不需 WinFsp SDK、不需管理员、不挂载**，跨平台 CI 可跑。

## 0. 为什么先做这一张

P3 的多数正确性判据与"是否真的挂载"无关：路径规范化、大小写折叠、错误映射、打开处置、共享模式、delete-pending 状态机、
目录枚举游标、时间戳转换、长度-偏移、属性语义——都能用**纯模型 + 表驱动**验证，且失败信号是确定性的（L1）。
WinFsp SDK 与挂载是 L2/L3 的事，且当前 SDK 未安装（owner 处理中）。

## 1. 范围（新建 crate `fs-windows`，**无 FFI、无 `cfg(windows)` 专属依赖**）

| 模块 | 内容 | "怎么红" |
| --- | --- | --- |
| `path` | Windows 路径规范化：`\\?\`、UNC、相对 `.`/`..`、尾点尾空格、保留设备名（CON/PRN/AUX/NUL/COM1-9/LPT1-9）、非法字符、255 UTF-16 单位 | 逐条反例表：每行一个输入 + 期望结果（拒绝 `OBJECT_NAME_INVALID` 或规范化后的形态），**缺行即红** |
| `case` | 折叠后映射：精确字节优先；`fold(name)` 索引 + `CompareStringOrdinal(TRUE)` 终判；**折叠歧义 → `OBJECT_NAME_COLLISION`**；不做 Unicode 规范化；枚举显示真实名 | 表驱动：`{"A","a"}` 同目录 → 冲突；case-only rename 三种情形（目标不存在/同一文件/被占）；**每格一条** |
| `error` | typed error ↔ NTSTATUS 映射，**`match` 穷举**（缺一个变体编译不过） | 编译期即可证：新增变体未表态 → 编译失败；另加一张"每个变体 → 期望 NTSTATUS"表 |
| `disposition` | NT 六种 disposition × 目标存在与否 × 是文件/目录（ADR-009 §4.2 全表） | 表驱动，**缺格即红**；断言"动作 + 结果"两列，不只结果 |
| `share` | 共享位规则 `(H.desired & N.share) \|\| (N.desired & H.share)`（读/写/删 3×3×已有句柄） | 9 组合 + 已存在句柄的情形；冲突必须拒绝 |
| `delete` | 两段删除：`SetDelete(TRUE)` 只置 pending；新 open → `STATUS_DELETE_PENDING`；最后一次 `CleanupDelete` 才删；可 cancel | 状态机用例：置位→open 被拒→cancel→open 成功；置位→关最后一个 handle→删除发生 |
| `dirent` | 目录枚举游标：分页续读（marker）、`.`/`..` 过滤、删除中条目规则、重命名中目录 | 每类一条；**续读不得重复或漏项**（生成 1000 条目目录，逐页拼接后集合相等） |
| `time` | FILETIME（100ns since 1601）↔ 内部时间戳；精度与不能表示的时间（溢出/早于 1601） | 边界表：0、1、`u64::MAX`、本地时区无关；往返转换恒等（在可表示范围内） |
| `length` | 偏移/长度运算：`offset + len` 溢出、稀疏写补零、越界写扩展、EOF 语义 | 边界表（`u64::MAX` 附近），与 P2 `truncate` 复活同族 |
| `attrs` | 属性字：只读/隐藏/系统/目录/归档——**P3 不持久化**，只做"映射与拒绝" | 断言：写属性被忽略且不报错，读属性返回文档写明的常量；文档同时写"不持久化" |
| `volume` | 卷信息：总/可用空间为**策略常量**（工作区语义上共享/无限） | 断言常量来源单一（不是 0、不是随机），并写明为何不是真实磁盘量 |

## 2. 判据的硬要求

1. **每张表必须有"缺格即红"的机制**（拆列或索引检查），不允许"手写几条代表性用例"；
2. 每个模块至少一条**变异可证伪**用例（故意改坏 → 必红）；建议变异点：`fold`、共享位运算、delete-pending 迁移、`dirent` 游标；
3. **不依赖 WinFsp**：本 crate 不链接 WinFsp、不在 CI 里跳过；`cargo test -p fs-windows` 在任何平台可跑；
4. **不得假装支持**：范围外（ACL/SD、byte-range locks、symlink/reparse、ADS、8.3、稀疏/压缩/加密/配额/USN）**本卡不做映射**，
   只在文档列出并在 P3-2 给出 NTSTATUS（ADR-009 §6 已定，本卡只需引用，不要复制一份表）；
5. 命名与错误文案：`OBJECT_NAME_INVALID` 之类 NTSTATUS 用**常量 + 名字**，不要裸数字（便于判据指认）。

## 3. 不在本卡（列出以避免范围膨胀）

- **核心侧缺口**（ADR-009 §9）：G1 原子 disposition / G2 按 FileId 的读路径 / G4 条件 rename —— 属 **P3-2 核心补口卡**；
- **adapter 与 FFI**（`fs-winfsp`）：需 SDK（owner 处理中）；
- **挂载、L2/L3 验收、性能对照**：属 P3-3 之后。

## 4. 交付

- 分支基于最新 `origin/main`；**四步固定动作**：`fetch` → `rebase` → `is-ancestor` → `diff -- docs/` 必须为空；
- 四件套 0；新增 crate 的 `cargo test -p fs-windows` 全绿；全量不回归；
- 提交说明：每个模块的"表 + 缺格即红"机制、变异用例、以及与 ADR-009 章节的对应关系（§4.1/§4.2/§5.2/§5.3/§6/§7）。

## 5. 验收机制（QA 交底，2026-10-24）——"缺格即红"怎么被证伪

QA 对每张表做**同一种变异**（在隔离 worktree 内改、跑、立即还原）：

| 模块 | 变异 | 期望 |
| --- | --- | --- |
| `disposition` | **删掉表里的一行**（不是新增用例） | 对应用例红；若绿 ⇒ "缺格即红"只是口号 |
| `share` | 共享位运算反转一个 `&&`/`||` | 至少一条组合红 |
| `case` | `fold` 改成"直接小写"或"不折叠" | 折叠歧义与精确优先分别红 |
| `delete` | "最后一次 Cleanup 才删" 改成 "SetDelete 即删" | 状态机用例红 |
| `dirent` | 游标 marker 偏移 +1 | 续读拼接后集合不等 ⇒ 红 |
| `error` | **新增一个 typed 变体** | **编译失败**（穷举 match）；若有人加 `_ =>` 兜底，须被某条测试抓红 |

**前提**：表必须由**类型枚举**生成，或至少断言 `表.len() == <枚举基数>`；否则"删一行"不会红。
（反例：P2 的 `let n = 3; assert_eq!(n, 3)`。）

### 三条机械核对（不运行）

1. **不依赖 WinFsp、不 `cfg(windows)` 跳空**：`grep -rn "winfsp\|cfg(windows)" crates/fs-windows/` 应为空或仅注释；
   `cargo test -p fs-windows` 必须在本平台真跑；
2. **NTSTATUS 常量 + 名字**：`0xC000…` 只出现在常量定义处，不得散落在断言里；
3. **范围外只引用不复制**：ADR-009 §6 只引用（复制会分叉），且不得有"返回成功"的桩。

### 两条容易糊过去的

- `attrs`：必须**同时**断言"写属性不报错"与"读回是文档写明的常量"——只断言前者等于没验；
- `volume`：总/可用空间必须是**单一来源**常量（两处各写魔法数以后一定漂移）。

### 交付时必须附两句

**每张表的基数断言位置（文件:行）+ 变异点清单**。有这两句 QA 15 分钟出结论；没有她要自己数。
