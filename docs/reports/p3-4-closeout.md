# P3-4 收口报告（项目最终报告）

> 状态：终稿。所有数字来自 QA 独立复跑（L2/M/U 三段验收，SHA `46a097a`），判据编号可引（`criteria-and-acceptance.md` §十七，178–527 条）。
>
> **增补（2026-10-29）**：§12 收录 P3-4 收口后的 ADR-010 四期与收尾段（判据 528–647）。Backlog B-22/B-23/B-24 已随 ADR-010 落地消项。

## 0. 一句话

从 spec（"workspace CAS 文件系统"）到 Windows 侧可挂载、可读写、与 NTFS 对照 12/13 一致的文件系统——P1/P2/P3 全链闭合。

## 1. P3-4 交付清单

### 1.1 挂载能力
- ✅ 盘符挂载（X:/Z:/V:/Q: 等多盘符验证）：create/mount/start 全零，probe 列出内容
- ✅ 目录挂载：挂载点由 WinFsp 自建（FILE_CREATE + DELETE_ON_CLOSE），卸载后目录消失
- ✅ 写链：Create（六种 disposition）/Write（BOOLEAN u8）/SetFileSize（读回验证）/Cleanup（两段删除）/Close（引用计数——WinFsp 合同）/Read（EOF 夹取）/Overwrite（原地截断+属性替换）/ReadDirectory（marker 续读）/SetBasicInfo（两哨兵分开施加）/GetSecurity/GetSecurityByName（目录回落）/CanDelete/Rename/Flush——全部真实现
- ✅ 大小写折叠：fs-semantics `fold_bytes` 唯一定义，适配器入口归一化（分隔符 `\`→`/`）
- ✅ .gitattributes + pre-commit（CRLF/越界结构防护）

### 1.2 回调与结构
- 接口表 64 槽，实测校准链（判据 326/330/333）
- 注册表：一挂载一 Adapter 一 HandleTable；stop→remove→delete + 查无此键断言；miss 计数期望恒零
- FileContext 引用计数（memfs 合同：Open ref+1 / Close ref−1 / 零才释放——判据 460/461）
- Context id 单调递增永不复用，`id -> Box<EntryContext>` 表为唯一所有者
- Path { key, display }：匹配键折叠、显示名保留（一记录两字段——判据 443/470）
- DirOutcome { id, binding }：路径语义的回执由路径门给出（判据 452）

### 1.3 测试数字
- 锁内全量：**663/665**（2 红 = crash 族抖动，单跑绿；白名单口径逐字匹配 1546 签名）
- clippy --workspace --all-targets: **0**
- 判据：**527 条**（criteria-and-acceptance.md §一至§十七）
- B-17a 战役：9 轮（7 绿 2 抓），家族 2 crate/9 测试名/1546 统一签名，插桩诊断排收口后
- 不稳定红（1546 族）：**未修**，触发画像=高并行概率事件（环境重叠非必要条件）

## 2. L2 NTFS 对照（12 类 × 两形态）——定论

| 行 | NTFS | 挂载侧 | 档位 |
|---|---|---|---|
| 01 root | directory | directory | 一致 |
| 02 create+read | alpha | alpha | 一致 |
| 03 truncate | ok | ok | 一致 |
| 04 missing file | not found | not found | 一致 |
| 05a/b dir listing | a,b | a,b | 一致 |
| 06 rmdir non-empty | refused | refused | 一致 |
| 07 rename | old=gone new=present | old=gone new=present | 一致 |
| 08 case-only rename | renamed | request-not-delivered | **有据差异（U16）** |
| 09 delete while open | name=gone | name=gone | 一致 |
| 10 concurrent read | beta | beta | 一致 |
| 11 long name | refused | refused | 一致 |
| 12a/b attributes | readonly=yes / cmd | readonly=yes / cmd | 一致 |
| U15 多页 200 条 | — | count=200 unique=200 sorted ✓ | 一致 |

**13 行：12 一致 + 1 有据差异 + 0 缺陷。L2 通过。**

U16 依据：`mv` 与 `cmd ren` 双工具同形（均 rc=0 拼写不变）；`CaseSensitiveSearch` 翻位无变化；fsop.c Rename 分支直通无短路——**平台中转层（内核 rename 路径）短路，UM 不可修**（判据 467/468）。

## 3. M1–M9 挂载生命周期

| 项 | 结果 | 档位 |
|---|---|---|
| M1 缺 SDK | 错误消息带安装指引 | 通过 |
| M2 同盘符再挂 | 两次成功，内容一致 | 通过 |
| M3 目录挂载点 | 成功，卸载后目录消失 | 通过 |
| M4 二次挂载互斥 | 无 WorkspaceInUse 闸 | **B-22**（产品功能） |
| M5 有句柄时卸载 | 拒绝 ✓（超时不卸载）；持有者打印未实现 | 通过（partial，B-26） |
| M6 --force 卸载 | 诚实报告可能残留 + 已 flush 可读 ✓ | 通过 |
| M7 taskkill /F | 盘符残留，store 锁正确释放 | **平台局限**（WinFsp FSD） |
| M8 非管理员 | 挂载成功，行为可复现 | 通过 |
| M9 版本不匹配 | 版本闸 ✓ + 两端报告 ✓（不匹配路径待构造） | 通过（partial） |

**6 通过 + 1 平台局限 + 3 backlog + 0 缺陷。**（B-24 CLI 落地后 M5/M6/M9 从待环境升为通过——QA 复验 cfc0686）

M7 平台局限：WinFsp 用户模式设计——进程死后卷设备由 OS/驱动管理（store 锁已正确释放 ✓、文件不变 ✓——我们的部分没有错；卷设备需 `fsptool unload` 或重启清理）。

## 4. U1–U16 消项

| 项 | 结果 | 档位 |
|---|---|---|
| U1 rename-replace 目标有句柄 | FSD 拒绝（Access Denied） | 通过 |
| U2 SUPERSEDE 同 FileId | FileIndex 不变（fsutil 确认） | 通过 |
| U3 share mode + delete-pending | sharing violation + delete-pending ✓ | 通过（简化） |
| U4 目录 rename 有子孙句柄 | FSD 拒绝 | 通过 |
| U5 默认 SD | create/open 不因 SD 失败 | 通过 |
| U6 byte-range locks | 独占/拒绝/解锁全部正确 | 通过 |
| U7 cached I/O × 锁 | cached read 与写入一致 | 通过（简化） |
| U8 持久性 | write→unmount→remount→read ✓ | 通过（mmap 延后） |
| U9 ReadDirectoryChangesW | FspFileSystemNotify 未实现 | **待实现（P3-4b）** |
| U10 SUPERSEDE 属性重置 | 与 U2 合并验证 | 通过 |
| U11 v2 拒绝 v3 | v2 产物未保留 | **空转** |
| U12 版本/ABI 匹配 | static assert ✓ / 版本闸未实现 | 部分（B-23） |
| U13 有句柄时卸载 | 需挂载 CLI | **待环境（B-24）** |
| U14 短名+大小写折叠 | dir /x 无短名 ✓ fold 阻止重复创建 ✓ | 通过 |
| U15 多页 200 条 | 200/200/unique/sorted ✓ | 通过（消项） |
| U16 case-only rename | request-not-delivered | 有据差异（平台） |

**9+2 通过 + 2 待实现 + 1 空转 + 2 待环境 + 0 缺陷。**

## 5. 七环错误链（写链破案，判据 324–340）

| 环 | 码 | 根因 | 修复 |
|---|---|---|---|
| 1 | c0000010 | Overwrite@5 缺席 ⇒ fsop.c:907 类门整类拒绝 | 接 Overwrite |
| 2 | c0000078 | 自相对 SD 缺 SE_SELF_RELATIVE | control=0x8000 |
| 3 | c0000005 | GSBN 在 size 查询时仍写缓冲 | 两段式（只写 size） |
| 4 | Win32 267 | 夹具把根按文件造 | root = directory kind |
| 5 | c0000010 | 缓冲太小答成"未实现" | STATUS_BUFFER_TOO_SMALL |
| 6 | c0000010 | GetSecurity@16 没接（与 GSBN 两个操作） | 接 GetSecurity |
| 7 | 空目录 | ReadDirectory 签名不符 + 未写 *PBytesTransferred | 按 ddca7bd 签名改正 |

## 6. 分隔符 saga（502–519，收口前最后一段）

```
表面：挂载竞态（半活挂载——盘符在但回调零到达）
  → 一个 PathBuf 差异改变文件系统行为（UB 嫌疑）
  → mock.calls 探针：Open 从不升级 Create
  → 两道闸都答正确码 0x34
  → debug log 三行 IRP：根被答 0x34
  → 病根：WinFsp 交来 Windows 路径（\），本仓后端解析 /
```

修法：FFI 边界一次归一化所有门（`replace('\\', "/")`）。关闭三件事：探针失败 / QA① 子目录 error 3 / 挂载竞态。

## 7. 方法论沉淀（判据 299–527，四大主题）

### FFI 探针方法论（326/330/331/333/336/339/400/403/417/430）
- 位用探针（64 槽记号），形用裸值（十六进制打印+限长读串），头文件只当目录
- 签名与位不同源：成员顺序版本间重排，但成员签名稳定 ABI
- BOOLEAN 子寄存器只读低字节（u8，不读 u64——高位是栈残留）
- 门禁在成员之前（类门：Create+Open+Overwrite 缺一即整类拒绝）
- 文件式打开落到目录 = 打开它（NotADirectory/NotFound 都回落 open_dir）
- FspVersion 与校准不符 ⇒ 整表作废

### 证据与观测（301/304/309/328/335/337/347/352/354/434/455/465）
- 码没变来源变了（计数器读数 vs 错误码）
- 状态码说调用怎么想，OS 读数说现在是什么
- 错误链剥洋葱（每环一个具体缺陷，无一"重试就绿"）
- 指纹带时间窗与归属（差值配平非绝对值；per-mount 计数）
- 未注册返回 None 绝不返回默认配置（OnceLock 病根）
- 仪器三维：保真 / 隔离 / 可归属（一进程一挂载）
- 收窄不等于确认（措辞始终保留待验）

### 注入与改动安全（303/323/348/353/355/419/444/459）
- 结构靠写不靠 patch（文件到规模，重写比插入便宜）
- 注入三道防线：grep 特征 → 断言存在性 → 红法
- CRLF × \n 匹配 = 静默 no-op（工具与环境错配）
- 死代码是诚实的中间态，回退是撒谎的中间态
- 兜底不是修复是遮蔽——兜底的资格是能区分它兜的是什么
- 撞结构性理由时撤回是第一选项

### 挂载语义（341/349/369/415/435/453/494）
- 挂载点必须不存在（WinFsp 自建 + DELETE_ON_CLOSE 自删）
- 先接类门让错误码说真话
- 规则必须住在还有返回值的地方（CanDelete = 平台级错误出口）
- teardown 不是同步点（stop 返回 ≠ 队列回调已执行完——有界等待在 stop 之前）
- 值即身份（context 引用计数——memfs 合同：Open ref+1 / Close ref−1 / 零才释放）
- 边界翻译一次覆盖所有门（分隔符归一化分两次 = 中间态无效两个红都占）

### 诊断格言（worker 原话）
> "症状在最外层时，先去看最内层的约定（分隔符、键的拼写、值即身份）"
> "每一次'应该是这样'都要换成一个能失败的读数——不合格的仪器比缺陷更贵"
> "不是'我们的测试都过了'，而是'与 NTFS 逐行对照只有一行不同——这是外部参照'"
> "删掉一条兜底比加上一条难——先把兜底掩盖了什么问清楚，而不是先把错误静音"

## 8. 遗留与去向

### Backlog（新增）
| 项 | 内容 | 触发 |
|---|---|---|
| B-18 | snapshot 格式变更（索引键与显示名分字段） | 折叠下推磁盘 / 直读消费者 |
| B-19 | 折叠索引缓存 | L2/QA 验收显示扫描是瓶颈 |
| B-20 | 环境诊断（挂载竞态的会话状态因素） | 换机/重启后仍复现 |
| B-21 | ~~delete-pending 名字消失时机~~ | **撤销**（行为与 NTFS 一致） |
| B-22 | 挂载级互斥（WorkspaceInUse） | 产品功能 |
| B-23 | 启动时版本比对闸 | 产品加固 |
| B-24 | 挂载 CLI（mount/unmount/force） | 产品形态 |

### B-17a 诊断（crash 族）
- 家族：2 crate / 9 测试名 / 1546 统一签名 / crash 后 reopen 路径
- 触发画像：高并行概率事件（环境重叠非必要条件）
- 复现工具：QA 重放脚本（9 轮，2 抓 7 绿）
- 插桩诊断（两条观测项：dir 内容 / 第二连接）排收口后执行

### 既有 Backlog（B-1~B-17）
保留于 `docs/process/backlog.md`，不随 P3-4 收口消项。

## 9. 运行手册

- **盘符一次性使用**：旧盘符因 unmount 0xC000000D 残留，每轮测试用新盘符
- **DLL 版本**：`C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll`（2.1.25156.ddca7bd）
- **Git Bash**：`std::fs` 与 bash 工具形态不同（MSYS 路径 vs Win32 路径）
- **裸盘符**：`Z:` ≠ `Z:\`（317——"当前目录"语义）
- **非管理员**：挂载成功（WinFsp 用户模式设计）

## 10. 数据总结

| 维度 | 数字 |
|---|---|
| 判据条数 | 527 |
| 测试（锁内全量） | 663 passed / 665 run（2 = 白名单族抖动） |
| clippy | 0 |
| L2 对照 | 12/13 一致（1 有据差异） |
| M 生命周期 | 4/9 通过（0 缺陷） |
| U 消项 | 11/16 通过（0 缺陷） |
| 缺陷总数（三段验收） | **0** |
| Backlog 新增 | B-18~B-24 |
| B-17a crash 族 | 9 名，未修（排收口后） |

## 11. 引用

- 合并报告：`docs/reports/` p3-2-g1/g2/g3/g4-p33a/p3-3b/g1-supersede-a
- QA 验收：p34-readchain-acceptance.md + L2/M/U 三段（qa-archive-backup/）
- ADR-005（fs-semantics）/ ADR-009（SUPERSEDE 裁决）
- B-17 诊断卡：`docs/process/b17-diagnosis-cards.md`
- 判据全文：`docs/process/criteria-and-acceptance.md`（§一至§十七）
- L2 脚本：`scripts/l2-ntfs-compare.sh` + `scripts/l2-report-template.md`

---

## 12. ADR-010 与收尾段（判据 528–647）

P3-4 收口后，owner 裁定 CLI 行为模式重构（ADR-010，七项裁定 D1–D7）：**CLI 收敛为“生命周期管理 + 挂载服务”，文件操作完全经挂载后的文件系统**（文件级命令 put/get/ls/stat/mkdir/rm/mv/cat 全部移除，exit 2 + 指路消息）。

### 12.1 四期交付

| 期 | 内容 | 代表判据 |
|---|---|---|
| S0 | 文件命令移除（exit 2）；cli/current 全删（无默认，显式 `--workspace` 或路径发现）；全局 `--store`/`--workspace` | 528–552 |
| S1 | 挂载记录迁 `%LOCALAPPDATA%\layerfs\mounts\`（DELETE_ON_CLOSE 判活，进程死=内核删记录）；路径发现（resolve 纯函数 + 卷标安全网 + reparse point）；Ctrl-C = 正常卸载（flush→卸载→删记录）；控制管道骨架 | 553–570 |
| S2 | 发布锁 Gate(RwLock) 七判据 G1–G7（互斥/隔离/完整/深度/无绕过/饥饿/上界）；commit/snapshot/restore/rebind 从 `&mut` 改 `&self`；管道 commit/snapshot——**挂载内 `cd /j && layerfs commit` 走通**；A17 句柄身份跨 commit 不变 | 570–582 |
| S3 | 准入状态机（Open/Paused/Closing + 三回调持票，Paused 等/Closing 立拒——变异 116s = 真实死锁路径）；管道 restore/switch；W1–W8 | 583–626 |

**switch 语义（实测）**：换挂窗口 29.6ms（界 2000）；打开的文件句柄挡住换挂（exit 4，文件/目录数分开报）；目录句柄放行 + 报数字 + `cd .` 提示；新挂载失败回退旧 workspace（exit 1，内容仍在）；三种 shell 存活；控制管道串行（过渡态的诚实——期间不答 hello，结束后 180µs 恢复）。

### 12.2 收尾段（判据 620–647）

- **README + help**：用户入口齐了（README 176 行 + `help`/`help <command>` + `--help` 重写；判据 C1–C5——含“README 反引号 docs/ 路径必须存在”）
- **HandleTable 泄漏修复**（619）：每次 fs-cli 命令 +8 句柄永不回落——七轮读数定位（Cleanup 到 Close 没到 → 盘符也漏 → 不衰减 → refs 2→1 停住 → 记账自洽 live_refs=0 → 上下文表空 → 泄漏在 HandleTable：每次 open 注册新 handle 但上下文只记得第一个）。修法两件必须一起：一个 file_id 一份 handle + refs 计数——泄漏消失且计数语义不变
- **overlay 锁正名**（633）："database is locked" 的真名是 "workspace overlay is already open"（EXCLUSIVE 设计），归 in-use/exit 4——“独占是写者身份，不是资源争用”
- **覆盖率基线**（641）：6ecae76 首基线——行 82.76%、函数 82.90%（fs-semantics 100%；gc/apply.rs 73.20% 最低；串行消除并行 flake）
- **夹具治理**（638/644/645）：records_dir 单源 + 就绪判据两分法 + 等收干净 + 失败留现场——全量首次 739/739
- **并行族治理**：nextest 挂载类串行组；“固定两条红 → 一条偶发红”（mounted_write_chain，B-17a 等触发）

### 12.3 数据总结（增补）

| 维度 | 数字 |
|---|---|
| 判据条数（累计） | **647** |
| 全量测试 | **739/739**（首次全绿；另轮 738/739，唯一红 = mounted_write_chain 偶发，单跑绿） |
| clippy | 0 |
| 覆盖率（6ecae76） | 行 82.76% / 函数 82.90%（fs-winfsp 排除，原因具名） |
| CLI 命令 | 18 个生命周期命令；文件命令 0 个 |
| 换挂窗口 | 29.6ms（实测，界 2000） |
| Backlog 消项 | B-22（两层防护：记录层 + overlay EXCLUSIVE）/ B-23（版本闸）/ B-24（挂载 CLI） |

### 12.4 Backlog 更新

| 项 | 变化 |
|---|---|
| B-22 挂载级互斥 | **消项**（D5 两层防护落地：记录层先拒 + overlay EXCLUSIVE 兜底，exit 4） |
| B-23 版本闸 | **消项**（启动比对，不符即拒） |
| B-24 挂载 CLI | **消项**（mount/umount/list/version + force 清陈旧记录） |
| B-17a crash 族 | 保留；追加“半活族”条目（挂载活着记录未现，触发=连红两次，evidence() 已在场） |
| 新增 | 环境遗留：Y:/Z: stale 盘符需重启；C6 Display 穷举缺口；CDC 读路径 158 行未覆盖 |

---

> **从 spec 到 Windows 上的文件系统，再到完整的 CLI 与用户入口——P1/P2/P3/ADR-010 全链闭合。判据 647 条，全量 739/739，验收零缺陷。**
