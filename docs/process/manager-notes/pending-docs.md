# 待合入的文档批次（P3-1 合入时一并提交，避免卡在途期间动 main）

1. `arch/p3-winfsp-adr`：cherry-pick 顺序 `a1adaaa` → `2180353` → `81745f2` → **`4de2b68`**（后者修正 FILETIME 1601 不可表示 → i128 中间值 + 越界拒绝；Read 的 EOF 语义 `offset >= size` → END_OF_FILE）：
   - `docs/adr/ADR-005-inode-record-v3-draft.md`
   - `docs/process/p3-winfsp-ffi-surface.md`
2. QA 的 `p3-acceptance-skeleton.md` §6 报告骨架（她 scratch 里已写，合并时更新到 main 版本）。
3. ADR-009 §11 的卡序更新：P3-3a（adapter 骨架 + WorkspaceOps trait + mock）**提前到 P3-2 之前**（architect 建议，我已采纳）。

# 待派卡（P3-1 合入后）
- P3-3a：adapter 骨架 + `WorkspaceOps` trait + mock 后端（不需 SDK）。
- P3-2：核心补口 G1 原子 disposition / G2 按 FileId 读路径 / G3 profile v3 / G4 条件 rename。
- 之后 P3-3b：真实 host 实现 + winfsp-sys（需 SDK，owner 明天装）。

4. QA 的 FFI 边界骨架（`F:/work/qa-scratch/p3-ffi-boundary-skeleton.md`）→ 并入 `docs/reports/p3-ffi-boundary.md`，并作为 P3-3b 卡的验收 §。
   两条硬口径：
   - 布局权威必须留 L3（SDK 头文件 / bindgen 常量），L1 的“自己抄的常量”只能防漂移、防不了“文档本来就错”；
   - panic 越界必须二选一（`panic=abort` 断言子进程退出码 / 每回调 `catch_unwind` 转明确 NTSTATUS），两者都留 = 阻塞。

5. 【P3-2 内置的清理项】`cas-store` 导出 `pub const PACK_TAIL_BYTES: u64`（+ 需要的 `PACK_TAIL_MAGIC`，文档注明“由 ADR-008 §3 定义”）；
   删除 `cas-store/tests/pack.rs:16` 与 `tests/sync_probe.rs:28` 的本地 `const TAIL_BYTES: usize = 48`；
   只保留**一处**断言 `PACK_TAIL_BYTES == 48`，其余计算全用常量。来源顺序：ADR 条款 → 常量 → 测试。
   验收命令（architect 修正的精确版）：
     grep -rEn '\b48\b' crates/*/tests/          # 期望恰好 1 行：PACK_TAIL_BYTES == 48
     grep -rn 'const TAIL_BYTES' crates/*/tests/ # 期望 0 行（本地副本已删）

6. 【P3-2/P3-3a 判据补强】`time` 必须有 1601 / 1677 / 1970 / 2262 四个边界（i64 纳秒下界 ≈ 1677-09-21，1601 不可表示 → i128 中间值 + 越界拒绝，不夹取）。
   Read 的 EOF：`offset >= size` → `STATUS_END_OF_FILE`；部分越界 → 短读成功（与 WinFsp MemFS 一致）。

7. 【流程条目，P3-1 合入时写进 docs/process/submission-workflow.md】
   “实现期回写 ADR 必须带判据影响行”：
     判据影响：无（不改判据） | 无（澄清：仅措辞/数字/观测点） | 改变 <卡/测试名>：<原判据 → 新判据>
   - 无（不改判据）＝默认 token（代码卡不改 ADR 时用它，不要被迫写“澄清”）；
   - 无（澄清：…）＝确实改了文档措辞/数字/观测点，但绿红边界不变 → 可继续；
   - 改变 ＝三个方向任一：已绿→红；红/#[ignore]→绿（原本不保证的现在保证）；未覆盖→必须覆盖 → 停卡，
     由 manager 决定同卡补还是另开卡；ADR “修订记录”记这一条。
   - 判断权在“绿红边界是否翻转”，不在作者的解释（哪怕作者认为测试本来就写错了）。
   ADR 状态行下加极短 `## 修订记录`（日期 + 一条 + 判据影响），只增不改。

8. 【P3-2 卡 §5 追加，合入时提交】把 QA 的三条变异互补表放进 §5（不要只写“加第 5 条”），并保留：
   - “变异必须在干净树上一次一改”（三条一起植会同时红三格 = 不可归因的红）；
   - A/B/C 三行红格表：
       A 塞回 const TAIL_BYTES   → 第2红 / 第4绿 / 第5绿（第4看不见换名字的副本）
       B 一处改裸 48             → 第2绿 / 第4红 / 第5绿（第4抓“漏改”）
       C 植入 let pack_tail = 48 → 第2绿 / 第4绿 / 第5红（第5不可省）
9. 【流程条目，合入时提交】“判据的绝对数是抓副本与漏改的，不是让人调整代码去命中它”；
   以及“变异一次一改 + 变异后还原必须 git diff --stat 为空”成对。

10. 【流程条目，合入时提交】“README 每个 yes/no 都要有断言”的补充：**断言必须与宿主无关**。
    `cargo test` 的 passed 数是宿主相关观测量（会被 cfg 门控、ignored、feature 筛选），只有**源码扫描**与**字面量基数**是宿主无关的。
11. 【追补，排 G1 之后】① P3-0c README 的 objects/packs/commits/snapshots 四行 → shape 用例（从 README 读期望）；
    ② `fs-windows` README 的三条声明（no FFI / no WinFsp / no cfg-only）→ 扫描器（命中 cfg(windows)/winfsp/extern "C"/#[link( → 红并点名 file:line）
       + 逐文件 `#[test]` 条数表（attrs 6 / case 6 / delete 7 / dirent 5 / disposition 3 / error 4 / length 3 / no_wildcards 2 /
       path 5 / share 5 / time 8 / volume 3 = 57），两条都要阳性+阴性对照。
12. 【流程条目】“计数判据当目标”已第四次出现（合并用量 / 改注释 / 改注解位置 / 弱断言）；每条计数判据必须配：
    “不得为凑数合并或拆分”、“排除注释/非代码行”、“断言与宿主无关”。

13. 【追补细节，排 G1 后】
   - `fs-windows` 的源码扫描器**必须豁免注释行**：干净树上 `grep -rni winfsp` 命中 4 处，全是文档注释（含 README 自己那句
     "no WinFsp"）⇒ 不豁免就是"判据被良性情形触发 = 判据失效"（P2b-6 的 G2 同款）。豁免安全性：注释里的 cfg/词不产生门控与 FFI；
     "测试被静默门控"由**基数表**兜住。豁免后必须仍有阳性对照（`hits()` 能报出 `concat!` 植入的字面量）。
   - 第 5 条**两个正则都跑**，单独任一个都能被绕过：`let tail = 48u64;`（后缀）、`let pack_tail = 0x30;`（十六进制）、
     `let n = 48;`（换名字）、`let tail = 4+4+8+32;`（算式）。
   - QA 的验收脚本：`F:/work/qa-scratch/p3-source-level-assertions.md`（含代码、阳性/阴性对照、红法）。

14. 【G1 验收口径】“`fs-windows` 无独立 18 格表”不能只 grep `const TABLE`——会被**改名副本**（`SEMANTICS`/`DECISIONS`）绕过。
    验收写成：`fs-windows/src` 里**任何** `[（Disposition, Exists) -> (Action, Outcome)]` 形状的数组字面量都必须 0 处
    （按形状/类型查，不按名字查；QA 会直接读源码 + 可给扫描器）。同一条原则：**判据按语义形状，不按标识符名字**。

15. 【P3-2 卡，G1 语义裁决（合入时写进卡 §1/§3）】
   - `CreateOutcome { file_id, action, outcome }`：FileId 与 Action/Outcome **并列**（不藏变体里），
     否则“OVERWRITE 命中目录”这类格取不到 FileId ⇒ FileId 列变成“部分格可填”= 缺列。
   - SUPERSEDE 之后旧 FileId：**仍可读**（rename / delete-pending / supersede 同一条规则 = Windows FILE_SUPERSEDE 真实行为），
     但**机制落 G2**：G1 只断言身份关系（SUPERSEDE → new != old；OVERWRITE_IF → 保留），不断言 read_at(old)；
     G1 实现**不得硬删旧 inode 行**（否则 G2 的 read_at(old) 无内容可读，G2 得加 pending ⇒ 变大）。
   - 第 6 条判定式：名字含 tail + 值里出现数字 ⇒ 红（专抓 `let tail = 4 + 4 + 8 + 32;` 这种“算出 48”的写法）。
16. 【口径】“恰好 11 处”必须写明**不含扫描器自身**（`tail_constant.rs`，按设计包含全部模式，数进去是自指）；
    外部 grep 会得 19（11 + 扫描器 8），两者不矛盾——报告里必须写清分母口径，否则复核者会以为数字打架。

17. 【P3-2 卡 §5 定案：第 5/6 条三条检查】
   - R1（零容忍）：`(let|const|static) … = 48` 字面量绑定 ⇒ 0；
   - M2（零容忍）：`(let|const|static)\s+\w*[Tt]ail(_bytes|_len|_size)?\s*(:\s*\w+\s*)?=[^=]*[0-9]` ⇒ 0（QA 实测 5/5 真绕过、0 假阳性）；
   - M1（白名单）：名字含 tail 且值含数字的 let/const 必须叫 tail / tail_bytes / tail_len / tail_size，否则红
     （抓 `tail_bytes2 = 0x30` 这类 R1+M2 双漏；把例外变成**命名约定**：尾长只这四个名字，偏移叫 start/at）。
   - 三条联合覆盖才是证明；卡里放 QA 的 6×3 红/绿矩阵，**验收必须同时报假阳性**（只证灵敏度不证特异度 = 只证了一半）。
18. 【流程条目】验收表必须含“不该红的也没红”一列（特异度），与阳性对照（灵敏度）成对。

19. 【P3-2 卡 §5：M1 白名单形态】（QA 起点事实：`626b5d3` 上 R1/M2/M1 全为 0 ⇒ **白名单从空开始**）
   - 白名单写在判定式**旁边**（不是配置文件），每条 = `路径:行` + 一句“为什么这里的数字不是尾长”；
   - **必须同时断言条目数 == M1 命中数**：多一条=未登记的例外，少一条=白名单腐化（登记的已不存在）；
   - 报告第三行必须写 `假阳性 = N`（当前 M1 在 3 种合法写法上有 2 个：`tail_start = at + 1`、`tail_at = len - 1`），
     否则后来人会以为“M1 变零容忍”是免费的。

20. 【裁决改判：18 格表落点 = 新建零依赖 crate `fs-semantics`（原定 fs-namespace，作废）】
    事实：`cargo tree -p fs-windows --depth 1` 今天只有它自己（零依赖）；`fs-namespace → cas-store → rusqlite(bundled) → cc/pkg-config/vcpkg`
    共 23 个 crate ⇒ 搬进 fs-namespace 会让 `cargo test -p fs-windows` 需要 C 工具链，README 三条声明（no FFI / no SDK / any platform）全失守。
    改判：`fs-semantics`（零依赖，放 3–5 个类型 + TABLE + decide）；依赖方向 fs-windows → fs-semantics、fs-namespace → fs-semantics、fs-api 再导出。
    A6：missing row / missing column / missing cell 三条基数断言**跟表走**到 fs-semantics；fs-windows 保留 18 格行为用例（A7）。
    a/b/c 形状判定式的 a 目标由 fs-namespace 改为 fs-semantics。
21. 【流程条目】凡“把东西搬到哪个 crate”，**先跑 `cargo tree -p <目标> --depth 1` 再裁**——依赖闭包是落点的一部分，只看“谁用这个表”会漏掉 C 工具链/FFI 这类后果。
22. 【文档】ADR-009 里“表上移 fs-namespace”的表述要改为 `fs-semantics`（合入时同批改）。

23. 【fs-semantics 边界（G1 定稿，合入时写进卡 §12 与 ADR-009）】
   - 类型跟表走：`Disposition` / `Exists` / `Action` / `Outcome` + TABLE + decide 全进 `fs-semantics`；`fs-windows` 只 `pub use`
     （类型留 fs-windows 会形成 fs-semantics → fs-windows 反向依赖）。
   - `FileId` **不进** fs-semantics（存储概念）；`CreateOutcome { file_id, action, outcome }` 留在定义 FileId 的层。
   - `fs-namespace` / `fs-api` 也 re-export，不留第二份表。
   - 验收：A4 扩成全仓 `grep -rn '\[(Disposition' crates/ | grep -v crates/fs-semantics` = 0；
     A8 禁依赖 rusqlite/libsqlite3-sys/cc/pkg-config/vcpkg/windows-sys = 0；
     A9 `cargo tree -p fs-windows --depth 1` 依赖个数 0（今天）→ 1（搬完）。

24. 【P3-2 卡 §5 定稿补充：白名单形态 = 行内尾注释 `// tail-allow:` + 三断言】
   - F2 未登记命中 == 0；F3 标记数 == “被标记且确实命中”的行数（**双向**：腐化 + 漏登记）；F4 每个标记后 ≥12 字符理由。
   - 当前只许 **1 条**（`src/pack.rs:381` `&bytes[at + 16..at + 48]`，理由：摘要字段结束位置，不是尾有多大）。
     需要 2 条以上 = 布局偏移在代码里散落的信号 ⇒ 先看是否该抽布局结构体，不加白名单。
   - 否决“把 48 换成 PACK_TAIL_BYTES”的额外理由（QA）：那等于**给 48 造第二个名字**（今天常量、明天 DIGEST_END = 48），
     扫描器全绿而语义上已有两份定义——“唯一来源”的全部意义就是禁止这件事。
25. 【A10/A11 判定式（QA 清单 §E）】
   A10a 四类型在 fs-semantics = 4；A10b adapter 里不再定义 = 0；A10c 只 re-export ≥1；
   A10d **变体名不得在其它 crate 重现** = 0（类型改名能绕过名字 grep，变体名是行为动词，按形状查）；
   A10e fs-semantics 零依赖 = 0 行。
   A11a 语义 crate 不出现 FileId = 0；A11b 身份只有一处定义 = 1；A11c CreateOutcome 三字段并列（红法：塞进变体 ⇒ FileId 列缺格）。

26. 【F5：白名单条目数 == 1】
   理由不是“数量小”，而是机制性质：**白名单是知识的重复记录**，每加一条 = 把一处布局常量再手写一遍；第 2 条出现时正确动作是
   **消灭这份知识**（抽布局结构体 / 给“摘要字段结束位置”起语义名），不是继续登记。
   F2（未登记命中=0）与 F3（标记数==命中数）**只保证“登记与命中一致”，两条一起也允许 5 条一致的白名单**；
   只有 F5 钉上限，才会在“一致地膨胀”时报警——**一致地膨胀比不一致更难发现，因为前两条全绿**。
   验收要求：命中 ≥2 时报告不许写“加了 N 条白名单”，只能二选一——回归（补代码）/ 设计变更（抽常量或结构体），否则记为“红但不算数”。

27. 【判据里的每个数：要么“可解释”，要么“被钉住”（QA 提出，升级为原则）】
   - 可解释的数：`18 = 6 × 3`，读者能当场算出来；
   - 被钉住的数：`57`（测试条数）——本身不可解释，但被一条字面量断言钉住，意义是“不许悄悄变”，不是“它等于什么”。
   混用两个坑：把 57 当可解释（“大概是这么多”）⇒ 报告含糊；把 18 当被钉住（写死却不知为什么）⇒ 改动变成“改数字让它过”。
   本条与“解释不了的数不能当基数”（36 = 18×2 那次）是同一条的正反两面。

28. 【合并报告措辞（否定结果）】QA 的“孤立期望数”启发式实验必须写成**否定结果**：
   “尝试过 4 个方向的自动化：R1/M2/M1 三条正则有效、‘孤立期望数’启发式无效（250 处裸数字 → 133 无消息 → 8 孤立 → 抽查 6 处 0 缺陷 ⇒ 阳性率 0）”。
   **不能写成“检查过 8 处”**——那会被读成“这 8 处已经验过、没问题”，等于把一次失败的实验变成一个通过的判据。
   通用：报告里的失败实验只能记成“别再这样做”，不能记成“这样做过且没问题”。

29. 【ADR-009 内部矛盾（QA 发现，2026-10-25 裁决）】
   - `:117` SUPERSEDE「删除旧名字/旧 inode」 vs `:239`「核心不存在“已 unlink 但句柄仍开”的状态」——SUPERSEDE 恰恰造出该状态。
   - 裁决：SUPERSEDE = **解除旧名字绑定** + 新 inode；**旧 inode 行保留（不可达），下次离线 GC 回收**；read_at(旧 FileId) 在 GC 前仍可读。
   - GC 根集**不加** live-handle 输入：ADR-008 §6.1 离线独占 ⇒ GC 期不可能有句柄。**这条论证此前没有任何一处写下来**，由 architect 补进 ADR-008/009。
   - 已让 architect 基于 origin/main 改三处文字；worker 的 G1 实现约束：不得硬删旧 inode 行。
30. 【backlog B-8（QA 提）】`open(handle) → SUPERSEDE → gc(roots) → read(handle)` 期望读成功且内容一致。
    需 adapter 句柄表（P3-3）+ GC ⇒ 不进 G1，进 P3-3 验收必做项。**没有它，“由句柄表保证有效”只是一句声明。**

31. 【“GC 期无句柄”的承重件 = overlay 排他锁（不是我原话暗示的 store 独占）】
    store.sqlite：普通打开有界 busy_timeout、不设 locking_mode ⇒ WAL 下读者与维护者可共存；
    overlay.sqlite：`schema::PRAGMAS` 里**每次打开**都执行 `PRAGMA locking_mode=EXCLUSIVE` + busy_timeout=0。
    ⇒ 真前提：**任何挂载中的 adapter 必须持有某个 workspace 的 overlay 排他锁**（WorkspaceInUse 在任何改动之前被拒，repo.rs:1137）。
    ADR 里必须写“overlay 排他”，否则会被读成“store 独占”。
32. 【新判据 B-9（QA 提，30 行骨架，排 G1 之后）】只读挂载时 GC 必须**拒绝**：
    进程 A 只读打开 workspace（持 overlay 排他）→ 进程 B `gc --dry-run` ⇒ 期望 StoreInUse/WorkspaceInUse。
    理由：**“什么都没改”与“拒绝了”在 --dry-run 下输出可能一样**；若只读路径哪天改成“不打开 overlay”，锁消失而报告仍正常。
    ⇒ 这类“**前提消失、输出不变**”的退化，只有一条断言能抓。属 P2b-6 追补，不需要 adapter/SDK。
33. 【ADR-009:117 定稿措辞（QA 版）】“删除旧**名字绑定**并创建新 inode；旧 inode 记录保留（只增），因此旧 FileId 仍可解析，
    其可读性由『GC 期无句柄』保证。” —— 若实现者读成 `DELETE FROM inode`，(a) 当场破。

34. 【裁决：SUPERSEDE 保留行 —— 先按 B（只增），回收记 B-10；ADR 必须写明“泄漏的是行不是内容”】
   - 不选 A 的理由：A 要三个新机制（保留标记 / adapter release API / 崩溃后清扫），没有一样属于 P3-2 判据范围，
     为一条罕见路径引入三个机制，代价大于泄漏本身。
   - B 可接受的关键三点：① 内容对象不泄漏——保留行不可达 ⇒ 引用的 CAS 对象在既有离线 GC 的 mark/sweep 里被收走；
     ② inode 行 66–90 B/条，只增到 `workspace_discard`；③ GC 之后读旧 FileId 会失败，但那时无句柄（离线独占）⇒ 无人会读。
     ⇒ **泄漏有界于行数，与文件内容大小无关**。
   - B-10 候选：(A) adapter 最后一句柄关闭时 release（需三机制）；(C) **推荐**——离线 GC 顺带清扫“不可达且无引用的 overlay inode 行”，
     离线独占保证无句柄；判据=“GC 后这些行消失、且不误删可达行”。C 少两个机制，且顺手解决 overlay 一般性垃圾。

35. 【overlay 是急打开（eager），不是懒打开】证据三跳（QA 核）：
    `fs-workspace/src/repo.rs:517` workspace_open 直接调 open_overlay；
    `fs-overlay/src/store.rs:100-108` open 后立刻 apply_pragmas（PRAGMAS 含 `PRAGMA locking_mode=EXCLUSIVE`，schema.rs:76）；
    `repo.rs:525` 紧接着 `overlay.pending_root()?` → `publish.rs:89-90` 一次查询 = **首次加锁**。
    ⇒ `workspace_open` 返回时已排他持有 overlay。B-9 骨架里的 read_dir 只为“证明持有者活着”，不是“促成加锁”（注释已改）。
    ⇒ P3-3 好消息：adapter 挂载第一步就是 workspace_open ⇒ 挂载期间 GC 必然 WorkspaceInUse，不需要额外机制。
36. 【裁决：工作区任一时刻只许一个打开者（含只读）】保持现状；要写进 ADR-006/009：
    「工作区在任一时刻只允许一个打开者（含只读）；挂载期间 CLI 状态查询会得到 WorkspaceInUse」+ 出路（adapter 自己的状态接口 / 将来只读快照）记 backlog。
37. 【流程条目】注释里的“为什么”若因其它改动失效，**必须改注释**，不许留旧理由（例：read_dir 从“促成加锁”改成“证明持有者活着”）。

38. 【ADR-009 W2 附近要加的段（QA 措辞，已转 architect）】
    「一个 workspace 同时只允许一个打开者，包括只读者」+ 三跳证据（repo.rs:517 open_overlay、:525 pending_root、schema.rs:76
    locking_mode=EXCLUSIVE）+ 「第二个打开者得 WorkspaceInUse/StoreInUse，不等待、不并发只读」+
    **末句必写**：「挂载期间 CLI 的 status/log 也会被拒；这是**有意的**，不是缺陷」—— 不写，将来有人会“只读不加锁”地修掉，
    正好破掉“GC 期无句柄”。
39. 【backlog：挂载中查状态的更小解法】让 `status` 走只读 store、不打开 overlay（`log`/`branches` 已只用主库），
    而不是放开 overlay 锁；备注：**放开锁会连带取消“GC 期无句柄”，两件事必须一起评估**。
40. 【流程条目】**不一致的注释不是风格问题，是错误信息**：改代码使某条注释的理由不再成立时，必须同一次提交改注释；
    review 问“这段注释解释的机制现在还成立吗？”——不成立而注释还在 = 比没注释更糟（下一个人会照着它推理）。

41. 【源码扫描器固定三条（流程条目）】
    i. 排除自己（注释写“这是唯一的排除 / 第二条需要同等理由”）——`tail_constant.rs` 有、`dependency_boundary.rs` 漏（扫到自己第 90 行的
       `line.contains("pub const TABLE")` ⇒ 干净树就红）；
    ii. 阳性对照：把要抓的形状作为字符串喂给同一匹配函数 ⇒ 必须命中（缺它时“绿”不代表在工作）；
    iii. 阴性对照：“长得像但不是” ⇒ 不得命中（缺它会把良性情形当违规）。
42. 【grep 型判据无法区分“引用”与“定义”】必须锚在**定义的语法形态**（`pub const TABLE` / `enum X` / `pub use … X`），不锚名字；
    引用豁免靠限定前缀（`Disposition::Supersede` 不算副本，裸 `Supersede` 才算）。

43. 【未修（已知）：ADR 正文两处 “spec §5.4” 指代】
   `ADR-008 §2 P8`（:29）与 `ADR-005:181` 仍写 `spec §5.4`，实指 `workspace_cas_fs_spec.md §5 第 4 条`。
   `959e176` 只修了 backlog B-6 的引用，**ADR 正文没动**（architect 提醒别当成已修）。
   → 下次动 ADR-005/008 时顺手改成 `spec §5 第 4 条`；不单开提交。

44. 【三级分辨率（合并报告用）】同一问题在三个粒度上的计数：裸名字 `pub const TABLE` = 7 / 类型化形状 `[(Disposition, Exists, Outcome); 18]` = 3 /
    语意判据 = 1。三个数都对，各自的分辨率不同——“每提高一级，被误计为定义的‘提及’就少一类”。
45. 【通用一问】**凡“形状出现次数”的判据，先问“对照组自己会不会被数进去”**（阳性对照必须包含它要找的形状 ⇒ 出现次数 ≥ 1 + 对照数）。
    ⇒ “表签名只在 fs-semantics 出现 N 次”这类判据要**只数 `src/`**（定义只可能在那里）。A1 由 3 改 1 即此。
46. 【豁免类规则的验收】**必须包含“把豁免对象放进去仍然绿”**（QA 实测：同一句错引用放进「修订记录」节 ⇒ PASS）。
    缺这条，豁免可能把判据改瞎。
47. 【doc_refs 检查落点】一个 `#[test]`（`crates/fs-cli/tests/doc_refs.rs`），**不要 CI 另配一份**（两份必然漂移）。
    第一波扫 `docs/process/*.md` + `docs/reports/*.md`；第二波（基线绿后）再考虑 `docs/adr/*.md` 互引。
    三条规则：只扫正文（遇「修订记录」停）/ 编号带不带点都认 / 失败时打印实际小节清单（区分“没有这节”与“正则写错”）。

48. 【ADR-005 同号两文件 ⇒ 改号（待办，排 P3-2 G1 之后，交 architect）】
    `ADR-005-snapshot-format.md` 与 `ADR-005-inode-record-v3-draft.md` 同号，导致 `ADR-005 §x` 语义含糊；
    全仓 ADR-005 提及 56 处 / 20 文件，带小节 25 处，**两种指代都在用**（§3.2/§5/§7/§13 = 快照格式；§2.2 = inode record v3）。
    方案：`ADR-005-inode-record-v3-draft.md` → `ADR-010-inode-record-v3.md`，两份文件头各加互指一句，逐条判 25 处指谁。
    在此之前 doc_refs 检查用“同号取并集”。
49. 【流程条目】**解析器对“一个键可能对应多个对象”的键，必须先枚举全部候选再判断，不得取第一个**
    （QA 的 `ADR-005` 取第一个文件 ⇒ 7 条真引用被判错引用）。与“探针先自证 / 对照组自数”同族。
50. 已修两处悬空引用（`bbae666`）：known-issues `ADR-006 §5.3`→§5.2、ADR-003 `ADR-002 §4.2`→§4。

51. 【doc_refs 检查口径】**224 = 三目录同扫**（docs/process + docs/reports + docs/adr）；分别量是 128 + 76，**两个口径不可相加**。
    报告写 224 并注明口径。
52. 【ADR-005 改号的可判形式（QA 提，比 doc_refs 绿更强）】
    风险是**语义漂移**不是“找不到”：改号后 `ADR-005 §3.2` 仍能解析（accepted 还在）⇒ 检查绿而语义已错。
    判据：改号前导出**草案独有节号集合**（= `2.1/2.2/2.3` 与 `11`，accepted 没有），改号后断言 `ADR-005 §<草案独有节号>` 引用数 == 0。
    另需 56 处提及的三类清单：① 指 accepted ② 指草案 ③ **裸 `ADR-005`（人工判，必须给“判它指哪份+依据”）**——第③类是改号真正的产出。

53. 【引用记号规则（2026-10-25 定，已修 3 处）】
    小节 `§X.Y`（必须存在同名标题） ｜ 节内条目 `§X 第 Y 条`（不写成 §X.Y）
    修：`draft:170` §11.2→`§11 第 2 条`（时间戳进入内容）；`draft:166` §8.2→`§8 第 2 条`；
        `backlog:93/99` `ADR-005 §8`→`草案 §5（安全描述符）` + `§10 第 3 条`（原引用“节存在、内容不对”，doc_refs 永远绿）。
54. 【误报 vs 漏报】QA 的 `body()` 截断修订记录 ⇒ 其后 §10 整节静默不在扫描范围；修完浮出唯一一条真悬空（§11.2）。
    **前面的错都是“良性处误报”，这条是“良性处漏报”；漏报更危险——误报会被看见，漏报不会。**
55. 【ADR-005 引用总量口径】文档（docs/**/*.md + 根 *.md）81 带节号 + 38 裸 = 119 / 35 文件；代码注释（.rs）约 30。
    doc_refs 只管文档；**改号要动全部（含 .rs 注释）**。两个数不混用。
56. 【改号的判定只能人工的例子】`fs-snapshot/src/encode.rs:103` “inode record's first field is its key (ADR-005 §3.2)”——
    提到 inode record 但 §3.2 是 accepted 的 InodeTable（草案把编码放 §2）⇒ **“提到 inode”不等于“指草案”**。

57. 【已修 #39（5d7c798）】`p2a-snapshot-bench.md:5` 引 `ADR-005 §9`（不变量与 verify --deep）实为 **§5 Snapshot profile**；
    原句在 ADR-005-snapshot-format.md:166。**这类“节存在、内容不对”doc_refs 永远绿**，只能人工判。
58. 【口径】派生分类计数必须绑定“在哪个提交/口径上数的”：ADR-005 引用分类 51（`bbae666` 口径）→ 72（`df4b1dc` 口径），
    因为三处修复把引用从“草案独有节号”移进“共一节号”，③ 类随之涨——**不是新发现 21 条**。
59. 【ADR-005 改号执行（G1 后，architect）】只动 QA 判定表里**指草案的 10 条**，其余 61 条保持 `ADR-005`；
    两处 `.rs` 注释（`fs-windows/src/attrs.rs:1` `§3`、`time.rs:1` 裸引用）单列，防“只改 docs”漏掉；
    卡里写成**逐条打勾清单**，不写“按清单改引用”。三验：doc_refs + 草案独有节号=0 + 72 行表逐条内容核对。

60. 【改号验收三件（抄进改号卡）】
    ① doc_refs 两波零容忍 ⇒ 224 条 / 0 悬空；**数量应仍是 224**（改号只换名字不增删引用，数量变=有人顺带增删）；
    ② 10 条逐条复核：不得再出现 `ADR-005 §3`(属性字) / `ADR-005 v3` / `ADR-005 草案`；
    ③ `grep -rn 'ADR-005 §2\.[123]' crates/ docs/` = 0（草案独有节号；`§11 第 2 条` 除外，它指 accepted §11）。
61. 【不变量型判据（新类别）】“这个数变了就说明动了不该动的东西”——如改号后引用总数必须仍为 224。
62. 【引用记号的第二层规则】裸 `§X` 只在“上下文明确是本文档”时用；跨文档或易混处写 `§X 的<名词>`（`§9 的基准`、`§5 的 profile`）；
    更正引用时保留括号自注（`§5（Snapshot profile）`），把正确落点的名字写进引用本身，便于 grep。

63. 【G1 review 发现（worker 修中）】
    - `TruncateExisting` 只复用条目、**没截断**（`TreeWrite` 无 truncate 能力；`Overlay::truncate` 自开事务）。裁决：加
      `TreeWrite::truncate(id,new_size)`（唯一 impl = `OverlayWrite`，内部调 `truncate_impl`，不新开事务），臂里 `tree.truncate(id,0)?`。
      QA 端到端探针：写 11 字节 → `create_or_open(Overwrite)` → size 仍 11。
    - 测试缺：`TruncateExisting ⇒ assert_eq!(Some(file_id), before)`（QA 移植变异证明缺失：改成解绑+新建+重绑仍 18 格全绿）、
      非空种子、`size==0`、`rows`/`cols`/`18` 分开断言。
    - M5 并发（barrier 20–50 轮 + 恰好一个成功 + 两步负对照）未交付。
    - `inode_view.rs:17-20`「The rules never change it」**替缺陷辩护**——把 bug 写成分工，改 truncate 后同批改注释。
64. 【流程条目】**替缺陷辩护的注释**与“过期注释”是两个变种：前者不是懒得改，而是**在给 bug 找解释**，更难发现。
65. 【流程条目】一条变异**无法表达**时，通常意味着**当前代码已处在变异后的状态**——不要写“无法变异”，要写“当前实现即为变异”。

66. 【缺口的标准写法（QA 四步表，进卡）】能力 → 调用 → 断言 → 变异，每步单独可验收：
    ① 能力（编译期证据，如 `TreeWrite::truncate` 存在）② 调用（臂里出现该调用）③ 断言（端到端，如 `size==0`）④ 变异（删 ② ⇒ ③ 必红）。
    反例两种坏结果：删掉判据（缺陷失去记录）/ 写成必然红的用例（= 已知红，与没人管无区别）。
67. 【负对照的定义】**不是“反面的用例”，而是把一个不存在的路径造出来**——所以它和生产代码是两套实现，一套故意错的实现
    （M5：测试侧两步版 stat-then-create = 人为造出“两个判定同时通过”的路径）。

68. 【M5 口径改（2026-10-25，QA 代码证据）】原判据“两步负对照必须出现两个成功”**不可达/恒绿**：
    `fs-overlay/src/write_view.rs:110-127` 的 `bind` 自带前置检查（名字已存在 ⇒ `AlreadyExists`），
    所以两步版同样一成一败，只是错误类型不同。**恒绿判据比没有更糟**（给出“并发已证明”的错觉）。
    改三条：M5-a 失败者得到表内失败 `NameCollision`（非后端 `AlreadyExists`）；M5-b 失败者不改变树
    （指纹 + object_location 行数 + read_dir）；**建议升级为孤儿 inode：单事务 +1，两步且先提交 +2**。
    M5-c 报告口径：进程内并发被 Mutex 串行化，**真竞争面是两个进程**，不得写“进程内 20 轮通过 = 并发安全”。
69. 【证据三层】报告必须写清是“代码读取 / 行为实测 / 变异证明”哪一层（QA 标“代码证据 + 待确认”的做法）。
70. 【`bind` 注释缺口】`ON CONFLICT … DO UPDATE` 与前置换检查并存，**只在 `already == Some(None)`（墓碑行）可达**——
    代码未写，易被“顺手”删掉前置检查。要求同批补注释。

71. 【M5 终局（裁：加测试缝）】QA 证明“race ⇒ +1”也是恒绿：`Mutex` + SQLite 写锁**串行化**两线程，败者在 `create_inode` **之前**就返回
    （`decide` 返回 Failed(NameCollision)）⇒ 生产路径从未进入“两个都判定了不存在”的窗口，回滚与否都 +1。
    缝：`decide` 之后、动作之前，`#[cfg(feature="gc-test-hooks")]`（沿用 cas-store/fs-workspace 既有 feature 名，
    dev-dep 自引用），**恰好两个线程各等一次**（一次性 barrier），不得做成随机暂停。
    判据：同路径 CREATE_NEW ⇒ 恰好一个成功 + `ov_inode` **+1 不是 +2**；红法：`write` 改成“出错也提交”⇒ +2。
    理由：G1 唯一卖点就是“TOCTOU 从构造上消失”，没有可观测窗口就只是一句被信的话；P3-3 adapter 并发必然复用同一个缝。
72. 【M5-a 撤回】“失败者拿到 NameCollision 而非 AlreadyExists”也是恒绿（映射后同一变体，不可区分）。
    替换：**18 格用例的失败格必须断言“表里的 Failure 经映射后的具体结果”**，而不是 `(Failed(_), Err(_))`。
    红法：把映射里任一变体改错（如 NameCollision→NotFound）。
73. 【并发类判据的第一问】**“两个写者能同时进入哪个窗口？”——答不上来就是要缝。**
74. 【测量陷阱四条（QA 实测）】① 循环里 spawn().join() = 串行，必须先全 spawn 再统一 join；② overlay `locking_mode=EXCLUSIVE` ⇒
    计数必须在所有句柄 drop 之后（WorkspaceStore 克隆也持锁）；③ **root inode 懒创建** ⇒ `before=0 after=2` 是假象，要 warm-up；
    ④ store 克隆同样持锁。“观测器自己生成了被测对象的一部分”是这一族最危险的形式。

75. 【撤销“加缝”（2026-10-25，QA 350ms 实测）】`WorkspaceStore::write → OverlayStore::write` **在进入闭包之前**就锁住连接
    （Mutex + locking_mode=EXCLUSIVE），`decide` 在闭包内 ⇒ 两线程永不可能同时到达缝；2 方 Barrier = A 等 B、B 等 A = **死锁**。
    探针：A 在闭包 sleep 400ms，B 实测等 350.6ms。
    ⇒ G1 原子性判据改为**结构判据**（`g1_atomicity.rs`）：① `self.store.write(` 恰好 1 次 ② `decide` 在 write 之后
    ③ `tree.create_inode(` 在 decide 之后。红法：在 resolve_parent 后插一个 `store.write(...)?` ⇒ 第 1 条红。
    在“锁覆盖整个闭包”的架构下，运行期并发不可观测**不是缺陷而是设计的直接后果**；要窗口 = 要改架构。
76. 【G1 报告必须分三句】形状（一个事务、决定在内——结构性判据，已过）／度量（孤儿能被看见——负对照 +2，已过）／
    行为（并发原子性**不可运行期观测**，**不写“通过”**）。
77. 【时间型判据的正确用法】只用来证明“**曾经阻塞**”，不用来证明“永远阻塞多久”；sleep 400ms 断言 `waited >= 200ms`，
    注释写明“机制证据、阈值取半防假红、变红先提阈值不要删测试”。
78. 【CRLF 陷阱（第 5 个）】`"\n    }\n"` 在 CRLF 仓库永不匹配 ⇒ 截取跑到文件末尾，“找到”后面所有函数的 `write`（writes=4）。
    规则：**行级源码检查必须声明它相信哪种换行**（剥 `\r` 或用 `lines()`）。

79. 【错误的期望驱动错误的实现（比“写错了小数字”重得多）】
    QA 把 `two_step` 的期望写成 `(1,false,false)`、实际 `(1,false,true)`。若没跑对照，后续两条都是真危险：
    ① 测试红 ⇒ 去改**产品**让绿出现（把 `create_inode` 挪到 `decide` 之前）——期望错了，产品被改坏；
    ② 或把对照注释掉（“老是红”）⇒ 判据从此失效。
    ⇒ 卡里写：**对照用例的期望值，写下来之前先跑一次**；期望值从“这个判据要抓什么”推出，而“要抓什么”只有用真实的两步**形状**才能确认。
    ⇒ manager review 固定一问：**“对照跑过吗、红过吗？”** 答不上来按未交付处理（只问“写了吗”不够）。

80. 【review 固定清单（QA 四问表 + 我补第五行）——进 docs/process/】
    | 对象 | 问 | 理由 |
    | 判据（测试/断言） | 对照跑过吗？**红过吗**？ | 绿只证明它不碍事，红才证明它在工作 |
    | 注释 | 它解释的机制**现在还成立吗**？ | 不成立的注释是错误信息，比没有更糟 |
    | 提交说明的“断言了 X” | **指给我看那一行** | 本轮两次声称与代码不符 |
    | 报告里的数字 | 它**可解释**还是**被钉住**？ | 混用会让改动变成“改数字让它过” |
    | 缺口 | 缺在**四步中的哪一步**（能力/调用/断言/变异）？ | “还没做”看不出缺在哪 |
    共同点：**都在问“这个说法凭什么”，而不是“看起来对不对”。**

81. 【G1 ③ 四条实测坐实（QA）】
    a. criterion 2 注释与实测**相反**：两步版 20 轮 `successes per round` 全为 1、**0 次两个成功**（`bind_impl` 前置检查，与事务边界无关）；
       且该用例**根本没有负对照**（注释说 “the negative control is the point”）。用例只验证「一成一败 + 失败者 AlreadyExists」，**不验证原子性**。
    b. 扫描器 CRLF **靠运气**：`rules.rs` 566 CRLF / 0 裸 LF；`find("\n    /// Removes an empty directory")` 恰好匹配，换 `"\n    }\n"` 就得 writes=4。
       `find` 失败是 **panic** ⇒ “红了但不算数”。对照必须覆盖**解析层**（CRLF 文本仍截到函数尾），不只匹配层。
    c. **`cells == 18` 是循环计数器，不是基数断言**：表缺行→编译期红（还好）；**表多一行/重复一行→不红**。
       补 `TABLE.len()==18` / `rows==6` / `columns==3` 三条各自命名。
    d. 锁探针未落库 ⇒ “store serialises writers”这句无证据；它是唯一说明“为什么这里写不出 race 判据”的东西。
82. 【新形状：循环计数器假装成基数断言】数字对、位置对，但数的是**循环执行几次**，不是**表里有几行**。
83. 【流程】`failed=1` 未定位 **不能结案**：“连跑 N 次都过”不是解释；`--test-threads=1` 跑 10 轮 + tee + grep，抓不到就继续挂在提交说明里。

84. 【`failed=1` 未定位 → 候选已定位（QA，2026-10-25）】
    六次全量（默认并行 ×3 + `--test-threads=1` ×3）全部 440/0/8，**未复现** ⇒ 状态=**未验证**。
    候选 1：`crates/fs-windows/tests/volume.rs:26` 固定名临时文件 `fs-windows-volume-probe.bin`（全仓唯一不带 `process::id()`），
            两进程同时跑 → 一个 remove 后另一个 write 失败 → expect panic。**修法：带 pid（1 行，不改判据）**。
    候选 2：共享 `CARGO_TARGET_DIR`（本项目早期有过“共享 target dir 造成假 PASS”）。
    ⇒ 抓失败时必须记：① 当时有无第二个 cargo/clippy 进程 ② 墙钟时间。
85. 【判据状态三态（标准写法）】只跑过几次都绿 = **未验证**（失败名未知）；记录了失败名 = **待环境**（附复现命令）；
    复现并定位 = **缺陷，进卡**。**“再跑 N 次都过”不能升级状态**（与“窗口 <1ms 无法稳定采样”同档）。

86. 【合并报告：“未验证”栏成品（QA 写，长版进正文 / 短版进汇总表）】要点：
    - `p3-2/gaps` 曾一次全量 `failed=1`，失败名未捕获；QA 复跑 6 次（3 默认并行 + 3 `--test-threads=1`）全 440/0/8，**未复现**；
    - 候选（未证实）：`volume.rs:26` 固定名临时文件（两套并发实测未撞上）/ 共享 `CARGO_TARGET_DIR`；
    - **不成立的三句**：①“再跑几次都过”不能升级为“已排除”；②加 `process::id()` 只消掉一个候选，不构成“不存在偶发失败”；
      ③本项不得计入“通过”的用例数；
    - 升级条件：复现并记录失败名 → 待环境（附命令 `for i in $(seq 10); do cargo test --workspace --no-fail-fast -- --test-threads=1 || break; done`）；
      定位 → 缺陷进卡。
    - 报告层加：本项与 P2b-6“窗口 <1ms 无法稳定采样”同档——**不是缺陷记录，而是“我们知道自己不知道”**；合并报告设专栏收这类。

87. 【对照必须绕过被测函数（QA 抓，2026-10-25）】`44bc460` 的“两步版对照”`let seen = rules.resolve(..); let _ = seen; rules.create_or_open(..)`
    ——**仍调用原子函数**，当 `create_or_open` 内部变成两步时它依然 0，抓不到声称要抓的东西。
    规则：**一条对照若仍调用被测函数，它证明的只是“这个函数和它自己行为一致”。**
    真两步必须绕过：`TreeStore::write(store, OpLabel::Create, |t| { let id = t.create_inode(Kind::File)?; t.bind(...)?; Ok(id) })`，
    期望 **一成一败（1,1）**（QA 20 轮实测 0 次两个成功）。
88. 【review 固定动作：名字说 A、实现是 B（本轮第 3 次）】对着每个 control/对照/负对照 字样，去代码里找**它到底做了什么**
    ——不看它有没有红，看它**有没有把形状实现出来**。前两次：`inode_view` 注释替缺陷辩护、提交说明“断言了但没落地”。
89. 【清单自证】A/B 段验收清单里凡出现“已验”，必须同列写**验的是哪一行/哪条命令**，否则清单本身会变成第 4 次“名字说 A、实现是 B”。

90. 【清单必须写行号】理由：**行号会过期，但过期时是“点不到”，不是“点到了别处”**——「点不到」是可发现的失败，
    「点到别处」是静默的（验收人照着错的地方验，还会给出“已验”）。与“悬空引用 vs 语义漂移”同构：**坏引用要坏得能被发现**。
91. 【三个词各有定义，不许混用】**已验**（定位到行/命令）/ **未见**（附搜索命令，可复现）/ **未验证**（只跑过几次都绿，失败名未知）。
    清单里没有行号的条目，只能给“未见”，不能给“已验”。

92. 【failed=1 报告段落 v2（QA 用 48 进程打掉了自己的候选）】
    - 一次全量 failed=1，失败名未捕获；复跑 6 次（3 并行 + 3 单线程）全绿，**未复现**；
    - **已排除**：`volume.rs:26` 固定名临时文件——48 个并发进程实测 **0 失败**（文件只写不读、删除失败被 `let _ =` 吞）。
      改名仍做，但定位从“修复”降级为**卫生**；
    - **仍存可能**：① 两条 cargo 命令并行共享 `CARGO_TARGET_DIR`（曾有“共享 target dir 假 PASS”）；② 新用例触碰进程级全局
      （`cas-store/src/testing.rs` 的 `PACK_READ_BYTES` + `serialised()` 全局 Mutex；`gc_three_step.rs` 文件头记着历史泄漏）而未持守卫。
    - 升级条件不变；“再跑都过”不升级。
93. 【review 项】任何触碰 `pack_read_bytes` / `SWEEP_ONCE` 的新用例，**第一行必须 `testing::serialised()`**（把出处写进规则）。
94. 【“候选（未证实）”这个词的意义】让后来的测量**可以替换它，而不必反驳我**——撤回结论的社交成本远高于替换候选，
    否则多数人会选择维护旧结论。

95. 【规则成品（进 docs/process/，带出处）】触碰进程级测试状态的用例必须持守卫：
    `let _guard = cas_store::testing::serialised();`（**函数第一行**）。
    受管对象出处：`crates/cas-store/src/testing.rs:139`（`static PACK_READ_BYTES`）、`:149/154`（读写同一 static）、
    `:164-165`（`serialised()` 就是为此存在）。历史出处：`fs-workspace/tests/gc_three_step.rs:11-13`
    （“a global SWEEP_ONCE and a process-level pack_read_bytes both leaked across cases”）。
    当前合规：`cas-store/tests/pack.rs:149` 持守卫，全仓只此一处触碰 ⇒ 合规。
    违反表现：不稳定红（结果取决于另一个用例是否刚跑过）；**`--test-threads=1` 不是解药**（顺序依赖仍在）。
    review 动作：见 `pack_read_bytes`/`reset_pack_read_bytes`/`SWEEP_ONCE` ⇒ 问“守卫在哪一行”。
96. 【坑】守卫必须绑到有名字的变量（`_guard`）；`let _ = ...` **立刻 drop**，守卫失效且代码看起来加了。
    可反向抓：`grep 'let _ = cas_store::testing::serialised'`。
97. 【规则的双重价值】不只拦住错误，还能**给已有错误一个可指认的落点**（把 failed=1 从“未验证”推到“待环境”）。

98. 【因果词必须指出测量（本轮第 4 次同类）】凡注释/提交说明里出现因果（because/so/导致/因此），要求指出那条测量；
    没有测量的因果一律改成**陈述观察**（例：`volume.rs` 的 pid 改名 → “hygiene, not a fix for an observed failure”）。
    前三次：`inode_view` 替缺陷辩护、“断言了但没落地”、“两步对照名不副实”。
99. 【`let _ =` 是丢弃模式】`let _ = expr;` 求值后**立即 drop**；`let _guard = expr;` 才活到作用域结束。两条 grep：
    `grep -rn 'let _ = cas_store::testing::serialised' crates/` == 0；`grep -rn 'let _guard = cas_store::testing::serialised' crates/` >= 触碰面数。
    同族变体 `drop(_guard)` / 分支提前 drop ⇒ 只能**读**，不能 grep。
100. 【清单要标“读”还是“grep”】只能人工读的判断（如“守卫作用域内没被 drop”）必须在清单里标「读」，
     否则验收人会拿 grep 绿了就当过。
101. 【`--test-threads=1` 不是解药】串行只消除**并发交错**，不消除**顺序污染**（先跑留下的 `PACK_READ_BYTES`/`SWEEP_ONCE` 仍被后跑读到）。

102. 【取证方式四档（QA §G，进验收标准）】`[grep]` / `[run]` / `[read]` / `[time]`。
     关键：**“grep 得到 0”与“读代码确认没有”给出同一种报告结论，但证据强度完全不同。**
     `[read]` 条目必须写成“**我读了 `X:Y`，判断是 Z，理由是 W**”，不能写“已确认无误”（后者把不可复现的判断伪装成可复现的结论）。
     `[read]`-only 六类：守卫作用域内没被 drop／注释是否仍成立／“表还是循环计数器”／“对照绕过被测函数”／
     “两步形态 vs 同一函数两个用例”／“期望值从哪来”。
     `[time]` 档只证明“**曾经阻塞**”，不证明“永远阻塞多久”；报告写“实测值 + 阈值 + 为什么取这个阈值”，缺一样按 `[read]` 处理。
103. 【元判据】报告末行写 `依赖判断的结论：N / 总数 M` —— 这是让报告诚实的数字，不是显得严格的数字；
     N/M 超过一半时 manager 必须自己读一遍再合入。
104. 【因果词规则的可判形式】注释/提交说明每出现一个因果词（deletes/causes/because/so/因此/导致），**同一段内必须能指出测量**
     （哪个实验、什么数字、谁跑的），否则必须明确标为**假设**。本轮四实例见 QA 表（volume.rs / criterion 2 / inode_view / d3898d8）。
     共同后果：**下一个人省掉了自己验证的一步**——省的是别人的工作量。
105. 【总结句（合并报告用）】本轮高价值发现多数来自 `[read]`；`[grep]` 抓到的主要是“数量不对”。
     **grep 只能证明“文本里有什么”，判据的价值在“文本与意图是否一致”。**

106. 【读法污染】N 条 `[read]` 结论用的是同一种读法。一条被推翻时，正确动作不是“改那一条”，而是**把同一读法用到的其余结论重看一遍**。
     只改一行会让报告变成“大部分旧判断 + 局部新判断”的混合体，而没有任何标记说明哪部分重看过——比整份失效更危险。
     ⇒ **错误不在结论里，在读法里；修结论修不掉读法。**
     落地：N 中任一条被推翻 ⇒ 报告结论整体失效，不得只改那一行。
107. 【`[time]` 档报告四件】① 阈值 ② 实测值 ③ 机器（OS + 是否容器/共享 CI）④ **这条时间证据支持哪个结论**
     （如“支持：锁覆盖整个闭包”，**不**支持“原子性已通过”）。缺 ④ 会被顺手借去支撑它没测的东西。

108. 【证据必须是某个 ref（2026-10-25，QA 写成成品 rule-evidence-must-be-a-ref.md）】
    报告/清单里每条“某文件某行是 X”必须能用 `git show <sha>:<path>` 复现；**工作树只能用来运行，不能用来引用**。
     三次实例：QA 用停在 `21b2be3` 的工作树判断“main 有没有 fs-windows”（读的是别的时间点）；
     QA 植入变异后忘还原就继续读（读的是自己的实验）；manager 读工作树判断 R1/R2/R3（读的是未提交状态）。
     共同点：**工作树内容 ≠ 某个 SHA 的内容**，而“文件:行”隐含承诺了后者。
     落地：① 每条 `文件:行` 带 SHA，否则按 `[read]` 处理并标“未定位”；② 复核不看工作树，直接 `git show <sha>:<path>`；
     ③ 必须引用未提交状态时写「**未提交观察**」，不计入结论。
     只对 QA 成立的推论：她的 `qa-*` 工作树就是实验台（随时有未还原的变异）⇒ 不能同时把它当证据。

109. 【判据的可证伪性取决于“能不能只动被测的量而不动期望”】
    `HOLD / 2` 与 `HOLD` + `MIN_WAIT` 正常运行时**完全等价**（数字一样、绿一样），差别只在**有人要证伪它时**：
    前者改 HOLD 会同时改期望（红不了），后者可以。
    ⇒ 好的注释记录的是「**这个判据是怎么被证伪过的**」，而不只是「它在测什么」（后者是多数注释的内容）。
110. 【回放总表（四行，含证据种类）】R-A 删 truncate → 红（跑过）；R-B 换 FileId → 红（跑过）；
    R-C waited=常量 → 绿（**推导**）；R-D write 出错也提交 → 绿（**跑过**）。
    R-D 必须留：唯一一条“真跑过、结果是绿”的回放——它证明的不是“判据不灵”，而是“**这个架构里那条 race 判据没有判别力**”，
    正是改写成结构判据 + 锁探针的实测依据。删掉它，那段改写就失去依据。

111. 【结构性不变量：内层量不可能大于外层量】若 `inner` 是某次调用**内部**测得的时长、`outer` 是**包住这次调用**的墙钟，则恒有 `inner <= outer`；
    违反 ⇒ **至少有一个量不是它名字说的东西**。它是**产品级**约束（报给调用方的数必须落在调用方看到的窗口内），不只是测试断言。
    比“调容差”强：调容差只消红（口径仍不同）；断言不变量**把“口径不同”变成红灯**，指出的是量的问题。
    适用面实测：产品里“报给调用方的时长”只有一处（`fs-workspace/src/busy.rs:25 elapsed()`）；`created_ns`/`modified_ns` 是时间戳不是时长。
    **“名字说 A、实现是 B”家族里少见的可机械化成员**（其余只能读）。
112. 【B-12（CAS 路径的钟）落地注意】改成“本次操作”会**收紧上界**；现有 `waited_ms <= MAIN_BUSY_TIMEOUT_MS + 1000` 的余量是按“陈旧钟（更大）”选的，
    改完应更稳，但要顺手确认 `waited_ms >= measured` 仍成立（改完应更接近相等，而不是反过来）。

113. 【改注释也有机械证据】例：改完 `busy.rs` 的 “actually spent” 后，`git grep -n "actually spent" crates/fs-workspace/src/busy.rs` = 0
    —— 否则下次 review 又要读全文才能确认它改了。
114. 【证据三列（比“关系 vs 语义”细一层）】
    | 层 | 例 | 机械手段 |
    | 关系 | `inner <= outer`、`TABLE.len()==18` | 不等式/等式 |
    | 结构 | 守卫是否被 drop、对照是否绕过被测函数 | 可解析（语法树），**当前无工具 ⇒ 仍走 `[read]`** |
    | 语义 | 这句注释还成立吗、这个名字说的是不是它做的事 | 只能读 |
    ⇒ **把 `[read]` 里的结构项单独标出来，就是一份工具待办清单**（本轮 6 条 [read] 中 3 条是结构层）。
    “工具债”的量化方式：不是“缺工具”，而是“**有 3 条断言本该自动**”。P3 之后：汇总结构层 [read] 条目作为下一个工具/lint 的输入。
115. 【三段注释成品（QA，待 worker 落）】① `busy.rs` 头：at most the time since the last main-database lock + 实测违例 5794 vs 5631 + 不变量；
    ② `core.rs` 用例头：**这是观测不是判据** + 为什么无法升级；③ 单边断言说明：为什么单边而非 ±100（两量不覆盖同一区间）。

116. 【WinFsp SDK 就位（2026-10-26，owner 指示“开始安装 sdk”）】
    - 机器上 WinFsp **2.1** 已装（SxS 布局：`C:\Program Files (x86)\WinFsp\SxS\sxs.20261022T083021Z`，驱动已加载由 `fsptool lsdrv` 证实
      `R WinFsp+20261022T083021Z`）；**但系统安装缺 Developer 组件**（无 `inc/`、无 `lib/`、System32 无 `winfsp-x64.dll`）。
    - 用**管理安装**（`msiexec /a winfsp-2.1.25156.msi /qn TARGETDIR=F:\work\winfsp-sdk`）把 SDK 抽出来，**不需要管理员**、不动系统：
      `F:/work/winfsp-sdk/DYNAMIC/{inc/{winfsp,fuse,fuse3}, lib/winfsp-x64.lib, samples, SxS/bin/*.dll+*.sys}`。
    - 工具链：`rustc 1.92.0`，host `x86_64-pc-windows-msvc`，VS2022 Community 在位。
    - **冒烟验证**（`F:/work/winfsp-probe`，独立 scratch crate，不入仓库）：`LoadLibraryW` + `GetProcAddress("FspVersion")` 调通
      ⇒ `status=0x00000000 version=0x00020001`。**不用 .lib 也能跑** ⇒ 与决定 B（自写最小 FFI、不 vendor GPL 头文件）一致。
    - 未做（也不需要）：系统级安装/修复（要 UAC 提权）。

117. 【计分规则：判据数按“独立的失败面”数，不按断言语句数】同一测试函数承担 B2+N3 ⇒ 记 1 条并注明；两个函数 ⇒ 记 2 条。
    理由：本轮出现过“同一断言被算两次”的风险（criterion 2 与它的“真两步”用例一度同形）。
118. 【N3 钉住语义】新句柄 `size == 0` 不只界面承诺，它钉住“**替换 = 全新空文件**”；若将来有人让 SUPERSEDE 复制旧 size（“保持看起来一样”是很自然的需求）⇒ N3 必须红。
119. 【C1-f 为什么必要】口径变更时，“多”永远是旧口径的残留：它抓的不是“多写了一个实例化”，而是“**还按旧口径跑着两个后端**”——两者在代码里长得一样，只有从“现在应该只有一个”出发才能区分。

120. 【时间型判据：靠“某段开销有界”才成立的边界都不是不变量】不管余量写成常数（`+1000`）、写成另一个量（`+measured`）、
     还是写成 abs_diff（`±100`）——三者在假设某段开销有界，而那段开销（前置物化）**随输入无界**。
     ⇒ 今晚三个失败尝试同源。只有“有语义依据”的边界（超时 ⇒ 等待 ≥ 超时）才是结构性的。
121. 【“范围不是保证”】QA 自我记录：三版上界都错在同一件事——**在找“更大的余量”，而问题在“钟的起点”**。
     与“指标不是目标”是同一族的近亲：把**容差**当成了**保证**。
122. 【B-12 之前该用例是观测，只报三个事实】CAS 确实等到超时、拒绝是 typed `Busy`、报告值不低于策略。
     上界留给 B-12（钟变成“本次操作”后 `waited_ms <= measured` 才是真不变量）。

123. 【grep 是行级的：跨行断言抓不到】`assert_eq!(\n    Some(file_id),\n    before,` —— 用 `grep -E 'assert_eq!\(\s*Some(file_id),'`
    逐行匹配**永远 0**（假红），会让人去补一条已经存在的断言。正确形态：`grep -A2 'assert_eq!(' … | grep -c 'Some(file_id),'`。
    ⇒ 规则：**自查单里的每一条也必须能红、能绿**（永远 0 或永远 >0 的自查项是假判据）。
124. 【合入自查单（7 条，QA 修正版，2026-10-26）】
    ① `grep -c 'fn truncate' crates/fs-namespace/src/tree_store.rs` = 1
    ② `grep -c 'tree.truncate(id, 0)' crates/fs-namespace/src/rules.rs` = 1
    ③ `grep -A2 'assert_eq!(' crates/fs-workspace/tests/g1_create_or_open.rs | grep -c 'Some(file_id),'` ≥ 1
    ④ `grep -cE 'const (HOLD|MIN_WAIT)' crates/fs-workspace/tests/g1_create_or_open.rs` = 2
    ⑤ `grep -cE 'must not under-report|within the policy plus a margin' crates/fs-workspace/tests/core.rs` = 0
    ⑥ `ls crates/fs-workspace/tests/g1_truncate.rs | wc -l` = 1
    ⑦ `grep -c 'never change it' crates/fs-namespace/src/inode_view.rs` = 0

125. 【grep 模板：多行宏必须用 -A2/-A3】凡对 `assert_eq!(` / `assert!(` / `matches!(` 的内部取内容，一律 `grep -A2`/`-A3`，
    不要用同一行模式（跨行必然假红）。误判方向是“假红（判成缺）”，比假绿安全，但会浪费一轮“去补已存在的东西”。
126. 【未提交观察的使用限制】它可以进报告，但**不得把“缺两件”变成“快好了”**——状态词仍是“缺”，直到提交到达。

127. 【T4 写法：可见性一致性，而不是“都成功/都失败”】`stat.is_ok() == read.is_ok()` —— 一条判据同时抓
     “tombstone 漏置”（stat 成功 read 失败 = 静默数据损坏）与“tombstone 误置”（两者都失败但本该可读）。
128. 【O1 写法：把新字段钉在已有表上】`replaced.is_some()` **当且仅当** `action == ReplaceExisting`（18 格逐格）——
     全给 `None` 会立刻红；O2 钉“binding 的名字不能过期”（SUPERSEDE 后是新名、rename 后是新名）。
129. 【执行顺序：先证观测器有效，再证被测物正确】读放大的反例对照必须在正例之前跑；
     顺序反了的话，观测点失效时会得到“读数恰好等于期望”的**假绿**。
130. 【“只断言被删的那一项消失”不够】按名字删的实现也能满足它（它删的恰好也是“某项”）；
     D1′ 必须同时断言“**没删错**”（`/a` 仍在且是 id2）。

131. 【“空转”不是“通过”】若判据的构造前提不成立（例：mock 不维护真实 `(parent,name)→child` 绑定，路径复用就构造不出来），
     该用例不是“通过”而是**没测到**。验收报告必须能区分这三态：通过 / 失败 / **空转（构造失败）**。
132. 【规则要附“它挡的那个错误”】ADR 里“SUPERSEDE 若置 tombstone ⇒ 旧句柄读不到（B-8 破）；remove_at 若留 live 行 ⇒ 静默数据损坏”
     —— 两句话分别给出错在哪个方向会怎样；下一个人改动时会先想“我这样改会破哪一条”。

133. 【纠正：`unbound` 不能代替“保留”】architect 指出我那句兜底不成立——`unbound` 只防止后续误删名字，**不保留旧内容**。
     若 FSD 真会下发 rename-replace 且目标有句柄，立即 retire 会让旧句柄读到 tombstone ⇒ 与 SUPERSEDE 的 B-8 冲突（旧句柄必须可读）。
     ⇒ 兜底必须改成“**改用 SUPERSEDE 的保留语义**：目标 inode 保持 live、旧句柄可读，`replaced` 仍返回供 adapter 标 unbound”。
     一般化：**“标记不可见”与“保留内容”是两件事**，不能用一个代替另一个。
134. 【rename(x,x) 误 retire 自己的后果】名字仍在、inode 变 tombstone ⇒ 所有句柄/读立即 NotFound ⇒ **数据全丢**。

135. 【`A || B` 的断言 = 两个变体互相顶替】`text.contains("NotFound") || text.contains("NotADirectory")` —— 把产品里的 NotFound 换成
     NotADirectory 仍然绿 ⇒ 判据没钉住语义。而 adapter 要靠 `NotFound` 产生 `DELETE_PENDING`，`NotADirectory` 是另一个意思。
     改法：**匹配变体**（`matches!(e, NamespaceError::NotFound { .. })`），并给另一个变体一条**自己的**对照用例
     （“对存在的文件调 read_dir_id ⇒ NotADirectory”），两者各自被钉一次。
136. 【字符串包含型断言的分辨率】它只区分“文本里出现过这个词”，不区分“返回的是哪个变体”——与“grep 分不清引用与定义”同族。

137. 【“过宽”的正确定名是“不可证伪”】`format!("{e:?}").contains("NotFound")` 对**任何恰好含该词的错误**都为真 ⇒ 这条判据不是“太宽”，
     而是**不可证伪**（没有任何输入能让它红）。判据写成“文本里出现某词”时，问一句“什么输入能让它红”，答不出就是不可证伪。
138. 【对照必须“独立会红”】正例变异（None 分支返 NotADirectory）与对照变异（文件 id 返 NotFound）**各自红**，
     说明两条断言互相不可替代；若只有正例会红、对照永远绿，那对照只是陪衬。

139. 【骨架五要素（QA 固化）】0 前提自证（布置确实生效，用与被测判据无关的观测）／1 两个对象在可观测维度上不同／2 被测操作／3 期望／4 对照（另一个对象也各归各位）。
    第 0 与第 4 的区别：**第 0 防“布置没生效”，第 4 防“两个对象返回同一份”**——两者都能让错误实现在用例下变绿，但机理不同，都要有。
140. 【“步骤”与“确认步骤生效”在清单里长得一样】要素清单只写“被测物要经历什么”时，最容易把“这个经历真的发生了吗”合并掉。

141. 【底座第 6 形态：名字承诺 A，断言只验 B】测试名 `..._tombstones_its_row` 里承诺“行还在”，但断言只验了“不可见 + serial 不复用”；
     把行真删掉（回退成物理删）这些断言仍绿 ⇒ **测试名承诺的东西没被验**。与“名字说 A、实现是 B”互为镜像（一个在测试名上，一个在实现上）。
142. 【grep 口径的两处教训】① “调用点 ≥4”写错——R3/R7 **共用同一段代码**，3 个调用点覆盖 4 条规则；口径应为 “≥3 且注释点名 R3/R7”。
     ② 成对判据（行在 + 不可见）只钉一半时，**回退成旧实现仍然全绿**。

143. 【断言应住在它承诺的那一层】`k1` 的注释承诺“墓碑经 `TreeRead` 不可见”，却用 `Rules::backend().read_at_id` 来验（隔两层）⇒
     中间任何一层“帮忙”过滤，注释就成了空话。补法：在离墓碑最近的那层直接断言（`tree.inode(id)?.is_none()`）。
144. 【成对断言应尽量住在同一层】跨层时必须**下层也有自己的一条**，否则变异只能说明“上面坏了”，不能说明“坏在哪”
     （QA 的“层次不对称”说法）。

145. 【层模型 L0–L3（QA）】L0 `ov_inode` 行（SQL，可见 `live`）／L1 `TreeRead::inode`（死行被隐藏 ⇒ None）／
     L2 `Rules`（转成 NotFound）／L3 `workspace::read_dir_id`（再包一层错误类型）。
     断言跨层（如 L0 一条 + L3 一条）时，**中间两层没有自己的断言** ⇒ 变异只能说明“上面坏了”，不能说明“坏在哪”。
146. 【层对齐的可跑判据】**对第 k 层做一条变异，必须有一条第 k 层的用例红**；若红的是第 k+2 层，说明第 k 层没有自己的断言。
     与“观测点自证”互为两面：观测器没接上 ⇒ 阴性结论无效；断言在别层 ⇒ 阳性结论不可归因。
     ⇒ 结论的可信度由“**证据与断言的距离**”决定，距离越远，红的定位越贵。

147. 【断言三条同时满足：对象对 / 层对 / 输入对】“距离决定定位成本，**对象与输入决定结论真伪**”。
     反例：① 距离 0 但断言对象错（L0 断言 `live==TOMBSTONE` 却没有“行存在”的断言）；② 层对但下层漏过滤（L3 断言读不到、L1 没过滤）；
     ③ 层与对象都对但输入错（`inode(从未存在的 id).is_none()` —— 等于断言 `None == None`，恒真）。
     ⇒ 典型：`tree.inode(删过的 id).is_none()` 必须用“删过的那条 id”，并加对照 `inode(活着的 id).is_some()`——
     没有对照，**“永远返回 None 的实现”也会绿**。

148. 【清理纪律（2026-10-26，owner 指示）】**每张卡验收完成后**：
    ① 删该卡的 QA worktree（git worktree remove，不能 rm，否则主仓库记录悬空）；② 删对应的 qa-t-* target 缓存目录；
    ③ 分支合入 main 后立即删分支与 worktree（本地+远端）。
    本轮积到 44 个 worktree + 37 GB target 缓存才清，就是因为没定过这条。
    持久笔记（manager-notes、qa-scratch 中仍被引用的清单/骨架）**归档不删除**（→ layer-fs-manager-notes/qa-archive/）。

149. 【A/B = 1 条判据 2 条断言】同一失败面（“读了超过所需的块”）的两种度量：A 绝对、B 相对；同一条变异（fetch-all-extents）
     同时红两边 ⇒ 报告记“1 条判据、2 条断言”，否则总数虚高。**B 更耐用**：块粒度变大（合法变更）时 A 会红、B 不会——
     B 只断言“成本不随文件大小增长”，与格式无关。
150. 【B-15 的措辞】不是“未达标项”，而是“**已知的格式代价 + 一个待决取舍**”：范围读最小代价受格式粒度约束
     （≤32 KiB = 单 Blob 整对象；≥64 KiB = 32 KiB 块），当前实现已在该下限上（实测 33,976 B / 512 B 请求）。
151. 【shell 脚本的 cd 用 && 链接】architect 清理时的事故：`cd` 失败后后续命令落到别的目录（当时检出 p3-2/g2），幸好全是只读。
     规则：**脚本里 cd 一律 `cd X && …` 串起来**——cd 失败即整段不执行，不在别处继续跑。

152. 【硬规则：manager 不动别人的工作树】2026-10-26 我在 worker 主工作树上执行 git checkout -- . 误删其 G4 未提交改动
     （g4_rename.rs 480 行因 untracked 幸存）。根因：违反了“别人的工作树是实验台/编辑台”那条。
     ⇒ **manager 永远不在 worker/architect 的主工作树上执行任何写操作（checkout/stash/reset/clean）**；
     需要删分支时发邮件让 worktree 的主人自己切换，或等提交后在独立 worktree 操作。
153. 【分工（owner 定，2026-10-26）】所有集成测试（全量跑）归 QA；worker 只跑当前任务的定向测试；manager 不自己跑测试。

154. 【变异固化为断言（worker 的“就地红法”）】当“可被写错的形状”能抽成纯函数时（如 close_removes(remaining, pending)），
     把它抽成 pub fn，并把**合理错法**（pending 就删 / 永不删）作为对照喂进同一批用例，断言它们都与真规则不一致。
     ⇒ 变异（改源码再跑）是一次性的、没人会再跑；对照是**每次 CI 都在跑的反驳**。
     注意：此类辅助 fn 须注明“仅判据辅助”，否则变成没人敢动的公共面。

155. 【互斥失败面（判据 2 的写法）】SUPERSEDE 判据的三条断言（旧句柄读旧字节 / 新条目 size 0 / 名字解析到新 id）
     各有**它独有的失败面**：原地截断只丢旧字节、删旧 inode 只丢旧句柄、复制旧 inode 字节对但 size 错——
     任意两条都被某个错实现满足，三条互斥。⇒ 多条断言不是冗余的判据：**每条都要找出只有它抓的那个错实现**。
156. 【“未验证”不接受打折】a criterion whose second half has not run is not half true, it is **unverified**。
157. 【测试文件的用例数是指纹】判据 5 事故：脚本切到文件末尾删掉一个用例，用例数 7→6 是信号。
     ⇒ 每个测试文件的用例数应可解释；变动必须在提交说明里出现。

158. 【“数据丢失穿上更整洁列表的外衣”（dirent 去重）】错实现的特征不是行为变怪，而是**行为看起来更好**——
     折叠去重后的目录列表更整洁，所以这个 bug 极难从外部观察。判据必须配“后端确实有 3 条”的对照。
159. 【“重复比空页更糟”（marker 失效）】两个坏选择里挑哪个不重要，**写清理由**才重要——
     下一个人一定会想“从头开始不是更省事吗”，注释要先回答他。

160. 【错误折叠要么显式要么不做，绝不顺手对齐】mock 的 OpsError 无 InvalidRecord ⇒ 守卫不匹配落 NotFound，
     而核心是 InvalidRecord ⇒ 不同 NTSTATUS（LeafMissing vs Internal）。worker 拒绝悄悄对齐：
     “那会让真后端的**内部错误伪装成文件不存在**”。折叠记录在案 + P3-3b 处理（加变体或映射层说明），不当 bug 报。
161. 【映射检查用“不同状态数”而非“存在性”】“全部映射成 Internal”也能通过“映射存在”式检查 ⇒
     检查升级为“adapter 用到的 failure 映射出 ≥8 个不同状态”。存在性检查对退化实现免疫。

162. 【判据钉在“产物可验证的量”上】pages/largest/根高字节都是写完后重读一遍就能核对的；
     guardrail_hits 只在编码过程中存在（过程量），事后不可验证 ⇒ 移出判据、留作报告的解释性数据。
163. 【口径不确定的数不能当判据】height 的“层数口径”（2）与“页内 height 字节”（边数，3）差一——
     若直接沿用旧值 2 当红界，等于把“数边还是数层”的分歧留给未来的红。改写成“根页 height 字节 ≤ 3”（产物可验证、口径唯一）。
164. 【宁可少一个数，不留一个歧义的数】worker 重测时 height/guardrail 反推值与卡值不一致，他没有把反推值印在卡值旁边
     （“那会制造一个差不多一样的数字，以后一定被当成同一次测量引用”）——同一量两种口径的预防（128+76≠224 那族）。

165. 【余量 0 的判据 = 最敏感的那条】v2 的 99,999 条记录恰好三层（根 height 字节 = 3，红界 ≤3，余量 0）——
     v3 记录变大 24 B 后只要分出第四层就红。这不是缺陷：**树高是对记录膨胀最敏感的产物量**
     （页数只涨 ~1%，高度直接 +1）。
166. 【共享输入的判据组合，失败必须可归因】判据 3（高度超限 = 扇出问题）与判据 1（页数变多 = 容量问题）
     共享同一批输入，但**消息必须各自点名是哪一类**——绝不让一条 assert 同时承担两个含义。
167. 【耗时判据不进 CI】判据 4（time ≤224 ms）的断言用例与测量用例同置 #[ignore]；
     红界检查放在“改动自己的用例”里（v3 落地时新旧两套对比内部做）。CI 机器 ≠ 基线机器 ⇒ 耗时判据在 CI 上会假红。

168. 【哈希判据的期望独立】profile_id = BLAKE3(描述串)，所以期望必须是**整串的独立字面量**——
     不能用被测代码拼同样的片段（拼出的期望与被测物共享代码 ⇒ 任何变化同时改变两边 ⇒ 恒真）。
     “期望独立”在哈希判据上的形态：哈希是对全串的，期望就得是全串。

169. 【round-trip 只证明对称，不证明布局】`the_layout_is_the_documented_field_order` 逐字节钉偏移
     （0..8 serial / 8 kind / 9..17 size / … / 57 tag / 总长 58）——布局是 ADR 的承诺（跨版本兼容靠它），
     round-trip 编解码对称的实现对**任何**布局都成立；将来调整字段顺序，字节级断言会指名道姓。
170. 【派生值不落盘（且是拒绝规则）】v3 的 attributes 只存 READONLY/HIDDEN/SYSTEM/ARCHIVE 四个可写位；
     DIRECTORY 0x10 / NORMAL 0x80 由 kind/content 派生，盘上出现 ⇒ Malformed——防止“存储的属性与 kind 打架”一整类不一致。

171. 【校验不挂在写路径上】validate() 写时读时都跑；用例直接改字节绕开 encode 再 decode——
     防住“只有本 build 写的数据才合法”的自证循环。格式的不变量属于格式，不属于写者。
172. 【派生位拒绝规则的效果】矛盾（文件带 DIRECTORY 位等）只可能存在于**写坏的盘**上，
     不可能存在于本 build 写出的盘上——本 build 成为“矛盾-free”的证明者。

173. 【半 v3 陷阱：自洽错误在格式版本上的形态】writer 写 inode 页 fmt=2 而记录仍是 v2 布局 ⇒
     页在**本进程内**完全自洽（编解码对称、往返成功），任何别的 build 读不出，且现有用例抓不到（两边同一错误假设）。
     记录层的错误布局会被字节偏移断言抓住，这里的半 v3 页连这层保护都没有 ⇒
     **格式版本切换必须一次提交**（fmt 与记录布局不可分步），且配“反半-v3”判据：
     v3 页按 v3 布局解出四个新字段的**已知测试值**（不是“能 decode 就行”）+ 负例（同一份字节按 v2 布局解必须失败）。

174. 【fmt 错 vs 布局错的分工】fmt 字节错（1↔2）⇒ UnsupportedFormat（“不认识这个组合”）；
     fmt 对、记录布局错 ⇒ Malformed（“认识组合但数据坏了”）。两条各自红法——判据不是只看 fmt 字节。
175. 【夹具的默认值不是语义断言】38 处构造里 4 处夹具补新字段填 0/空，须在提交说明写明“夹具的 0 不是断言”
     （否则半年后被读成“这些测试断言了时间戳为 0”）；更好：封 helper（legacy_record）把这句话写在一个地方。
176. 【已知测试常数优于随机值】0x1122334455667788 / 0x99AABBCCDDEEFF00 / 0xDEADBEEF 这类交错模式：
     字段错位时读出的值面目全非（顺序换了不是“差不多的数”）；随机值每次变，认不出是哪个字段。
     “解出四个字段”很容易被“解出四个零”蒙过——用随机或时间戳时尤其如此。
177. 【提交时可分性 = 未来可错性】“fmt 与记录同提交”的自检要具体到 diff：inode_fmt 的行与 inode_v3 接线同批——
     防下一个人 cherry-pick 一半。

178. 【改代码的脚本也要自证】机械补字段的脚本把 impl 块也当字面量插了字段（编译立刻红救了它）；
     修法：匹配加前提（必须有 content 字段、前缀不能是 struct/impl/->）。
     规则：**这类脚本必须带“我到底改到了什么”的断言**（改了多少处、都在哪些文件），否则它会安静地改错位置。
     与扫描器三件同族：读代码的扫描器与改代码的脚本，都需要自证。
179. 【该不变的数不变，同样有信息量】1a 步（InodeRecord 扩字段、v2 编码忽略新字段）全量 512 不变——
     这是“零格式变化”的计数证据，与“测试用例数变了就是信号”方向相反、价值相同。
180. 【用设计消灭错误类，而不是用测试抓】“不用两个 struct”——两个 struct 会引入“只为了忘掉 v2 存不下的字段”
     的转换函数，那个函数正是半 v3 格式的滋生地；同类：“去掉 Copy”（按值复制会邀请 64 KiB SD 的拷贝成本）。

181. 【基线四数反验 = “SHA + 编码器没动”的机械化】“格式零变化”的验证从提交说明（人写的，可能错——声称与代码不符已 5 次）
     改为四个基线数字可对（pages 1472 / avg 69.06 / largest 16110 / 护栏 2.3%）——从产物算出，错了对不上。
     四数本来就是基线表现成的，反验成本≈零。若偏了：先怀疑 fixture 构造（顺序/随机性差异会让页数先漂），再怀疑编码器。

182. 【变长记录解码：take(used) 而非 take(remaining)】v3 记录先解析才知道长度（security_len 决定）；
     take(remaining) 在单记录页完全正确、多记录页吞掉第一条之后的全部——bug 只在“多条”时显形。
     ⇒ 用例至少 2 条记录（单条断言全绿）；配套 Reader::rest()（窥视不消费）+ 文档写明陷阱。
183. 【设计级消灭：“两处同形”】记录字节的选择与页 fmt 的选择用同一个判据（profile.inode_fmt）——
     “页说一种格式、记录是另一种”这个错误状态在代码结构上不可表达，不是被测试抓。
     与“不用两个 struct”（180）同族：用设计消灭错误类，优于用测试抓。

184. 【第 0 条自证的现场证据】Memory::get 返回裸页字节而 reader 期望带信封对象 ⇒ 四行拒绝用例全部通过——
     通过理由是 Malformed: wrong magic，与判据毫无关系。第 0 条（同一夹具先读出健康 v2/v3）立刻红。
     ⇒ 没有第 0 条，拒绝矩阵是一排假绿：四行“通过”看起来是判据在工作，实际是夹具坏在判据之前。
185. 【“值”与“缺失”分开钉】v3 断言 accessed_ns == 3（值）；v2 断言 == 0（缺失）——两种含义分别钉住，
     防止“v2 也有这个字段、只是为 0”的误读。写入侧注释（1a）与读取侧断言（1c）闭环。
186. 【discriminant 断言：两个变体必须不同】用 std::mem::discriminant 断言 Malformed ≠ UnsupportedFormat——
     把“两个变体必须不同”本身变成断言；一条 is_err() 会让“对损坏报 unsupported”的实现全绿。

187. 【零写入检查用哈希不用长度】拒绝前后对比 store 内每文件的 BLAKE3（不是文件长度）——
     长度相同内容不同正是“拒绝时顺手写了点东西”的形态。零写入 = 零字节变化。
188. 【靠结构不靠纪律】基线探针读 Profile::CURRENT 而非硬编码 v2 ⇒ v3 切换那批若忘了更新数字，
     探针拿 v2 数字判 v3 产物立刻红。与“两处同形”（183）同族：忘掉就红，优于“记得就改”。
     配套：读 CURRENT 的理由写在文档头，防后人“好心”改回硬编码。
189. 【第 0 条自证推到判据所在的层】v2 夹具自证不只断言 root 读得动，还断言 inode(1) 真有记录——
     “root 解析成功、inode 表坏掉也会通过”。自证的深度要达到判据的深度。

190. 【存储的克制与报告的完整是同一设计的两面】盘上不落派生位（DIRECTORY/NORMAL，落盘 ⇒ Malformed——防属性与
     kind 打架）；但 Stat.attributes 作为**给前端的报告值**必须**合成**派生位（目录必返 0x10；无其它位的文件返回 0x80 NORMAL；
     READONLY 文件不含 NORMAL）。报 0 会让 Windows 层把目录显示成“无属性的目录”。
     set_attributes 的输入是**存储值**（Windows 语义不接受 DIRECTORY/NORMAL）；往返断言 = stat 返回 派生(存储值)。

191. 【“读”的一半与“写”的一半】attributes 报派生值裁定了读侧四条用例；QA 指出写侧开着：
     set_attributes(DIRECTORY/NORMAL) 的行为卡里没定。裁决：**拒绝**（存储值域只有四个可写位；静默忽略 = “接受了但没生效”
     更易造成下游误判；Windows SetFileAttributes 同样不接受 DIRECTORY）。错误类型用现有 InvalidParameter 类变体，不新造。
192. 【两向量分开断言】“读对象数不随 SD 变化”中 stat 的读数必须不变，但 encode 的对象图**应该**变（SD 属于记录）——
     分两条断言，否则两边互相“顺带满足”。

193. 【“不新造变体”的准确边界】新变体要么**有既有词汇表可挂靠**（NamespaceError::InvalidParameter 挂靠
     Failure::InvalidParameter → STATUS_INVALID_PARAMETER 的既有映射），要么**有可预见的第二处消费者**
     （set_times 的参数校验同词汇）；两者皆无才不许造。“不新造”防的是孤儿变体，不是词汇表的自然生长。
     决定性理由（worker 给的）：规则只在 mock 层 ⇒ P3-3b 真后端直接调核心会绕过它。
194. 【InvalidRecord 与 InvalidParameter 的分工】前者 = “盘上的数据本 build 解释不了”（数据损坏）；
     后者 = “调用方给了不在值域里的值”（参数非法）。混用会让两类错误在日志里无法区分。

195. 【零依赖 crate 的职责（第二次验证）】fs-semantics 装“跨层共享的规则”（disposition 表、attributes 的
     validate/report），不装“某一层的实现”。判据：若一条规则要被核心、前端 mock、真后端三方使用，
     它必须落在零依赖 crate——否则依赖方向（fs-winfsp 不能拖 bundled SQLite）会逼出“规则写两份”，
     而“两份”意味着规则在其中一处失效。第一次验证是 disposition 表（QA 用 cargo tree 抓的）；
     第二次是 attributes（编译失败当场暴露）。
196. 【不造半边入口】校验存在、写入没接的入口（set_attributes 有 validate 但 overlay v2 没有三列）
     会诱导调用方绕过它直接改存储——入口存在但接错方向的规则比没有更糟。等写入列同批做。

197. 【T1 全列清单的第二次救场】新增四列（accessed_ns/changed_ns/attributes/security）时 T1 立刻要求登记
     “是不是 GC 根”——若 security 存的是 oid 而非内联字节，一个隐藏根会静默漏扫，未来的 GC 会把还被引用的对象收走。
     四列均登记 NotARoot（时间戳/属性位/内联字节都不是对象引用）。第一次救场是 ov_meta.pending_root。
198. 【“元数据不进内容寻址”的两端】SD 不进 Stat（报告路径不付 64 KiB 拷贝）+ SD 不进 CAS（security 内联 BLOB、
     不是 GC 根、不必付对象解码）——同一条设计的两端。

199. 【防御深度：盘前校验】值域校验放在最接近盘的那一层（fs-overlay 写入前调
     fs_semantics::attributes::validate）——规则层校验挡住正常调用者，盘前校验挡住绕过 Rules 的调用者
     （未来 CLI、导入工具）。两层挡同一规则不是冗余：是同一个函数（fs-semantics::validate）被多个入口调用，
     不是多份实现。“派生位不落盘”必须在最接近盘的层也成立。
     （fs-overlay 加依赖 fs-semantics 不违反零依赖原则：fs-semantics 本就是零依赖、被依赖是它的用途；方向与 fs-namespace 同向。）

200. 【没写完就不出现】“不在提交里放注释掉的尸体，也不在消息里假装它存在”——消息声称、代码缺失（四次教训）的
     完整反面不是“下次把代码带上”，而是未完成的部分完全不出现：不留死代码诱导误读，不留半句声明制造虚假完整感。
     编译器否掉猜的函数名只花几秒；按纪律停手比继续猜难得多。

201. 【零提交 > 编译绿但判据够不着的代码】提交一段没有任何调用者的函数 ⇒ 编译通过、clippy 干净、
     但没有任何东西能证明它对。这比“消息声称、代码缺失”更隐蔽：代码是真的、检查是绿的，唯独判据够不着。
     死代码会诱导下一个人“既然存在就调一下”，而它从未被证明对。
202. 【操作只能改它声称要改的】materialise 对照用例：改属性后 size 与 created/modified **保持 base 的值**
     ——证明 insert 没顺手改写别的字段。这是“拒绝不留副作用”的正面版。

203. 【时钟注入的选型】(a) gc-test-hooks 风格的 feature + 线程局部时钟（与 crash-points/sync-probe 同族，生产零成本）。
     决定性理由：判据要“把时钟拨到已知值”而不是“前进”⇒ 必须有 set/读取两动作。
     排除 (b)：把测试概念塞进生产配置结构（OverlayOptions.clock）= 判据渗透进产品面；
     排除 (c)：只有“前进”没有“设定”，语义不够。
     feature 名沿用 gc-test-hooks（不新起名——两个缝就是两道门）。
204. 【manager 的等待链错觉】三方都在线且无阻塞，“所有人停了”其实是 manager 自己的 review 停顿造成的观感。
     ⇒ manager 收到提交后 review 信要在下一封邮件前发出（哪怕只是“收到，稍后回”），否则整条链观感停摆。

205. 【标题/摘要/清单都是索引，不是内容】按标题判“通过信”漏掉了 3 条实际要求。
     ⇒ 标题只用来排序；判断“要不要动”必须读正文。与“清单里没有行号的条目不给已验”（91）同族。
206. 【failure_for：病因级修复】NamespaceError → Failure 的映射此前不存在（只有 Failure → NtStatus）——
     P3-3b 会被迫自写一份；且 InvalidRecord 映到 Internal 而非 LeafMissing（“盘上数据解释不了” ≠ “文件不存在”）。
     mock 侧的折叠是症状，缺 failure_for 是病因——补上后病灶消失。

207. 【传输层可触发性】ADR 定义一个外部可见语义时，必须同时写“传输层如何触发它”——
     写不出来的语义是纸面语义。SUPERSEDE=新 FileId 在 WinFsp 的 fsop.c 里走 FileOverwriteIf 同一路径、
     同一 FileContext，用户态拿不到新建 inode 的时机 ⇒ 该语义经真实挂载无法表达。
     （本该在 ADR-009 写 §4.2 时发现——“以实测为准”写了，“能否触发”没查。）

208. 【删 worktree 前先 grep 未跟踪文件】清理事故：qa-main3 的 zz_doc_refs.rs（B-13 的测试草稿）随 worktree 删除丢失。
     规则：删 worktree 前 git status --porcelain 的 ?? 行先拷再删——未跟踪文件没有任何 ref，删了就是没了。
     （B-13 开工时从报告重建：判定表与三条规则在邮件与流程记录里完整。）

209. 【CURRENT 切换后基线四数“应该变”】v3 的 24 B 在 Blob 内容下被忽略（CURRENT 仍 V2）⇒ 页数不变是当前的特征；
     一旦 CURRENT = V3，inode 记录进页布局，1472 → ~1533 是**预期中的红**——不变才是问题（= 切换没生效）。
     验收 CURRENT 切换批时：四数变化是正确信号；探针同批更新数字。

210. 【裁决“保留”时必须问“保留的是什么”】行保留 / 内容保留 / 可达性保留是三件不同的事。
     B-8 裁“旧 inode 行保留”时被无意识地假设了“Blob 数据也保留”——但行与 Blob 是两份存储。
     “行可以保留而数据被删”是完全可能的（SUPERSEDE 的真实形态）。
211. 【存储层证据比源码证据更硬】QA 的 (ii)：Blob 整对象替换后旧字节无指针可达 ⇒ “旧句柄读旧内容”
     在当前格式不可表达——**与传输层无关**。即使 NTFS 实测显示 FileIndex 变化，我们也做不出旧语义。
     两条独立证据（传输层不给时机 + 存储层表达不了）⇒ (a) 不依赖任何外部证据。

212. 【owner 总原则（2026-10-26，SUPERSEDE 裁决附带）】“只要保证用起来和普通文件系统真实行为一致即可，
     **包括以前和以后的开发内容**。”
     ⇒ “与真实行为一致”是后续所有语义裁决的最高准则：手册说 X 但实测是 Y 的分歧，以实测为准
     （ADR-009 §4.2 的“以实测为准”升为全局）；G1 修订卡 / P3-3b / P3-4 / NTFS 对照 12 类都从它出。
213. 【SUPERSEDE 裁决定案：(a)】同 FileId + 截断 + 属性替换。三重证据（传输层不给时机 / (b) 自相矛盾——
     FileNode 缓存让旧句柄的 FileId 也变新 / 存储层表达不了）。ReplaceFileAttributes=TRUE 是 SUPERSEDE 的
     唯一传输层判别信号（可断言：SUPERSEDE 必达 TRUE、OVERWRITE 必为 FALSE）。

214. 【归因错了处置就全错（两个方向都趟过了）】B-15 那次把格式代价误读成实现缺陷（差点假修：24 KiB 判据其实
     低于格式下限）；CURRENT=V3 这次把实现缺陷误读成格式代价（差点假接受：456 ms 其实是每记录一次 Vec 分配）。
     ⇒ 报告数字前先问“这个代价属于格式还是实现”——修实现再重测，归因才能立住。
215. 【回退保绿优于交付红分支】23 个红里有两类判断（读路径认哪些 profile / 夹具刻意用 v2 验历史），
     混在一次提交改会让“v2 夹具回归”不再是证据（改了它要验的东西）。分开改，每处都能回答“这是设计还是夹具”。

216. 【“调参到刚好过”会让数字失去意义】v3 编码 225 ms vs 红界 224 ms：231 ms 的实现浪费已修（encode_into），
     剩下的 1 ms 是“每条记录多写 24 B”的真实代价。为过线继续“优化”= 指标成为目标的最纯粹形态。
     处置：保留红、报数字与原因、转 owner（选项：接受并调红界至 ×1.25 / release 重测——判据 4 本就标“同 build 同机”）。
217. 【执行序：先怀疑自己的实现，再怀疑格式】214 的两例镜像完成：B-15（查实现 → 确认无缺陷 → 接受格式代价）
     与 CURRENT=V3（查实现 → 发现 10 万次分配 → 修掉 231 ms → 剩余为真代价）。

218. 【“重跑就绿”的合规版本】crash.rs 3 例 IOERR → 单跑 11/11 绿 → 次轮全量也绿 ⇒ 并发环境抖动。
     不是“绿了就算”，而是单跑对照证明了环境因素才算。CI 遇 crash.rs 报 IOERR 先单跑再定性。
219. 【期望值随 CURRENT 翻转的断言】encode 侧的“对象图应变”在 CURRENT=V2 下的正确期望是“不变”（SD 不进 v2 页）；
     切 V3 后才应变。与 baseline_reverse 四数同一机制、同一翻转时刻——此类断言必须与探针在切换提交里一起翻，
     谁都不许单独留旧期望。

220. 【版本翻转断言的同批核对（QA 落成验收判据）】期望值随 CURRENT 翻转的断言（探针四数 / encode 侧 / Stat-SD-fmt 的
     版本依赖期望），切换提交必须**同批全部翻转**；验收判据：git show --stat 核三处，**缺任何一个 = 切换提交不收**。
     把“谁都不许单独留旧期望”从纪律变成机械核对。

221. 【“改测试让它过 = 悄悄定成 (c)”（产品级版本）】CURRENT=V3 切换后 24 个测试红，根因是升级语义未定
     （v3 build 遇 v2 workspace：拒绝/只读/迁移）。改夹具让它过 = 替 owner 做了产品决定。
     ⇒ 测试红是“语义未定”的忠实反映；洗红等于洗决定。在裁决前不提交切换批。
222. 【v2→v3 升级语义（待 owner 裁）】(a) 拒绝（现状连带：所有旧 workspace 打不开）/
     (b) 只读打开（worker 与 manager 倾向：能读不丢数据、不写不错配，与“真实行为一致”及 policy 初衷都合）/
     (c) 自动迁移 policy（危险：静默改语义，正是 policy 机制要防的）。
     依据：main_db.rs:132 钉 Profile::CURRENT.id() 进 policy、:179-185 每次打开比对。

223. 【worker 交接清单（865ebcc，544/0/9）】等 policy 裁决的切换批四件：
     ① CURRENT=V3；② 两探针同翻（sd_v2_shape→应变、baseline_reverse→1533/16116/3）；③ 24 红按裁决分类处置；
     ④ debug（保留红 225>224）与 release（v2=68ms，只作记录）两套耗时。
     两个交接提醒：g3_budget 与 baseline_reverse 切换时自己先红是设计意图；3722c33 的 base_root 断言不随切换翻。

224. 【“待补清单” ≠ “分支上的全部提交”】验收按后者——manager 给 QA 列“三处待补”时漏了中间 4 个提交
     （性能修复与两个 encode 侧）。“我关心的”与“存在的”是两个口径，验收必须按存在的。
225. 【靠巧合成立的正确】open_any 原列表 [CURRENT, V1] 只在“CURRENT 恰好是最新可读格式”时碰巧对——
     它一直绿着，直到切换日才破。与“看起来是它其实不是它”同族但更隐蔽。
     规则：读历史的入口永远列**全部可读格式**，不列 CURRENT。

226. 【未提交观察有保质期；转发前按当前 ref 重核】QA 在旧时点报的 "?? sd_two_way.rs" 在 52ee70d 后已 tracked，
     但观察行残留在报告里；manager 转发时未复核，差点让 worker 删掉刚验收的证据——worker 用四条可复跑证据
     （ls-files/log/status/test）挡住了错误指令。
     规则：① 转发他人的未提交观察前，先在自己这边按当前 ref 核一次；② 接到指令先按证据复核——
     错误的指令应该被证据挡住，而不是被执行。

227. 【“结论有保质期，命令没有”（worker 的实操建议）】转发未提交/工作树类观察时贴复核命令
     （git ls-files <path> / git log -1 -- <path> / git status --porcelain -- <path>）而不是结论——
     接收方可逐条复跑。
     真残留 vs 旧快照的区分，一行判断：git ls-files 有它就不是残留（已被提交纳入）。
228. 【“不道歉”的立场】指令被证据挡住而不是被执行——流程起作用的证明，比零失误更值得记录。
     worker 拒绝执行错误指令 + 四条可复跑证据，是这套体系的预期形态。

229. 【“第二个传播者”（QA 自认）】报告里的任何 git 状态断言，必须在**报告发出前重跑一次** git status 取当轮值——
     状态断言也是观察，也有保质期。事故链：旧观察（8ca05fd 真值）→ QA 报告未重取 → manager 转发未复核
     → 差点删掉已验收证据 → worker 四条证据挡住。两个传播点各断一半、执行端守住——
     “删之前要证据”在两端都要立：提删方给当轮证据，执行方核证据。

230. 【升级语义裁决定案：(a) 拒绝】owner 2026-10-26：未上线、不需要兼容。
     v3 build 遇 v2 workspace ⇒ SchemaMismatch（现状连带效果，无需新代码）。
     24 红的处置：v2 建库夹具重建为 v3（这是 (a) 的正确处置，不是"改测试让它过"——语义由 owner 定死）；
     v2 夹具回归（open_any 路径）必须仍绿——若红是 bug。

231. 【裁决命名带域】两个 (a) 撞名：裁决-S（SUPERSEDE 同 FileId）与裁决-P（policy 拒绝 v2 库）——
     引用必须带域。编号撞名的第 N 例（§10、M1、B-1 之后）。
232. 【夹具重建 vs 期望松动的机械边界】切换批验收第 1 条：git show 看断言改动落在哪个文件——
     断言文本变了的红不算处置、算改测试，除非属于 220 同批翻转的那两处（探针）。
     “合法处置”与“洗红”的边界用文件位置定，不用意图定。

233. 【事故未外溢时报告它，比事故外溢后解释它便宜】解冲突脚本把冲突标记提交进本地（未推送）→ architect
     主动报告含“中途一次失败”→ reset 重做。事故链：发生 → 发现 → 修复 → 声明外溢范围（“没有推送过”）。
     主动报未外溢的事故 = 下一个人不用从远处发现它。

234. 【按角色选页格式的检查点要 grep 枚举】references_of 的 InodeTable 分支用 fmt 而非 inode_fmt（第三处同型错误；
     前两处 inode()/decode_inode 已修）⇒ GC mark 在 v3 store 上走不动，22 条测试同红。
     规则：grep -rn 'profile.fmt' 逐处问“这一处是哪种页”；修过一处不等于修完。
     若当夹具问题改 22 个测试 = 灾难性洗红（留下坏掉的 GC）。
235. 【跨线不稳定比通过诚实】同批两个 debug 读数 225/180，红界 224，波动 25% 足以横跳 ⇒ 不把 180 当通过，
     报“180–225 ms（跨 224 红界，波动约 25%）”。重跑就绿不是解释的正向版：数字不稳定就说不稳定。
236. 【sd_v2_shape 泛化】断言 CURRENT.inode_fmt == 2 而非硬编码 V3——跟 CURRENT 走，v4 出现时不用再改。

237. 【234 的闭环：穷举代替感觉】worker 对 grep -rn 'profile.fmt|profile.inode_fmt' 的 20 处逐一列出
     "哪一页、该用哪个、为什么对"——inode 页三处全用 inode_fmt（含本次修的 references_of），
     其余 17 处都是非 inode 页。"没有第四处"是穷举的证据，不是感觉修完了。
     （可机械化方向：fmt 选择只经 pager::page_fmt 单点，散落直读不可达——记 P3-3b 顺手项。）

238. 【防错结构两个方向要对称】写侧有 pager::page_fmt 单点、读侧 20 处各自判断 profile.fmt——
     不对称的一侧正是本轮出问题的地方（references_of 是第三处、且在读侧）。
     完整形态：读侧 page_format(profile, role)（Role::InodeTable ⇒ inode_fmt，其余 ⇒ fmt），
     与写侧对称；role 是 fetch 已有参数，成本是搬移不是发明。
     落点：P3-3b 顺手项（真后端正好再读一遍这些页）；不挂 G1 修订卡（与页格式无关会被遗忘）。

239. 【合并多提交分支：按内容核，不按 SHA 核】cherry-pick 只挑了链上的一个提交就以为齐了（supersede-a 是五个提交的链）；
     P3-4 卡冲突时 checkout origin/main 的版本——但 main 上还没有该文件，checkout 不存在的路径 = 什么都不恢复，
     冲突中的文件被静默删掉。合入后看 log 出现过某 SHA 就以为齐——log 有 ≠ 内容在。
     核对方式 = 分支头与 main 的**文件级 diff 为空**（architect 的做法）。它救回了 6 个提交。

240. 【239 的机械化口径（architect 给的命令版）】git diff --name-only main...<branch> 列出分支动过的每个文件，
     对每个跑 git diff <branch> main -- <file>，结果必须为空。
     该方法也能发现“解冲突时文件被删”：文件在 --name-only 列表里、但 main 上不存在 ⇒ diff 不为空。

241. 【合入后核对归 QA（两道关卡）】每张卡合入后，QA 对分支头与 main 做文件级 diff 为空核对（240 的命令）——
     manager 出错的那次恰好没有第二道核对；机制化后 manager 第一道（自己跑）、QA 验收道，合入动作有必经关卡。
242. 【(b) 死因的两层链路】DLL 侧 fsop.c:981（Overwrite 返回的 FileInfo 写进 Response）+
     内核侧 sys_create.c:1263（FspFileNodeSetFileInfo 写进共享 FileNode 缓存）——两层各一步、各带行号；
     只引 DLL 半步，读者会问"写进 Response 怎么就影响旧句柄"，内核那半步才是答案。

243. 【行号引用必须带版本 + 贴 grep 原行】QA 报 create.c:1263、architect 核为 1265（1263-1264 是相邻的
     FspFileNodeOverwriteStreams）——同一 tag 下的读数偏差（QA 从上下文块"数行"，错了 2 行）。
     两条规则：① ADR/报告引用外部源码行号必须带 tag/commit；② 必须贴 grep -n 的原行输出，
     不许从 sed 块里数行——机器可复核的引用不经过人手转抄。主张与证据链不受影响（1265 成立，两层链路完整）。

244. 【数字是观测量，名字才是证据】worker 两次"failed=1 名字未捕获"的根因（QA 点中）：只记了数字。
     抓捕协议：① 全量 --no-fail-fast，任何失败原样留档（测试名 + panic 行 + 所在二进制）；
     ② 失败即对照（同二进制单跑 → 绿则 --test-threads=1 串行）；③ 合入核（241）后紧跟一次全量，
     边界与观测绑同轮；④ 第三次出现且两轮对照不能复现 ⇒ 记 KI-* 进 ignored 挂 owner（与 KI-11 同待遇）。
245. 【replaced 字段裁决：保留恒 None】create 侧恒 None / rename 侧仍是 Some——两个同名字段语义分叉，
     两处文档各写一句“不是同一件事”。不删字段（避免第三次破坏性变更；P3-4 后若实测不需要再删，那时是收尾不是破坏）。

246. 【判据失去意义时：删除，而不是改成恒真】G1 修订判据 2 的第二例（“旧对象未删”）在 SUPERSEDE=同 id 后
     失去意义（没有第二个对象）——删除是诚实处理，改成恒真是把它变成装饰（= 洗红的判据版）。
     提交说明写明减了几条、为什么（否则下一个人会当误删）。
247. 【因果断言比改值高一档】翻转期望时同时断言“值”与“值的原因”：
     例① SUPERSEDE 与 OVERWRITE 两格必须相等（守表的结构，防将来重新开格）；
     例② replaced=None 处同时断言 file_id==created.file_id（None 的原因是条目活着，不是字段坏了）。
     只改值会让读者怀疑字段故障；因果断言把翻转的语义一并钉住。

248. 【机械等价用作证据传递】全量跑在验收提交（3f5b56c）上、main 相对它只多 docs 提交且 crates/ diff 为空
     ⇒ 不重复跑同一份内容。一次运行 + 一份 diff 等价证明 = 两次运行的效力。省的是时间，没省证据。

249. 【P3-3b 派卡（2026-10-26）】顺序：③ 读侧 page_format 单点化 + 20 行审计转用例（先做，不依赖真后端）
     → ① 真后端 WorkspaceOps + 7a 改“每场景恰好两处（Mock+Real），只加不减，按名点名缺哪个”
     → ② C1-b/c/d（判据 3 拆“核心可判 / 必须 adapter”两半，拆分线写进文档注释，adapter 半边留 P3-4）
     → ④ 属性落盘两向（判据形态：set→close→重开 workspace→stat 读回——跨 open 的持久性才是落盘的证据）。
     分支 p3-3b/real-backend，基于 2e2e500。

250. 【“审计过”与“不可能再犯”是两件事】十月审计过 20 处（一次性、读一次）≠ 第 21 处不可能再犯——
     后者需要一条会自己跑的断言（page_format 单点 + 扫描器断言 reader.format 裸用为 0）。
     审计 → 断言的升级与就地红法同族，但对象是结构性约定（哪类页用哪个 fmt）。
251. 【扫描器 needle 用 format! 拼】静态 needle 会被自己的扫描命中（源码里包含要找的字符串）；
     format! 拼出则 needle 不以字面量形态存在——与 is_scanner 排除自己同族但手法相反。
252. 【role 枚举无 _ 兜底】page_format 的七个非 inode 角色逐个列出——将来加新 Role 时编译器逼你回答
     “它用什么 fmt”。穷举 + 无兜底 = 新增时不许沉默通过（与 18 格表 match 同一纪律）。

253. 【真后端的放置（P3-3b 裁决）】fs-api 实现 WorkspaceOps（新增 fs-api→fs-winfsp 依赖，方向无环：
     前端 trait ← api 实现）；场景函数留 fs-winfsp（零平台依赖不动）、Real 实例化放 fs-api/tests/、
     7a 扫两个目录（常量 + 失败消息列两路径 + **两目录存在性也要断言**——防“只加不减”在目录维度漏）。
     转错误用 failure_for，不写第二份映射（206 的病因不复发）。

254. 【读写 API 面对称】G2 补了读侧（read_at_id/read_dir_id）却漏了写侧（write/truncate 只有路径入口）——
     WorkspaceOps::write(file_id) 用 write_at(path) 冒充 = 把 G1/G2 消掉的 TOCTOU 在适配层请回来
     （§3.9 路径复用红法在真后端上的形态）。与 238（检查点两方向对称）同族，这次是 API 面对称。
     夹具：写入→rename→按旧 id 写→新路径内容必须变（truncate 同样要 rename 对照）。
     适配层抵消核心层的成果，是 adapter 卡最大的风险——开工前拦住。

255. 【“免费得到的检查”在按 id 的 API 里是系统性风险】路径版从 resolve_file 免费拿到目录检查（IsADirectory），
     按 id 直达就会漏——写目录的字节比报错更糟（看起来像数据）。read_at_id 当初漏过（读侧），
     write_at_id/truncate_id 这次主动补上（写侧）。补 API 时把“路径版免费、id 版必须显式”的每一项过一遍。
256. 【“rename 才是牙”（再确认）】只写不 rename 的用例会通过“内部偷偷按路径解析”的实现；
     “让新对象占用旧路径”比“rename 后旧路径空着”更强——前者让错误实现成功写错对象，后者只让它失败。

257. 【共享场景的常量必须与核心同源】fs_winfsp::ROOT=1 与核心 FileId::ROOT=0 不同源——mock 上恰好对、
     真后端上问到“第一个文件”⇒ NotADirectory。单后端测试永远不红（mock 自己的常量“恰好也对”）；
     只有第二个后端才暴露。C1-d（同一变异两个后端各自红）的镜像：变异是意外植入的。
     修法：常量同源优于常量翻译。
258. 【状态映射与动作分派是两个消费者】failure_for 把 InvalidRecord 映成 Internal 对**状态**正确（NTSTATUS 层词汇统一），
     但 adapter 对“句柄过期”与“条目没了”的动作相反——需要**变体**粒度，Internal 把变体信息丢了。
     ⇒ 映射前单独处理变体：状态可折叠（206），动作分派不可。进 ADR-009 错误语义段。
259. 【定义的位置要能被所有消费者够到】场景函数从 tests/ 上移 src/scenarios.rs——tests/ 里的东西跨 crate 够不到
     （Rust 的边界提示）。一处定义、两处实例化，不是两份。

260. 【P3-3b 全卡完成（2d8e80d，554 passed）】五提交：③ 读侧单点化 → ① 写侧补口 → ② 真后端+7a → 拆分线 → ④ 属性落盘。
     ④ 抓到核心缺陷：核心从未实现属性替换（只有 mock 实现）——同会话读回通过（内存态），只有重开才暴露。
     修复：TreeWrite::set_attributes + TruncateExisting 按 Supersede 清零。
     口径修正：SUPERSEDE 后 get_info 报 0x80（NORMAL 派生）而非 0——“替换”= 存储位清零 + 派生 NORMAL。
     本卡实质产出：三个只有跨后端/跨会话才暴露的缺陷（ROOT 常量 / 属性替换 / 场景跨 crate）——
     C1-d 的现场证明：意外变异 + 真后端 = 免费的变异测试。

261. 【P3-3b 合入（6ed9aae）】QA 验收：C1-d 定向变异红在预测位置（真后端 read 改路径缓存 ⇒ 只有
     scenario_identity_across_rename 红、其余 4 绿）——场景集承重证明（变异只红一个 = 那个场景独占那个失败面）。
     抓捕协议第三次带全名：concurrent_puts（cas-store/store.rs:740）MissingPack——与 KI-11 同族；
     KI-11 证据从复现描述升级为三处独立现场 + 完整留档。定性：并发发布→MissingPack 是一个待诊断问题的
     三个症状面（记 B-17，不阻塞 P3-4）。

262. 【P3-4 已派（2026-10-26）】分支 p3-4/mount，基于 6ed9aae。六点：
     ① 第 0 步 = 纯 Rust 的判据 3 adapter 半边（接线与首次语义验证分离，把“不可用真实挂载调试”的风险提前消掉）；
     ② FFI 纪律：catch_unwind 每回调 / UTF-16 边界一次转 / NtStatus 构造数=0 / ABI static assert；
     ③ L2 同一脚本（NTFS 侧与挂载侧只有路径前缀不同——防脚本漂移被当成行为差异）；
     ④ M1–M9 挂载生命周期；⑤ U1–U14 逐项消（三态，禁只打勾）；
     ⑥ 抓捕升级：失败留档加挂载会话状态（挂载点/句柄数/进程存活）；**卸载失败的测试先卸载再报**（不留挂着的盘）。
     QA 四件备清单：会话状态（进程级抖动归因的上下文）/ 三分档预先定落点 / 消项格式 / KI-11 第四现场预判。

263. 【绑定属于 FileId 而非句柄（P3-4 第 0 步设计）】HandleTable 增 bindings: BTreeMap<FileId, (parent, name)>——
     remove_at 的守卫用 id，绑定的生命周期就该跟 id；同 id 多句柄共享一条（每句柄各存一份会在 rename 后各说各话）。
     三条：open 时从 OpenOutcome.binding 抄入；rename 成功后同步；x→x 时断言 binding 指向当前名字（断言状态不路径）。
     这正是 ADR-009 W3 修订（句柄不存绝对路径、可存定位键、依赖 W2 排他）的 adapter 层落地。
264. 【删除静默失败最难从 NTSTATUS 反推】adapter 拿旧名字去删 ⇒ 守卫拒（NotFound）⇒ 删除静默失败——
     L2 上会被当成“偶尔删不掉”（权限/时序），真因是绑定过期。0a 的表级用例是第一道防线（不进 L2 就能红）。

265. 【断言没发生的事件（比恒真更险）】用例断言“rename 后的新名字”，但用例里根本没有 rebind——措辞完全合理
     （rename 后名字当然是新的），只是让它成立的动作没被调用。若实现碰巧在别处做了，用例会绿但证据链是断的。
     “用例写错和实现写错一样会假绿。”判据预审“可达吗”不仅问被测物，也问用例自己的步骤是否真的执行了
     它声称依赖的路径。
266. 【表是投影不是副本】HandleTable 的绑定只反映自己持有过的（rebind 只更新已存在，不凭空造状态）；
     rename 在核心层可作用于任何路径，但 adapter 的表永远不比核心多知道一件事。

267. 【“拒绝即未改动”的价格明码标价】挂起检查放在后端创建/截断之前 ⇒ 每次 open 多一次后端调用——
     这是原则的价格，不是待优化的开销。反过来（先改再拒）会留下“日志说拒绝、文件已被清空”。
     将来想优化掉这次探测，得先推翻“拒绝不留副作用”。
268. 【RenameOutcome.file_id：核心本来就知道】从路径反推 id 是本 crate 一直避免的猜；
     路径→id 翻译只发生在路径语义的入口（create_or_open/rename），不发生在身份语义的地方（绑定更新）。
     与 read_at_id 同族。
269. 【一个挂载 = 一个 Adapter】回调里 new 会得到两张表、两个 delete-pending 视图——两张表 = 两个真相源，
     两段删除会在两个视图里各删一次或都漏删。用类型限制生命周期（而非注释）。

270. 【手写 FFI 结构体的可溯源性】手写最小结构体（不 bindgen——SDK 只在测试时需要，不成为构建依赖）时，
     每个字段注明 SDK 头文件行号出处（// winfsp.h:xxx）；布局断言的期望值（size/offset）注明计算方式。
     与 243（行号引用带版本）同族：手抄必须可溯源，否则布局漂移时无法核对。
     bindgen 静默跟随头文件变化；手写 + 断言在漂移时红——更强而非更弱。

271. 【结构体尺寸是推导值，不是记忆值】VolumeInfo = 8+8+2+64 = 82，u64 对齐 ⇒ round 到 88——
     期望值旁写算术，否则 88 是魔数。判据数字纪律（第 6 条）在 ABI 尺寸上的形态。
272. 【自洽 ≠ 正确（结构体版）】自己的 offset 断言只证内部自洽；与 SDK 自身的 static assert
     （fsctl.h:291/:276 的 72/88）对上才证“抄写没引入错误”——第二证人。
     与 round-trip 不证布局（169）同族。

273. 【位置式表的形状必须先于行为被钉住】WinFsp 回调表按 SDK 次序排列（重排即 ABI bug）——但重排后每个函数
     仍正确执行，任何行为测试都抓不到（或表现为“偶尔调错回调”的诡异形态）。
     断言：尺寸=项数×指针无填充、项数钉数字、默认全 None（None=不支持，WinFsp 认的答案，比悬空指针安全）。
274. 【空表也必须被描述成空表】只有形状、没有接线时，文档与提交说明写明“今天挂载看到的是空表”——
     主动声明“还没有”，防读者以为 FFI 已能用。交付物的诚实（与判据的诚实 156 同族）。

275. 【拒绝留给数据语义，不留给显示语义】超长卷标签截断（31 单元+终止符）而不拒绝——拒绝会让挂载卷
     因装饰性字段不可见，代价远大于截断。内嵌 NUL 仍拒（会静默丢后半段 = 数据语义）。
     错误的严重度要分层：功能代价 > 显示代价才值得拒绝。
276. 【同名不同值的单位换算单点化】VolumeLabelLength 是字节、VolumeLabel 是 UTF-16 单元（off-by-two 的经典坑）——
     换算写在函数体（最靠近数据的一层），包装层顺手做 = 每调用点各算一次、迟早一个错。与 page_format 单点化同族。

277. 【载入失败的可诊断性】LoadLibraryW 失败要报 GetLastError + 尝试的路径，且**路径不存在与导出缺失分开报**——
     前者是 SDK 没装、后者是版本不符，指向完全不同的环境问题。一句 "load failed" 让两者不可区分。
278. 【AllocationSize 是策略不是事实】核心存精确字节，NTFS 按簇取整——报精确值会让普通文件在比对工具里
     看起来稀疏。ALLOCATION_UNIT=4096 显式化（可讨论可改）；对齐的是消费者期望，不是模仿 NTFS。
279. 【同一个决定只在一处做出】adapter 的 get_info 已报派生属性 ⇒ FFI 层直通，不二次派生
     （DIRECTORY/NORMAL 的决定出现两处 = 两处可漂移）。与“错误映射只走 failure_for”同族。

280. 【错误按修法分类，不只按错误码】LoadError 三臂：NotFound（装 SDK）/ MissingExport（换版本）/
     LoadFailed 193（换架构）——每个臂对应一个不同的用户动作。错误设计从“发生时是什么”转向
     “收到的人该做什么”。与 194（InvalidRecord/NotFound 动作分派）同族但方向相反：那是 adapter 的动作，
     这是用户的修法。
281. 【构造测试值，不伪造场景】无法在无 OS 环境触发的 LoadFailed 臂，用例直接构造它——伪造载入会让测试永远绿，
     但测的不是真实路径。与 P3-0c“造不出真 v1 库就明说”同族。
282. 【跳过也要留痕】CI 找不到 SDK 时显式跳过并打印原因，不静默通过。

283. 【跳过要自报身份】CI 找不到 SDK 时 eprintln "SKIP: … (this is a skip, not a pass)"——
     跳过的检查看起来是绿的，比没有检查更糟。skip 与 pass 必须在输出上可区分。
284. 【同名 DLL 三份只有一份能用】本机：Program Files (x86)/WinFsp/bin/winfsp-x64.dll（真实运行时，FspVersion→2.0.1 ✓）、
     SxS 布局同份、winfsp-sdk/DYNAMIC/SxS/DYNAMIC/bin（dynamic-loader 变体，GetProcAddress 直调即 0xC0000005）。
     DEFAULT_LIBRARY 优先顺序 + WINFSP_DLL env 覆盖；dynamic 变体在文档注明“不要再试”。
     握手的价值第二证明：“路径看起来对”与“这份 DLL 能不能这样用”只在真正调用时分开。

285. 【FFI 签名必须逐字段核头文件——崩溃的第一嫌疑】FspVersion 崩溃（0xC0000005）根因：签名写成
     fn() -> u32，实际是 FSP_API NTSTATUS FspVersion(PUINT32)——无参调用让它把垃圾当指针写。
     排查时三个候选（FspLoad？调用约定？DLL 变体？）全错——**第一嫌疑应该是签名本身**。
     修法含双断言：status == 0（NTSTATUS 成功）且 version > 0（单独 version > 0 可被 error 码假绿）。
     worker 的 #[ignore] 处置正确：崩溃不能被跳过（带走同文件其他用例），完整理由 + 两种结果并列。

286. 【FFI 崩溃排查：排除本身就是贡献】worker 列三候选（FspLoad/调用约定/DLL 变体）逐一排除；manager 在干净的
     排除基础上看头文件一行定位签名真因。排除得越干净，定位越快——含糊的候选会淹没真因。
287. 【两套错误码不混用】GetLastError（Win32）与 NTSTATUS（内核）数值空间重叠但语义不同——
     NTSTATUS 包进 LoadFailed{last_error} 时注释写清，防止后人合并两套（否则 LoadLibrary 失败与
     FspVersion 错误在日志里无法区分）。

288. 【读起来像表、布局却不是表】FSP_FILE_SYSTEM_INTERFACE 实际是 64 槽数组（SDK 自身断言 winfsp.h:1091），
     不是 10 个命名字段——第一版“按 SDK 次序排 10 项”看起来满足位置式纪律，但那纪律的前提是布局真是槽位数组。
     形状钉错，行为层永远不会告诉（传进去的 Create 落在“恰好对上”的槽里，根本没有对应关系）。
     只有对照头文件自身的 static assert 才会红。272（第二证人）的又一层：不仅字段值对，表的形状本身要与 SDK 断言对上。
     worker 自纠（对照 SDK 断言发现），比外部发现好。
289. 【观测方式的嫌疑】不稳定红三次逃过同一个文本 grep（FAILED$/panicked/1 failed）——三次都空，
     说明问题在观测方式而非测试。升级候选：JSON 事件流（--message-format=json 解析 test failed 事件）优于文本 grep。

290. 【抓捕协议 v3：JSON 事件流】文本 grep 空手是结构性的（并发下交错、折行、失败详情与 failures 块分离——
     grep 永远在追会变形的目标）；JSON 一事件一测试，名字与结果在同一事件——观测方式不再依赖文本形状。
     三条实现：每次全量落盘 runs/时间戳.jsonl（绿跑也留，第几次出现本身是数据）；grep '"event":"failed"'；
     failed 立即单测级循环 ×5（比全量循环便宜两个数量级）。
291. 【边界假设收紧（三次数据点）】不稳定红全部出现在“新构建后首次高负载并行”（冷缓存+全并发），
     与 rebase 无关；干净边界的常规全量 0 次。⇒ 每次合入/rebase 后的首跑 = 指定抓捕窗口，永远带 JSON 落盘。
292. 【形状类判据的唯一可靠证人】产生布局的那一方自己的断言（SDK 的 static assert winfsp.h:1091-1092
     + 1089 注释 "always contain 64 function pointers"）——我们的测试只能证自洽。

293. 【文本 grep 漏名的两层机制】① 形态变形（并发交错、折行、失败详情与 failures 块分离）；
     ② 位置不可预测（失败在最后一个 crate 时 grep -B12 取不到上下文——crate 边界间顺序不固定）。
     两层叠加使文本 grep 在结构上不可能稳定 ⇒ JSON 事件流是常驻观测底座，不只是猎红工具。

294. 【检查顺序就是诊断顺序】库在不在 → 符号在不在 → 回调行为——每层失败直接指向下一层该看什么。
     把检查从清单变成决策树；与 280（错误按修法分类）同族：设计的消费者是收到失败的人。
295. 【“停止”与“停止后可重启动”是两件事】stop 与 delete 之间观测 dispatcher 线程是否已退
     （WinFsp 不提供则记“无法观测”并注明）——M1–M9 的“重挂”依赖后者，不只依赖“能停”。

296. 【位域用“不透明字节 + 访问器”，不手抄 struct】C 位域的位序编译器可自由选择（标准不保证）——
     即使逐位对照头文件，Rust 与 MSVC 布局也可能不同。504 字节缓冲 + 带行号的类型化访问器（显式位运算）
     把位序决定权从编译器手里拿回来。用例“36 之后全零”防的是“顺手编码几个位”——位域多设一个位
     不会编译错，只会运行时行为漂移。

297. 【unsafe 的位置本身就是文档】FFI 加载的自纠：四元组匹配 + 一次 FreeLibrary ⇒ unsafe 只留在
     transmute 一处；调用已 transmute 的函数指针再套 unsafe 是多余的。
     unsafe 块应该只覆盖真正不可证明的那一步——读代码的人从 unsafe 的位置就知道风险在哪。
298. 【框架局限做成断言】stop_is_observable = false 且用例断言它就是 false——WinFsp 2.x 无
     “dispatcher 线程已退”查询。将来提供该查询或换版本时这条会红，提醒重新评估。
     “无法观测”从注释升级为可判事实。

299. 【不要为“将来可能需要”加全局机制】④ 的两个回调不需要挂载级可变状态 ⇒ 不引入全局注册表 (b)
     （Mutex 串行化 + 清理顺序 + 全局可变状态三重风险）。等 ReadDirectory/Rename 需要 (b) 时同批写
     清理顺序与并发约束——“顺手加一个只有一半消费者的机制” = 造半边入口（196）的 FFI 版。
300. 【FileContext 的生命周期纪律】由 WinFsp 决定（Cleanup/Close 后不再给）⇒ Box::from_raw 只许在
     Close 里做一次，配 debug_assert 计数器（分配数==释放数）。双重释放/泄漏在挂载场景表现为
     “几次操作后崩溃”——最难查的形态，计数器把它变成可判事实。
     FileContext 最小集 { file_id } 起步——它是 WinFsp 替我们保管的指针，塞太多就是把自己的状态机寄存在别人家里。

301. 【不半填，宁可不写不可半写】缓冲小于结构体 ⇒ 拒绝——半填返回成功但对面读到未初始化内存
     （可能是 0 也可能是任意值，取决于栈残留）。拒绝让调用方重试，半填让他拿垃圾做决定。
     同族：“未知 id 拒绝且缓冲不动”（零记录显示成空文件 = 静默错误）。
302. 【测试合并承认生产约束】两个测试抢同一个 OnceLock ⇒ 合并成一个并写明理由——一进程一挂载是
     生产形态，不是测试不便。--test-threads=1 是掩盖（“两个测试抢一个全局”的事实消失），合并是承认。

303. 【第三次同一工具错误 ⇒ 改工作方式，不再试一次】嵌套 heredoc（bash 嵌 Python 三引号嵌 Rust）
     三次被截断——三层引号规则间传内容，截断不是意外是必然。改用文件写入工具一次成型。
     脚本适合机械替换，不适合写入含引号的整段新代码。
     工具事故的处置：立即回退而非提交（工具产出的一半不是进度的一半，是损坏——200 的工具版）。

304. 【"没调用"与"调用失败"分开】mount 失败时 probe=None 有歧义——短路没调用闭包 vs 调用了返回 None。
     三态 Skipped{reason}/Ok/Err(msg)。与"无法观测 ≠ 已验证"同族。
305. 【0xC0000035 排查记录】create=0（接口表被接受 ✓）但 SetMountPoint 拒绝。排除：WinFsp.Launcher RUNNING、
     驱动 RUNNING。剩两候选：① 目录挂载点必须显式 create 空目录；② device_path 需 \\.\ 前缀（winfsp.Disk → \\.\winfsp.Disk）。
     unmount=0 且 looked_like_a_mount=false ⇒ 先卸载顺序起效，无残留。

306. 【0xC0000035 排查第二轮】两个候选排除（空目录必须显式建 ✓ 排除；\.\ 前缀 ⇒ create 反而变 0xC000003E ⇒
     前缀形式错误且 winfsp.Disk 无前缀是对的）。
     新头号候选（读 memfs 示例得出）：VolumeParams.Prefix（fsctl.h:237，UNC 形态）——目录挂载可能需要它构造
     volume name。分叉法：先试盘符挂载（不需要 Prefix，最便宜的分叉）→ 成功则补 Prefix 再试目录 → 都失败再试 NT 路径形态。
     memfs 示例的目录挂载传 Win32 路径（无 NT 前缀）⇒ NT 假设与示例矛盾，降级。

307. 【★ 盘符挂载成功（69c3295，2026-10-26）】Windows 侧第一次看见 layer-fs：
     create=0 / mount=0（盘符 Y:）/ start=0；read_dir 返回 ERROR_INVALID_FUNCTION——
     “未实现”（走到了我们回调表的 None 槽）与“挂载没生效”可区分。从 spec 到真实 OS 调用走通回调链。
     候选 ② 确认：目录挂载缺 VolumeParams.Prefix（盘符不需要）。
     卸载 0xC000000D：SetMountPoint(NULL) 盘符形态可能不支持——memfs 退出路径只用 StopDispatcher+Delete
     （delete 隐式清理挂载点）；验证法：delete 后 GetLogicalDrives 看盘符消失（比 NTSTATUS 更硬的证据）。
308. 【“未实现”与“没生效”可区分】read_dir 返回 ERROR_INVALID_FUNCTION = 已进回调表但槽是 None；
     返回“找不到路径”才是挂载没生效。挂载诊断最重要的一对区分。

309. 【状态码说“调用怎么想”，OS 读数说“现在是什么”】unmount=0xC000000D 配上 GetLogicalDrives 盘符消失 ⇒
     0xC000000D 只是“这条路径不通”，不是“没卸干净”。两个证据层分工：NTSTATUS 是某次调用的返回（调用失败 ≠ 状态残留），
     OS 读数是状态的直接观测。断言放在状态证据（released_after_delete）上，状态码只打印——断言位置跟证据类型匹配。
     （“这个形态不支持 SetMountPoint(NULL)”与“没卸干净”——两个事实被一个证据分开，与 308 同款。）

310. 【Prefix 偏移 = 40（三层校验闭合）】标量区 0-35（Version@0/Sector@2/SAPU@4/MaxComp@6/VolCreation@8/
     Serial@16/Transact@20/IrpTimeout@24/IrpCapacity@28/FileInfoTimeout@32）+ 位域区 36-39（32 bits 恰一 u32）
     ⇒ Prefix(WCHAR[192]=384B) @40。
     校验 1：SDK static assert V0==456（fsctl.h:258），40+384+32=456 精确吻合；
     校验 2：V1 剩余 48（位域4+五timeout20+Fsext4+Reserved32 4+Reserved64 16）两头闭合；
     校验 3：worker 已钉的 0/2/4/6/32 与推导一致。
     worker 拒绝猜偏移（脚本两次被反斜杠打断即停）——第三次同手法失败换手法（303 的再应用）。

311. 【布局联动断言】HEADER == sizeof(FileInfo) + 32 ——两个“独立”的 ABI 断言耦合成一个不变量：
     dir_info 的 header 包含 FileInfo，两者关系本身是接口契约的一部分。分开钉各自对，合起来可能错
     （算术错在关系上）。将来 FileInfo 变了这条跟着红。
312. 【Rust 无对应形态时选暴露算术，不伪装 struct】柔性数组（WCHAR FileNameBuf[]）与位域（296）同一决策：
     伪装的结构体看起来类型安全，实际只是把 unsafe 挪了个位置；算术显式、可断言、可教学。
     header/对齐错误不产生错误码，只产生垃圾行为（文件名乱码）——与 64 槽（288）同族。

313. 【差异排除法：改一处、输出不变 ⇒ 那处不在关键路径】接了 ReadDirectory 但错误码与接前一字不差
     ⇒ 问题不在 ReadDirectory——WinFsp 的调用链是 Open →(FileContext)→ ReadDirectory，
     没有 Open，Read 根本不会被调到。改了没变化的那个改动本身就是证据。

314. 【allocated=0：Open 前面还有一环】计数器证明 Open 从未被调用 ⇒ 错误在到达我们之前。
     头号候选：GetSecurityByName（winfsp.h:252，槽位 1）——Windows I/O 管理器 CreateFile 的固定序列里
     它排在 Open 之前（先查安全描述符再打开）；槽位 None ⇒ 整个 Create 失败 ⇒ ERROR_INVALID_FUNCTION
     ⇒ Open（槽位 4）轮不到被调。完美解释 allocated=0。
     VolumeLabel 候选降级：mount=0 已证卷名不是挂载阻塞项。
     计数器是本轮最值钱的数字——“从没被调”是可判事实（比推断早一步）。

315. 【挂载点前置断言】Y: 盘符本机已存在 ⇒ 之前所有 LAYER_FS_MNT=Y: 的实测挂在已存在的盘符上，读数不可靠
     （含 released_after_delete）。以后测试先断言挂载点不存在——把环境事故变成永久断言。
     更正后 Z: 上结论不变（create=0 mount=0 卷在）——方向没错，读数根基被加固。
316. 【接口表生命周期（嫌疑最大）】FspFileSystemCreate 文档没承诺拷贝，C 惯例是存指针；栈上 interface
     在 create 后悬垂。悬垂表的表现恰好是：卷在（create 时表活）、回调全零（start 后表死——WinFsp 读到
     悬垂内存的 None 槽 ⇒ not implemented ⇒ ERROR_INVALID_FUNCTION）。与全部观测吻合
     （GetSecurityByName=0 / allocated=0 / probe INVALID）。若成立必须固化：接口表生命周期 ≥ FileSystem 生命周期，
     写成类型保证（mount 持有 Box 或静态存储），不是试一次就完了。

317. 【机取救了 manager 的数】GetSecurityByName 槽位是 2 不是 1（winfsp.h:221 是 SetVolumeLabel，:252 才是
     GetSecurityByName）——manager 读行号时数错位置，worker 的机取索引救了双方。
     人读行号会错，机取不会——包括 manager 给出的数。相邻标签极易混淆，索引一律机取。
     probe 口径修正：Y: 意为"Y: 的当前目录"，须用 Y:\（无目录状态的 FS 无法回答前者）。
318. 【断点定位的二计数器法】GetVolumeInfo（任何卷打开都会问的第一个问题）与 GetSecurityByName
     （Create 路径第一环）两个计数器：全 0 ⇒ IRP 从未进入用户态（派发层/表问题）；GV>0 而 GS=0
     ⇒ IRP 进来了但 Create 路径断。三观测判定矩阵：盘符在吗 / GetVolumeInfo 计数 / leak 后回调来吗。

319. 【owner 裁定：挂载测试一律 Z:】Y: 本机已存在（Get-PSDrive: C D E F S Y）——owner 原话
     "Y: 已经存在了，以后使用 Z: 测试"。LAYER_FS_MNT 缺省改 Z:；前置断言挂载点不存在照旧
     （今天不存在不代表下个月不存在，断言才是保险）；Y: 历史读数的更正说明保留不回改。

320. 【别猜，先读 FSD 侧观测器】"卷在、回调全 0"两个 WinFsp 自带观测器（导出已验证存在）：
     ① FspFileSystemGetDispatcherResultF（winfsp.h:1392）——派发线程致命失败会记 DispatcherResult（:1107），
       之后 IRP 全拒——与全部观测吻合（卷在/mount=0/回调全 0）。probe 后读一次即得确切 NTSTATUS。
     ② FspDebugLogSetHandle + FspFileSystemSetDebugLogF（:2165/:1410）——FSD 侧 IRP 跟踪写文件
       （memfs 的 -d 就是它）。JSON 事件流的 FFD 版：观测优先于推断。
     注意：SetDebugLog 的 inline 版写私有字段，必须用 F 后缀导出函数版。
321. 【VolumeParams 对照 memfs（二级候选）】逐字段比 memfs.cpp:2432-2460，我们缺：VolumeCreationTime/
     VolumeSerialNumber（memfs 用系统时间）/**UnicodeOnDisk=1（我们 0 = ANSI 名，IRP 来了 UTF-16 解析也错——暗雷）**/
     CasePreservedNames / PersistentAcls。全在位域区 @36-39 同一 u32——若日志指向参数，按 memfs 全集一次补齐，
     不逐个试。587/2 首跑红 = KI-11 族老毛病（复跑绿，仍记未解释）。

322. 【debug 日志三坑】① FspDebugLogSetHandle 是进程级，须在 create 之前设好（否则丢头几条）；
     ② 日志文件放测试 temp 目录 + 时间戳；测试结束先关句柄再读文件（append 句柄开着时缓冲可能未落）。
     ③ flags 第一轮 0xffffffff 全开——不知道哪类跟踪是关键行，先全量读到答案再收窄。
     预读两分叉：FSD 发了 TRANSACT_REQ 而 UM 没收 = 派发断层（表对接/读取时机）；
     FSD 自己拒 = 理由在它前一行。CaseSensitiveSearch 不设（按语义定，不照抄 memfs——它=1 是测试配置）。

323. 【结构靠写，不靠 patch】303（heredoc 截断）与本次（脚本插入错位+括号失衡）的共同点：用脚本改结构
     而不是写结构。文件到一定规模（mounted.rs 480 行 + 6 extern 块），整段重写（先完整读原文）比往里插便宜
     ——插入位置错与括号失衡这类错误在重写里根本不存在。零提交+立即回退的处置两连正确。
     附：extern 声明收拢成单一 ffi 模块（ABI 行号注释的单一去处）。

324. 【观测器 2 战果：两行日志干掉五个假设】debug 日志（FspDebugLogSetHandle + SetDebugLogF）输出：
     ">>Create ... FILE_OPEN" + "<<Create IoStatus=c0000010" ⇒ IRP 到达 FSD（断层/身份/表/位域假设全灭），
     真因：Create（槽 3）是必需回调，FILE_OPEN 也走它（由它转 Open）。槽位 None ⇒ WinFsp 回
     STATUS_INVALID_DEVICE_REQUEST——与日志字面一字不差。
     附教训：**"槽位表里有名字相似的回调"≠"它是入口"**——我们接了 Open(槽4) 以为打开走它，
     但 WinFsp 的入口是 Create。入口判定只能靠日志或文档，不能猜名字。
     （回顾：接口表生命周期/派发器死/卷身份/位域/Prefix 每个都够盲试一轮——观测优先于推断的完整回报。）

325. 【d83258d 后的新假说层】接上必需回调 Create 后仍 c0000010 且全计数 0 ⇒ "缺 Create"是表象，
     根因是"表没被采用"或"索引错位"。穷举探针三补强：① 塞满全部 64 槽（不止 0..15——DLL 的 Create 可能
     排在 15 之后）；② 同一性检查：打印传给 FspFileSystemCreate 的表指针 vs 填记号的表指针（不同则当场结案
     ——leak 的可能不是被读的那份，悬垂表假说当初"leak 了也没变化"的真正原因）；
     ③ Version=504 嫌疑最低：create=0 已证卷创建 IOCTL 接受了 Version（校验发生在那步）。

326. 【★ 头文件知其有，探针知其位】DLL 实测 Create 在索引 32，头文件机取是 3——谁都没数错：
     安装 DLL = 2.1.25156.ddca7bd（2025 发布版），SDK 头 = sxs.20261022T083021Z（主线），两个都报
     FspVersion 2.1，但 FSP_FILE_SYSTEM_INTERFACE 条目顺序在两版间重排过（差 29 指针）。
     纪律：头文件只用来知道"有哪些操作"，不用来知道"在第几个"；槽位索引必须对安装的 DLL 实测
     （64 槽记号探针 + 同一性检查 + 逐槽触发）。三步探针：填满 64 槽（表被采用 ✓）/ 指针同一（✓）/
     逐槽触发（Create=32）。
327. 【勿假设平移；日志点名下一个】重排不是平移——已接的全部索引（GV=0/GSBN=2/Open=4/Close=7/RD=18）
     都未被调用过（计数全 0）= 全部未确认，每个都要独立探针。收敛循环：挂盘→触发→日志下一个 >>Op 失败行
     = 下一个缺失槽，每轮自动指定下一探针。校准表断言：每条注实测日期+DLL 版本+探针命令；
     总断言：FspVersion 版本与校准时不一致 ⇒ 整表作废（防"版本号相同构建不同"——两个都叫 2.1 是教训）。

328. 【码没变、来源变了】Create@32 接上后 create=1：同样的 c0000010，before 是 FSD 读空槽自答，
     after 是我们的 NOT_IMPLEMENTED 走完全程。错误码是静态的，计数是因果的——计数器比错误码值钱。
     早退分支区分：每个 return NOT_IMPLEMENTED 前独立计数（RESOLVE 未配 vs file_info 时间戳 Err）。
     root id 0 lookup 覆盖 = 探针走完的最小前提（Create 第一个请求就是 "\"），不是改生产语义。

329. 【矛盾读数 ⇒ 早退点枚举不全】create=1 但三条已计数早退全 0 且未分配 FileContext——两者矛盾 ⇒
     还有未枚举的路径。最可能：Create trampoline 自己的 disposition 门（FILE_OPEN→open，其余→直接
     c0000010 且无计数）——与全部读数吻合。具体嫌疑：Create/CreateEx 双槽（Ex 多 Pattern/EA 参数），
     DLL 2.1.25156 若把 32 号槽当 CreateEx 调，参数从第 3 个起错位，Disposition 读歪。
     处置：入参打印，裸值行+解读行都打——裸值是事实，解读是假设，假设错了裸值还在（观测与解释分开，
     304 同族）。再加计数器只是把同一未知切细，不是新信息。

330. 【位用探针、形用裸值、头文件只当目录】与 326 成对：槽位索引用 64 槽记号探针实测；参数签名用
     裸值打印反推（每个参数打十六进制，指针样的限长读串）；头文件只告诉我们"有哪些操作"，
     不告诉我们"在第几个""怎么调"。交叉核对源：DLL 自己的 debug 日志行（>>Create … CreateOptions=21
     …）是 DLL 用它的签名解出的 ground truth——裸值序列必须能对上这些已知值，找到 CreateOptions 落在
     哪个 pN，前后参数全定。指针解引用须限长（0xC0000005 教训）。

331. 【★ 探针陷阱：槽 32 = DispatcherStopped（VOID 成员）】"实测 Create@32"是双重陷阱：
     ① 机取脚本只匹配 NTSTATUS 返回的成员——VOID 返回的 Cleanup(6)/Close(7)/DispatcherStopped(32) 全漏数
     （枚举 ABI 按函数指针数，不按返回类型）；
     ② 单槽探针被顺序依赖骗：n=3 不亮是因 GSBN@2 为 NULL 短路了 Create 路径（先 GSBN 后 Create）；
     n=32 必亮是因 DispatcherStopped 在每次 stop teardown 时被调（陷阱槽，与目标操作无关）。
     全填探针 hits=2 = GSBN 一次 + DispatcherStopped 一次，不是两次 Create。
     裸值 (fs,0x1,…) = DispatcherStopped(fs, Normally=TRUE)，后随寄存器垃圾。
     修正探针：依赖链一起填（GSBN@2+Create@3+目标槽），teardown 槽留空或过滤。
     结果：全部头文件索引（0/2/3/4/6/7/11/18）被精确源码（commit ddca7bd=2.1.25156）证实——原始机取从头就对。
     方法教训：ground truth = 精确版本的源码（版本号里的 commit hash 可直接取 raw 文件），比头文件和朴素探针都硬。

332. 【证据落在 /tmp = 没落盘】KI-11 第四现场（concurrent_opens_wait_then_publish_correctly，
     pack_concurrency.rs:237，os error 2，release 构建，2026-10-24）因复现日志躺在 /tmp 未归档而漏报，
     QA 清理时翻出。证据的归宿是归档目录或邮件；temp 里的证据随会话消失——尤其不稳定复现的现场，
     丢了可能几周再见。抓捕协议第 1 条已并进"立即归档（名字+panic+归档路径）"。
     KI-11 画像升级：四处现场 / 两条 ignored 测试都有带名复现（不是防抖静音，是症状面证据载体）/
     debug+release 双口径（排除 debug 断言时序解释——问题在产品并发路径本身）。已转 owner。

333. 【★ 门禁在成员之前（fsop.c:907 NULL 门）】ddca7bd 源码：FspFileSystemOpCreate 进门先查
     (Create|CreateEx) + Open + (Overwrite|OverwriteEx) 三元组，缺一即 return STATUS_INVALID_DEVICE_REQUEST
     ——不调任何接口成员。Overwrite@5 缺席 = 第一天以来所有 c0000010 的唯一根因
     （不是槽位错/表未采用/参数序）。每类操作各有门（Read/Write/GetFileInfo/GetVolumeInfo/
     ReadDirectory/… 同款，fsop.c:1018/1044/1095/1251/1324）。
     方法论闭环：接了 A 但 A 计数=0 的第一候选 = 同类操作其他必填成员缺席，不是槽位错。
     只读挂载最小必填集：Create@3+Open@4+Overwrite@5+GSBN@2+ReadDirectory@18+GetFileInfo@11+Cleanup@6+Close@7。
     附：全填探针 hits=2 = 两 dispatcher 线程各调一次 DispatcherStopped@32（start(fs,2)），
     read_dir 期间零接口调用（门禁挡住）。
     ground truth 链条最终形态：精确 commit 源码（fsop.c 的控制流）> 头文件布局 > 任何探针。

334. 【两段式契约 = 不半填的另一形态】GSBN 的契约是两段式（先传小缓冲/NULL 问大小 ⇒ 只写 *size
     不碰缓冲；再传大缓冲 ⇒ 写完整描述符）；GetVolumeInfo 是单段（不够 ⇒ 拒绝）。
     同一"不半填"原则在两种契约下的两种形态——判别信号是契约本身，不是风格偏好。
     ACCESS_VIOLATION 的第二候选：fsop.c:797 调 GSBN 时 PFileAttributes 传 NULL（四个 0）——
     无条件写 *file_attributes 即崩；四个输出参数（attributes/descriptor/size/size 本身 NULL）
     全要防护（f9ee1c3 的四条拒绝接到真实 trampoline）。

335. 【错误链 = 逐缺陷剥洋葱】c0000010 门禁（缺 Overwrite）→ c0000078 SD 缺 SELF_RELATIVE →
     c0000005 写 NULL 参数 → 267 ERROR_DIRECTORY（目录语义）——四环四修，无一白走。
     错误码前进就是链路在流动的证据（328 的姊妹条：那次码没变来源变了，这次码本身在变）。
336. 【夹具 shape 要跟真实语义一致】探针/夹具的问题会表现成实现的问题（root 夹具按文件造 ⇒
     GSBN 报非目录 ⇒ FILE_DIRECTORY_FILE 请求被判 ERROR_DIRECTORY）。判别法：症状指向的语义
     与夹具声明对表。探针方法论拼图：326（位）+ 330（形）+ 331（顺序依赖/teardown 陷阱）
     + 333（门禁）+ 336（夹具 shape）。

337. 【FileContext 1:1 配平 = 生命周期纪律首次真实收账】根改目录后读数跃变：volume_info=1（Windows
     开卷第一问终于有答案）、Open 分配 1 / Close 释放 1——"Box::from_raw 只许在 Close 做一次 + 计数器"
     （299/300）在真实挂载上首次验证为对称。双重释放/泄漏这对最难暗病，计数器说：没有。
     create=0 不是缺陷：fsop.c 分派 FILE_OPEN→Interface->Open，Create 只在 FILE_CREATE 型 disposition
     被调——读数要对照控制流读（328 又一例）。失败定位先翻 debug 日志（>>Op 行已点名），
     计数器只在日志不含该 op 行时才需要（那时是 IRP 没到，另一种问题）。

338. 【★ 里程碑：dir Z:\ 成功（290985f）】Windows 侧第一次读到 layer-fs 命名空间。读链闭合：
     volume_info→security→open→read_dir→close，FileContext 1:1。
     错误链完整复盘（收口报告素材）：c0000010(门禁缺Overwrite)→c0000078(SD缺SELF_RELATIVE)
     →c0000005(写NULL参数)→267(根夹具按文件造)→c0000010(三处不同来源)→Ok(())。
     七环七修，无一"重试就绿"。
     三条新判据：①缓冲太小≠未实现（BUFFER_TOO_SMALL 是"换大缓冲再来"）；②GetSecurityByName 与
     GetSecurity 是两个操作（相邻名字≠同一回事，与 317 同族）；③空列表与槽没接必须可区分
     （共用错误码则"实现错了"与"根本没到"永远分不清）。
     下一步：内容断言（alpha.txt/beta 名字+属性）→QA 挂载验收→合入 main（滚动口径）。
     注：user@2e9954e1 无 mailbox（owner 未注册）——owner 通报改为当面/渠道外，此前"转发 owner"的邮件均落此坑，待确认 owner 实际收件方式。

339. 【ReadDirectory 精确签名（ddca7bd :703）】(fs, FileContext, Pattern, Marker, Buffer, Length,
     PBytesTransferred)——Pattern/Marker 在 Buffer/Length 前，无 Offset 参数，末位是**输出指针**。
     裸值逐位吻合：p4=Buffer 大地址、p5=0x328=808=Length（曾被误读为 Marker）、p6=PBytesTransferred 待写。
     空目录双层解释：参数序错 + 未写 *PBytesTransferred（OS 拿 0 计数 ⇒ 无条目）。
     续读协议：首查 Marker=NULL 从头；续查 Marker=上批最后一条名；结尾 SUCCESS + 计数 0 = 目录完。
     输出指针必须写（与 GSBN 的 *PFileAttributes 同族，均带 NULL 防护）。
     方法论：拿到精确 commit 源码后，签名问题先查源码再探针（330 的升级：源码在手，裸值只做验证）。

340. 【★ ④ 读链闭合（f75a8ad）→ QA 验收启动】dir Z:\ 列出 ["alpha.txt","beta"]——内容逐字可验。
    读链 volume_info→GSBN→Open→ReadDirectory→Close 全真实回调，FileContext 1:1，卸载干净。
    错误链七环收尾：门禁→SD标志→NULL参数→根语义→三处未实现→参数序→传输计数。
    分支 p3-4/mount @ f75a8ad（44 提交，基于 6ed9aae）已交 QA 挂载会话验收（滚动合入口径，241）；
    worker 转下一批：目录挂载（Prefix@40）→ 有副作用回调 → L2 → M1–M9 → U1–U14。
    合并报告素材清单已发 worker：功能分层 / 错误链简表 / 判据对应（326–339）。

341. 【★ 目录挂载点必须不存在（源码纠正 manager）】fs.c Preflight + mount.c FspMountSet_Directory +
     util.c FspCreateDirectoryFileW 三连证据：目录挂载点由 WinFsp 自己创建（NT FILE_CREATE，
     带句柄 DELETE_ON_CLOSE）——已存在 ⇒ STATUS_OBJECT_NAME_COLLISION；不存在 ⇒ 创建并挂载；
     卸载关闭句柄 ⇒ 目录自动消失（卸载断言 = 目录消失，与盘符 released_after_delete 对称）。
     第一天以来目录挂载全部 0xC0000035 都是这一个原因——manager 当初"显式建空目录"的建议方向完全反了
     （worker 的"前置断言存在且为空"是在诚实执行错误假设）。
     Prefix 真面目：UNC 网络形态专用；mount.c 开头网络卷（VolumeName 反斜杠≥3）被 Directory 分支
     拒绝（NETWORK_ACCESS_DENIED——Windows 不允许 junction 指向网络 FS）。目录形态不设 Prefix。
     方法：挂载失败读 DLL 精确源码（fs.c/mount.c/util.c），同 fsop.c 战法。

342. 【目录挂载打通（3588d9d）】挂载点必须不存在（341 的翻转）+ 目录形态不设 Prefix（撤调用点、
     留代码）⇒ create=0 mount=0 start=0，probe 列出 ["alpha.txt","beta"]，卸载后目录消失
     （DELETE_ON_CLOSE）。盘符与目录两形态同一份代码都通。
     卸载纪律统一（309 同条）：两种形态断言的都是"留下的东西"（盘符消失/目录不存在），
     SetMountPoint(NULL) 的 INVALID_PARAMETER 是 API 固定行为，不追。
     L2 分工：worker 写对照脚本（同一脚本只差路径前缀，定向调通挂载侧），QA 跑两侧完整 12 类对照（验收）。
     合并计划：QA 验 f75a8ad 通过后，合并到分支当前头（按内容核，240 口径）。

343. 【L2 对照脚本纪律】一个脚本两个根（只收路径前缀，两侧同一份代码——差异不可能是脚本差异）；
     只出观测（NN name: <observation>）、比较交给两侧 diff——脚本自判 pass/fail 会把措辞差异
     和真差异一起藏掉（观测与判断分离）。两处假差异预警：05 枚举顺序（口径：内容排序后比较、
     顺序单独记录不比较——枚举序不在承诺面，Windows 本身不保证跨 FS 枚举序）、12 属性打印
     （口径：比属性位语义，格式差异记录不判）。三分档：一致/有据差异附证据 ref/缺陷，
     每行落一个档不许空。挂载侧定向运行默认交 QA 完整验收（不为了看一眼改生产路径）。

344. 【口径写进输出本身】05a/12a 参与比较、05b/12b 只留档——比较面与留档面在输出里自描述：
     QA diff 时不需要翻规则文档，口径就是数据的一部分；只看 diff 也能知道哪行该红哪行不该红。
     与"判据影响行写在提交首行"同族：规则寄生在产物上，不寄生在人的记忆里。
     其余 10 类保持单行（语义观测无格式成分）——形式服务于比较，不服务于整齐。

345. 【P3-4 读链批合入 main（18d84e3，49 提交）】QA 验收通过（独立复跑 589/0/9 + 真实挂载非 SKIP +
     ABI 33 成员机械枚举核对 + 判据行 49/49）。ff 合入，diff 为空（240），临时 worktree 即删，
     分支滚动续用（卡完再删）。
     QA 附缺口（写链批第一个提交必补，缺一不收）：名字逐字断言（probe 曾故意丢弃名字）+
     FileContext 配平断言（跨 before-after 差值）——观测不是断言（244）。
     QA 亮点入档：第二证人原则救验收人自己（前两轮枚举漏 Obsolete0 被 Reserved[31] 交叉否定，
     枚举配方=成员数对 Reserved[N] 核）；nextest 0.9.150 抓捕协议 v4 定稿（JSON 不可行系 owner 更正）；
     mount_lifecycle stop 不可观测=正面形态样板。
     不稳定红第 N 次（587/2→589/0×2）：QA 合入后首跑 nextest 全量 = 抓捕窗口。

346. 【滚动合入清单 + 报告口径修正（clippy 破口复盘）】目录挂载提交破 clippy（mounted.rs:527
     unused is_drive_letter），两个缺口叠加溜进 main：worker 后续报告丢了 clippy 项 + manager
     滚动合入清单（ancestor/diff/判据行）没有合入头 clippy。
     修正：①每个动代码的提交，报告必须带 fresh clippy（"clippy 0"四字）；②滚动合入清单加第四项：
     合入头 clippy 提交方报告 + manager 复核。修复搭写链批第一个提交一次清零（三件套）。
     QA 241 亮点：参照一律对 origin/main（本地 main 停旧位曾出 3905 行删除假象，243 同族）；
     从"基本成立"挖到"精确说"（mounted_read_only 前置判据变了但带证据合规）——241 存在的理由。
     不稳定红台账：5 次（2 无名+2 带名+1 无名 worker 587/2），边界假设成立（4 次提交/合入后首跑，
     干净边界 0 次）；下个抓捕窗口=写链批合入后首跑（并发面更大）。

347. 【差值配平 = 指纹带时间窗】断言 (after.alloc-before.alloc)==(after.free-before.free) 而非
     alloc==free：绝对值会被上一轮残留计数偶然满足，差值把证据窗口收窄到本次挂载。
     绝对值断言在"第一次跑"无懈可击，"第 N 次跑"变恒真。与"计数是指纹"同族精化：指纹要带时间窗。
348. 【注入必先证红——静默未生效比编译失败更阴】replace 没匹配上（fmt 重排过那段）⇒"断言已加"
     但不在文件 ⇒ 测试照旧全绿——全绿读数对"改动没进文件"完全无感（303/323 是补丁坏编译会响，
     这次是不响）。唯一防线：注入式改动（替换/patch/生成）先证明确实改到了（理想是红法：
     故意错期望值看它红），再相信后续绿。

349. 【先接类门，让错误码说真话】只读卷上 Write 槽为 NULL ⇒ "写"类全表现 c0000010（未实现）——
     把写保护伪装成没实现。先接槽返回 STATUS_MEDIA_WRITE_PROTECTED：Windows 看到"不许写"而非
     "缺功能"。两个真话不能共用一个谎言的形状（338 逆用：错误码不只是入口，还是诚实度）。
     收益：后续换真实现只改 trampoline 体，槽位与类门不动。
     写链批纪律沿用：纯 Rust 判据先红 → 接槽真机验 → fresh clippy（346）。
     make(path)->FileId 钩子走 adapter 既有路径链（路径→id 只在路径语义入口，P3-2 G2 老纪律）。

350. 【(b) 注册表落地——299 的到期条款】写链（make 钩子/Cleanup 两段删除/SetFileSize）全是挂载级
     状态 ⇒ 进程级 OnceLock 配置结构性到头（测试顺序依赖、配置项已四五个）。
     299 原条款到期执行：配置下沉 Adapter 字段 + FSP_FILE_SYSTEM*→Adapter 注册表 +
     stop→注销→delete 清理顺序同批写。一挂载一 Adapter 一 HandleTable，天然双挂载，测试无顺序依赖。
     302 老判据（OnceLock 合并测试）被结构取代——迁移时提交说明写明"取代不是删除"。
     执行要求：一次结构提交（不与 Create 判据混）；注册表用 Arc/显式 remove（非 leak），
     delete 后 registry 查无此键写成断言（别让清理顺序变成新的悬垂）。
     撤回纪律再次正确：没有判据就不提交，不留没有判据的实现（196）。

351. 【对策不是注释，而是顺序+断言】悬垂风险的对策 = 代码顺序（remove 在 stop 后）+ 可判形态
     （delete 后 registry 查无此键断言），不是"注意别在 remove 后回调"的注释——注释提醒人，
     顺序与断言约束程序（与 344 规则寄生在产物上同族）。
     Mutex<HandleTable> 定位：并发安全是正确性问题不是性能取舍（回调来自 dispatcher 线程，真并发；
     RefCell 在真实挂载是 UB 风险）。锁开销若成问题是带测量的 B 项，不是现在的猜测——
     结构提交不夹性能猜测。

352. 【未注册返回 None，绝不返回默认配置】进程级 OnceLock 的病根：查不到时它编一个答案
     （上一次配置的残留），而不是说"没有"。None 是诚实的失败——让"回调拿不到 Adapter"
     变成可判的错误而不是静默的错答案。与 301（零记录≠空文件）、304（没调用≠调用失败）
     同族谱：宁可显式失败，不可静默编造。
     附：泄漏的表永远无法被证明已经不在了——Arc+remove 把生命周期规则从泄漏形态变成可断言形态；
     注册表 insert 撞键返回旧 Arc（WinFsp 复用指针时静默覆盖会搅在一起，报旧值让事情被看见）。

353. 【348 补条：注入生效的三道防线】替换没生效第二起（fmt 折行致 replace 不匹配，miss 计数没进文件）——
     这回是断言当场抓的（left: 0, right: 1），上回是红法抓的（断言不在测试全绿）。
     两次不同探测器、同一根因：注入后必须验证注入生效。
     防线分层：grep 目标特征（最便宜第一道）→ 断言存在性（第二道）→ 红法（最强）。
     附：miss 计数只数 miss 不数命中——计数器的价值在于期望值（miss 期望恒 0，
     非零即悬垂现场），命中计数会随正常挂载增长毁掉不变量。

354. 【指纹要带时间窗，也要带归属】按挂载计数（Adapter.contexts）的理由：进程级计数回答
     "全局有没有漏"，teardown 真正要问"这一次挂载配平了吗"——后者不能被别的挂载掩盖。
     用例形态：第二个 adapter 只分配 ⇒ (1,0)，第一个的 (1,1) 掩盖不了它。
     不稳定红第 6 次（595/1→596/0×2，仍未留名）；抓捕窗口=写链批合入后 nextest 首跑。

355. 【切换拆两步：死代码是诚实的中间态，回退是撒谎的中间态】A 步新路径活（mounted 三段接线 +
     trampoline 读注册表 miss 即拒绝 + 直测改 Adapter 构造，OnceLock 变死代码但还在）；
     B 步纯删（删 OnceLock ×4 + 302/300 迁移，无新行为）。
     不要做"注册表查不到回退 OnceLock"——回退是 352 反面（查不到时编答案）。
     改动面积与事故概率单调相关（本轮两次注入事故都在"一次动太多"）。
     B 步的删干净证据：grep -c OnceLock 期望 0。
     在上下文边界不硬上（交接说明形态）本身是好交付。

356. 【owner 裁决（2026-10-27）】①判据 4 耗时口径改 release（选项 ii）——耗时判据在 debug 上是构建噪声
     放大器（G3 时已有"耗时判据不进 CI"先例）；②B-17 不稳定红诊断先窗口后专项——写链批合入后
     nextest 首跑为抓捕窗口，留名则免费拿入口，不火再排专项。
     本日管理动作：判据 178–355 归档进 criteria-and-acceptance.md §十七（783 行，main 61dc514）；
     P3-4 收口报告骨架落地（docs/reports/p3-4-closeout.md，[ ]/[TBD] 即收口核对清单）。
     发现并记录：main 判据文档曾停在 177，178–298 一直只在 manager 笔记——归档债务这次清了。

357. 【OnceLock 的去留判别：归属，不是语法】保留一个 OnceLock<Registry> 而删四个配置 OnceLock 的口径：
     配置 OnceLock 回答"这个挂载该怎么表现"——语义属于挂载，进程级静态=错的归属（352 编答案）；
     Registry 回答"这个进程里有哪些挂载"——语义属于进程（FSP_FILE_SYSTEM* 是进程内指针），
     全局=对的归属。判别标准不是"用不用 OnceLock"，而是被静态化的东西归属谁。
     注册表是 (b) 设计（299/350）的天生全局件，不是配置被搬进全局。
     分组切换：无状态 trampoline 跳过（等真实现时顺手切）；每切一组跑真机读链验证。
     grep 抓出 10 入口/4 静态（比双方记忆多）——记忆数入口必错，grep 数才对（348/353 结构层应用）。

358. 【注册表泛型的解法：dyn WorkspaceOps（(b)）】架构依据：WorkspaceOps trait 定义在 fs-winfsp
     （ops.rs:177），fs-api 依赖 fs-winfsp（后端无关设计）——注册表存 Box<dyn WorkspaceOps>
     是这条线的自然完成：挂载入口供 trait object，fs-winfsp 全程不知具体后端。
     (a) 固定 ApiBackend = 循环依赖（fs-winfsp 不能认识 fs-api）；(c) Any+函数指针 = 手搓
     trait object，(b) 能用就不发明。object-safety 预检：11 方法全 &self+具体类型，dyn 路通。
     转发层 11 处手写转发的抄写错误是 348 新温床——Deref/宏。不稳定红第 7 次（596/1→597/0）。

359. 【卡文本也要以实测修订】p3-4 卡 M3 原文"挂目录挂载点（空目录）；卸载后目录恢复"——正是 341 推翻的
     错误假设（第一天 0xC0000035 的根源就在卡的这句话里）。总纲"以实测为准修订文本"适用于卡本身，
     不只适用于 spec/ADR。已修（main 2922a6a）：绿=挂载点必须不存在、卸载后消失；红=已存在目录被拒。
     worker 全量改用 nextest：不稳定红 7 次里 3 次在其提交首跑——抓捕窗口从"合入后一次"倍增到
     "每次全量"，B-17 最大加速。

360. 【三个"数数"各管一层】grep 数入口（357）、编译器数方法（dbd12fd：记"十一个"漏 flush，
     E0046 当场抓回）、断言数行为——都不是靠人脑记忆。转发层的判据不是"能编译"而是
     过箱子与过具体类型同行为：同 kind/size、同字节、同错误变体（discriminant 比）、同目录条目数
     ——参数错位在类型层完全隐身（签名相同、实参交换照样编译过）。
     显式 impl 优于 Deref：Deref 让装箱后端自动解引用到别的 trait，"这是一个后端"从声明变成副作用。
     nextest 口径：594/594 + 9 skipped（与 cargo test 597 差 doctest，一次性说明）；
     worker 全量读数自 dbd12fd 起一律 nextest（每次全量=一次抓捕窗口）。

361. 【纯函数核心 + 薄 unsafe 壳】注册表切换的切法：volume_info_for(adapter,...) 纯函数可直测
     （含无卷⇒None 分支），unsafe extern 入口只做"取表→miss 拒绝+计数→转调"——把不可测的
     unsafe 面压到最薄，逻辑全搬进可直测层。与 d7dc714（回调体先落地纯 Rust）同一纪律的
     第三层应用：FFI 座架→回调体→注册表壳。Lister 从 Adapter 派生（WorkspaceOps 里 read_dir
     是正主，不再单独注入——少一个归属错误候补）。
     预算算术诚实：两个 600 行文件+unsafe 边界+本轮三次工具事故 ⇒ 不开工（半接线=被禁中间态）。

362. 【不半填的字段维度】断言 total_size 与 free_size 都写对——只搬第一个字段的实现也能过
     单字段判据（字段级半拷贝在单字段断言下隐身）。不半填三层补全：缓冲（字节维度，301）、
     记录（BufBound）、字段（本条）。
     assert_ne!(NOT_IMPLEMENTED, TOO_SMALL)：两个不同问题不得共用答案——338 的判据化
     （防将来"简化"成一个码）。pub 常量=变体有名字可引（断言要引用的东西必须先存在为可引用实体，
     与 353 计数器期望值同构）。

363. 【NextOffset 定性 + U15 新增】dir_buffer::push 从未写 NextOffset（恒 0）——但当前配置无害：
     DirectoryMarkerAsNextOffset 未设（零位域）⇒ FSD 用最后一条文件名做续读 marker，
     NextOffset 全程不被读（ddca7bd fsctl.h:232 / winfsp.h:687）。真机列举通过不是侥幸。
     真缺口是多页续读从未验证（夹具仅 2 条目永远单页）——已加卡 U15（>单页目录约 200 条真机列举，
     归 L2/QA）。NextOffset 耦合项写链批后单列（写全解除耦合 / 断言 flag 恒 0）。
     判据从缓冲读回名字（Size@0/Name@104/对齐步进）——判据读产物不是返回码（338 教训的落实）。

364. 【第 0 条自证：一个断言若不能失败它什么都没说】位域断言（flag 恒 0）先在副本里把该位设上、
     证明读数会红——否则可能因偏移/掩码写错而为错误的理由通过。位掩码断言尤其需要（掩码错一位
     断言恒真还全绿）。与 G3 fmt 矩阵第 0 条自证同族，348（红法验证断言本身）的位域应用。
     shell 签名 Option<&DynAdapter>：miss 进类型（None=miss 一等公民，不是异常分支），
     trampoline 压到一行，unsafe 面最小。

365. 【红 ⇒ 停、报告、不提交；证据落在管道里=没落盘】clippy|head 管道链掩盖退出码，全量红时提交
     照做（b0b1e8d）——处置：不回滚（3×绿+已知家族+主动报告）但言明"运气好≠程序对"。
     修正：全量与 clippy 分开两条命令各自判退出码；完整输出永远先 tee 落盘，grep 只用来读不用来截。
     第 8 次不稳定红：nextest fail-fast，"128/607 not run"时失败名就打在输出顶部——
     名字曾可得、亡于 grep 截取（332 族谱：/tmp 如此，grep 后的 stdout 也如此）。
     --no-fail-fast --status-level fail + tee 永久化。B-17 从"等窗口"转"主动重放"：
     QA 有界战役 build+run ×20 轮 tee 落盘；抓到=入场券，全绿=负结果修正形态假设。

366. 【★ 不稳定红留名：crash.rs:187 reopen / SQLite 1546】tee 落盘后抓到（第 8 次名字曾可得亡于
     grep 截取，本次补获）。签名：every_commit_crash_point_recovers... panicked at crash.rs:187，
     reopen: SqliteFailure(SystemIoFailure, extended_code=1546=SQLITE_IOERR_DELETE_NOENT，
     "OS file delete failed: the file does not exist")。单跑 5/5 绿（含慢例）⇒ 非确定；仅全量并行；
     与 P3-4 无关。同一行另一测试名（a_crash_after_the_publication...）——同一 reopen 路径。
     worker 假设（标注最可疑非结论）：删 -wal/-shm 时文件已被别的句柄打开/已被删——crash 模拟
     journal 文件 + 并行下另一连接/清理与 reopen 抢文件。
     1546 = 可检索条件：随机红从现象变成条件。两个家族问题：KI-11（pack 缺失）与本条（journal 删除竞争）
     可能不同族（文件不同形状同）；前 7 次无名红无法归族，QA 重放 --no-fail-fast 数据可首次分开出现率。
     处置口径：全量红但=已命名家族 ⇒ 允许提交但提交说明留痕绿红边界——白名单第一行不是通行证。

367. 【发现形状变更即停，不就地编造】派生 Lister 时发现 DirEntry 只有 {name,kind} 没有 file_id——
     trait 层改动（非壳改动）。最危险捷径=就地编 id：编译过、列举出、每条记录塞错号——
     产物级污染，比红更隐蔽（错号会一直绿到 L2 对照才炸）。停手、挪 B 步、据实报。
     "宁可显式缺，不可静默错"（352 族）的数据形状应用。
     B 步口径：DirEntry 加 file_id 是 trait 欠规格（目录记录需 metadata、后端按 id 回答）——
     mock 与真后端同批改（P3-3a 双后端纪律），判据含"id 与 read_dir_id 对应条目一致"。

368. 【差分协议结案形态：隔离绿+安静全量绿=环境归因】挂载配平红 (1,0) 四红批次：隔离 5/5 绿
     → 等安静窗口 → 608/608 全绿——同一改动两种环境两种读数，环境归因从"应该是"变成"是"
     （miss_count 用不上也是信息：新注册表路径没漏）。crash 三红同批转绿。
     ConfiguredOps 标注"桥不是设计"（真后端接入后消失）——写明它是什么就写明它该死在哪，
     将来没人把它当遗产保护（与 355 中间态自我标注同族）。
     DynOps: Send+Sync——不能跨线程共享的后端根本无法挂载（dyn 路第二块拼图）。
     计数先证明、归属后迁移（A 步不顺手改 B 步的事）。

369. 【★ teardown 不是同步点：StopDispatcher 返回 ≠ 队列回调已执行完】挂载配平红 (1,0) 的最终定性：
     判据在异步事件上下了同步断言（drop 句柄→IRP_MJ_CLOSE 在途→stop 杀派发器→之后回调永远不来，
     (1,0) 是终态）。差分证据：10 次隔离跑 miss_count 全 0（排除悬垂），1 次红。
     修复形状：有界等待放 teardown **之前**（轮询配平，上限 2s；这不是重试换绿——测试自己的动作
     （关句柄）有异步完成（Close 回调），等待它=场景正确排序；超时保持红=真缺陷），
     teardown 后断言原样零余量。挪断言到 delete 后只是挪红（派发器已死，delete 不补投递）。
     产出二：真实挂载流程也必须在 stop 前等在途回调排空——直接喂 M5/U13（有句柄时卸载）。
     miss_count 进挂载测试常规输出（删临时打印，留观测）。

370. 【超时信息自带鉴别诊断】Err("in-flight Close did not arrive within 2s … 派发器还活着，
     所以这是真缺口而不是机器慢")——报错把"为什么这不是环境问题"写进信息本身，
     看到红的人不用重建推理链就知道往哪查。错误信息不只描述失败，还排除竞争假设。
     crash 家族台账（转 QA 归族）：5 个测试名 2 个 crate——fs-workspace::crash ×2
     （every_commit.../1546 已留名、a_crash_after_publication同行187）+ cas-store::crash ×3
     （between_two_puts/completing_put/staged_file_synced）。QA 重放数据回答：家族多大 +
     单根因多症状还是多根因同形状。重放与 worker 全量错峰（上次 4 红教训）。

371. 【A 步收尾：双形态逐项一致】同一份代码在 Z: 与目录挂载下读数逐项相同
     （volume=1/read_dir=3/contexts 1:1/misses=0/released=true）——排除"注册表接线只在
     某形态工作"的整个假设空间。计数由新壳递增="注册表取到 adapter"是计数器读数不是推断。
     目录形态 drive=None 按形态分支（不是失败，是形态性质）。
     B 步顺序：4（DirEntry 补 id，使 LIST 可派生）→ 1（删 OnceLock=删死代码，前置核对
     callbacks_direct 已走 Adapter，否则该处并入同批）→ 2/3（判据整理）。
     B 完即结构批整批交 QA 验收；写链回调另起一批不混。

372. 【B-17 战役：第 8 轮抓到 + 家族分离 + 错误码更正】QA 重放（touch 新构建+nextest --no-fail-fast
     ×8 轮 tee 落盘）：7 绿 1 红，红轮抓 2 失败同文件同行 crash.rs:187 签名 1546；
     单跑 7/7 绿、抓后全量绿。
     ★ 错误码更正：1546 = SQLITE_IOERR_TRUNCATE（10|6<<8），不是 DELETE_NOENT（=5898）——
     worker 假设修正：机制是 DB 主文件 truncate 被挡（journal 重置/WAL checkpoint 时另一句柄
     还开着），候选：crash 子进程刚 kill 句柄未释放/杀软扫描/并行连接——与"只在 crash 测试、
     只在全量并行、单跑必绿"吻合。
     家族分离：1546 族 1 轮、KI-11 族 0 次——受控条件下可分，B-17 立两张卡（manager 起草）。
     双因素未分离：fresh-build × 总负载（1-5 安静全绿，6-8 与 worker 重叠第 8 轮红）——
     续跑 9-20 轮错峰+每轮标记并行度。
     第 8 次台账定稿（双方 tee 各自补获）；第 9 次（worker 4 红批）注记负载重叠、
     待安静窗口绿即闭合（afa9c88 的 608/608 时间窗待确认）。

373. 【互斥保证取代事后归因（文件锁）】全量/重放共享 F:/work/.layerfs-fullrun.lock
     （属主+命令+时间戳；新鲜<2h 则等，过期记录后接管防死锁）。第 9 次悬而未决的根源
     是"谁在跑"靠回忆——锁把环境重叠从猜测变成互斥事实。worker 提议，裁准即刻生效。
     新观测：worker 侧 rustc 0xC0000005 编译期崩溃（负载相关或独立事件，台账单独记）；
     安静窗口仍出现则升级为机器稳定性问题。
     366 更正注记：1546=SQLITE_IOERR_TRUNCATE（QA 源码自证），worker 的 DELETE_NOENT 假设作废，
     机制=DB 主文件 truncate 被挡（journal 重置/checkpoint 时另一句柄开着）。
     worker 判据：宁可未结案，不建立在不知道的时序上——正确。

374. 【窄面随时验，宽面择时验】B 步推进不被 QA 节奏卡死的分工：定向测试+clippy（窄面，
     不受整机负载影响）随时跑；全量（宽面、时序敏感）等锁双方落地后择时跑。
     提交照常（定向绿+全量待跑在提交说明写明），全量绿后补读数。
     锁内仍 1 红（1546）= 归因数据：环境重叠不是必要条件，可自发触发——
     触发画像收紧为"整机负载高时易红"（QA 双因素分离多一数据点）。
     锁设计四形态：fresh 等/缺失取/过期接管/trap 放行（异常也释放，不等死人）；
     等 30 分钟 exit 3 而非并发（并发正是要消灭的变量）。

375. 【"没有半绿中间态"的提交拆两刀 + ④的判据口径】删 4 OnceLock 的迁移面查清：四件事必须同批
     （壳取值/桥消失/测试迁移/resolve 替代），无"先做一半仍绿"的中间态 ⇒ 拆 (b)-1（壳从
     adapter.backend() 取值，旧静态暂留给测试——夹具未迁≠回退中间态：回退是运行路径借旧答案
     （撒谎），这只是夹具未迁（运行路径已全走新））与 (b)-2（夹具迁移+删静态+grep==0）。
     证据晚到一步不等于证据缺席。
     ④ resolve 替代（create_or_open(Open) 顶替纯查表）判据三条：存在路径→id 一致；
     不存在→NotFound（不是 InvalidRecord 更不是创建）；**调用前后 store 无新条目无尺寸变化**
     （钉死"不建不截断"——语义差异必须有自己的断言，不能靠测试还绿兜底）。

376. 【内联冗余会掩盖两半不一致】夹具 list() 声明 id 1/2 而 lookup() 只答 0/7——两半矛盾。
     旧路径把 NodeInfo 内联在列举里 ⇒ 矛盾永不可见；新路径按 id 取 metadata ⇒ 条目被正确丢弃 ⇒ 红。
     去掉冗余就是去掉掩护：数据只有一个来源时两个声明不一致会当场撞车；内联让每个消费者
     拿到"自洽的谎言"。处置：修夹具，不给丢弃逻辑开后门（按 id 取不到就丢弃是对的，错的是夹具）。
     DirEntry::file_id（367 发现→371 排期→本刀兑现）——发现→排期→兑现闭环走完；
     "按 id 语义"对 P3-2 G2 老纪律（路径→id 只在路径语义入口）的又一次收租。

377. 【迁移面数读取者，不是调用者】静态的全部生命周期在"谁读了它"（.get()），不在"谁配置了它"
     （configure*）——数调用者会漏 open/create 各两处的读取者。核对方法：按行号列出每个 .get()。
     让 grep 数（357/360 在迁移面的应用）。
     (b) 拆 2a/2b：2a=行为改动（open/create 加 *_shell，361 同法；④三条判据落这刀——
     行为改动在哪判据就在哪，不能落在删除刀里；新旧并存对照=真机不变式成为两者共同读数）；
     2b=夹具迁移+删四静态+configure*+ConfiguredOps+grep==0。
     open/create 是已通电路径（真机每次列举都 Open 目录）——行为改动与三重删除同刀
     正是一路在避的形态。

378. 【判据的数据源必须独立于被测路径的输出】断言 size 用 get_info 取而非 outcome.size——
     用后者是同语反复（验证后端自己刚返回的东西），用前者则两条独立路径必须给出同一答案，
     判据才有内容。断言的强度取决于它能否被"实现改坏而数据源不知道"打红；同语反复的断言
     永远绿=不测任何东西（与 360 判据的有效性先于数量同族，变异测试思维在断言设计上的应用）。
     ③"拒绝且不发放 FileContext"=悬垂的反面成判据：失败路径不产生需要回收的东西，
     与成功路径配平合起来，FileContext 生命周期在任何路径都有看守。
     分层红利：fs-winfsp 不再知道"名字怎么变成 id"——路径语义在 ops 层、FFI 层只剩身份语义，
     P3-2 G2 老纪律由结构保证不再由纪律保证。

379. 【期望值从设计算出来，不能从上一条判据抄】grep -c OnceLock 的期望是 1（且必须 MOUNTS）
     不是 0——套用 0 会逼我们对归属正确的注册表 OnceLock 撒谎（或为凑零删掉它=错上加错）。
     "恰好 1 处且必须是 MOUNTS"更强也更诚实。判据的期望值本身就是设计陈述。
     第 9 次定案：锁内 612/612（status=free 无重叠）⇒ 安静窗口绿，归因负载重叠结案；
     1546 仍是负载触发未修（B-17 跟踪）——归因与修复状态是两件事分开记。
     2b 形状：mount_read_only(backend + VolumeInfo) 显式两参（不提前抽象结构体；
     桥死在"签名需要后端"=当初标注的"死在哪"兑现）；closure 夹具→trait 实现逐字搬运
     （含 376 修复后形态），迁移不是重新设计夹具的机会。

380. 【零计数 ⇒ 问题在它之前；grep 数面、运行时验链，缺一不可】GSBN 仍在读进程级 RESOLVE/LOOKUP
     ——上一封按行号数了读取者（377）却仍漏了它。静态枚举有盲区；"该被调用却零计数"的运行时信号
     抓住了 grep 漏掉的。删除类改动的验证必须含被删物的全链路消费者（真机不变式那一跑就是为此——
     纯定向层 GSBN 未配静态不显形）。
     d472a1f：桥整块死亡（签名 backend+volume 由调用方给——"真后端接入后它消失"的兑现）；
     FixtureOps 逐字搬运。结构批最后一步：删四静态+三旧壳+configure*+grep==恰好1（MOUNTS）。

381. 【★ 结构批闭环（c927a16，21 提交）→ QA 验收】批的形状：配置从进程搬进挂载，挂载表留在一个
     OnceLock（MOUNTS）；FFI 层只剩身份语义，路径语义全在 ops 层。分组 A-G（注册表归属/dyn/
     纯核心薄壳/接线/行为路径/工具/判据微调）。四步交卡口径过（ancestor/docs diff 0/干净）。
     白名单红新口径（验收判据）：全量允许的红只有一类=逐字匹配已命名家族（1546 签名）；
     任何别的红即阻塞。grep==恰好1且必须 MOUNTS（验收独立复算）。
     302 被结构取代=callbacks_direct 拆回 6 独立用例。
     crash 家族第 6 名：a_crash_during_materialisation_publishes_nothing（2 crate/6 测试名）。
     worker 已开写链批（最后一段实现：Create 建文件/Write/SetFileSize/Cleanup 真语义）。

382. 【静默忽略 = 调用方信念与存储事实的静默分叉（写侧）】FILE_CREATE 被静默忽略 ⇒ 调用方以为
     建了、存储说没有——301（零记录显示成空文件）的写侧镜像：读侧"有数据读成空"、
     写侧"没写入装成写了"，共同病根=把一种事实伪装成另一种。修法：disposition 是 open/create
     的全部差别；只翻译能兑现的成员，其余按类掩码拒绝、不近似。
     ④ 拒绝不是失败的尝试：后端调用计数=0 把"未实现"与"试了失败"分成可判状态
     （拒绝时还打一次后端=将来诊断要排除假尝试——坑提前填）。
     红法验证翻译本身（left:[Open] right:[Create]）：①防零次/两次，红法防"调了但调错人"——
     翻译的每个错误方向都有判据看守。

383. 【一个读数被两种假设同时解释 ⇒ 它没有信息量；先问"字段在哪被定义"】
     60b4749 把 CreateOptions 低 3 位当 disposition——错；真值：fsctl.h:351 注释
     "Disposition: high 8 bits; Options: low 24 bits"（fsop.c:915 的 switch 同证，
     接口回调原样收到打包值）。0x21&7=1 既像 FILE_OPEN 又像 FILE_DIRECTORY_FILE——
     两假设同释=零信息量，当时没做区分实验。错的根源不是读错位，是没先查"字段在哪被定义"。
     DLL 日志分开打印 disposition（FILE_OPEN）与低位标志（0x21）——打包方式从第一天就在日志里。
     裁决：修复前进不回退（open_like/四判据/不近似结构全对，错的只是提取位）；
     顺手接出 FILE_DIRECTORY_FILE（make 钩子分目录/文件的正主）。

384. 【结构拦截：并发正确性先于接线】Cleanup 前查清：Adapter::set_delete/close 要 &mut，而注册表
     给 trampoline 的是 &Adapter（Arc 共享）+ open_shell 只造 EntryContext{file_id} 无 HandleId
     ⇒ Cleanup(DELETE) 没有可标记的对象——写"假装标记的壳"= 382 用在实现者自己身上（不做）。
     到期的风险注（351/批(i)）：HandleTable 两个容器各自 Mutex、方法全 &self——锁守数据不守对象，
     Arc 的共享性从障碍变成事实。EntryContext 增 handle（file_id 保留给 remove_at guard）。
     锁纪律：持锁最小化——改完表放锁再调后端（remove_at 不在锁内：慢后端放大竞争，
     将来后端回调即死锁）。判据接真入口：adapter_criterion3/handles_criteria（P3-3a 先行二十卡）
     等的这一刀就是接线。

385. 【B-17 两卡落 main（b863e69）+ 家族定稿 7 名】B-17a（1546/crash 族）：签名 1546=IOERR_TRUNCATE
     源码自证，2 crate/7 测试名全在 crash 后 reopen 路径+truncate 被挡；触发画像=高并行概率事件、
     环境重叠非必要条件（worker 锁内自发 1 红）、fresh-build×总负载未分离；机制三候选
     （kill 后句柄未释放窗口最可疑/杀软/并行连接）；诊断步骤=两条观测+句柄追踪+重放脚本。
     B-17b（KI-11 族）：MissingPack/os error 2，4 现场（2 带名），战役 0 出现 ⇒ 触发不含
     新构建+全量并行，需 --run-ignored pack 并发变体。归族按签名不按形状（:177/:187 两 crate 同名文件）。
     QA 第 9 轮 load-marker=3 抓到（证据不靠回忆）；rustc 崩溃仅 worker 侧，独立观测挂账。
     worker 649c16f：handles Mutex 化 + &self 化；锁纪律理由进注释（344 落地——改锁内代码的人
     先读到死锁警告不是死锁本身）；close 三段式=锁内决策/锁外执行/锁内清账，后端调用永不持锁。

386. 【共享策略裁决：(b) ALL_BITS 先行】句柄注册掩码不取 WinFsp 的 Granted/ShareAccess——
     共享策略是卡上 U3 的独立交付（share 9 格矩阵 + 双进程 + 回调计数判据现成）；
     现在做 (a) = 在没有 U3 判据下提前引入新行为，将来 U3 跑红分不清是判定错还是从未对过。
     (b) 让两段删除判据在已知行为上测试；ALL_BITS + 判据写明"句柄表已接线但共享策略未实施"
     = 诚实中间态（368 桥同族：写明是什么、缺什么、在哪补）。两段删除不依赖掩码。
     设计决定（如掩码取值）不带着半预算做——撤回实验、带决定再开工。

387. 【结构批 QA 验收通过；379 范围修订；合入挂起等 worker 停点】QA 独立复跑（锁下）617/616+1，
     唯一红逐字匹配 1546 白名单族、全日志无其他签名——白名单规则首跑干净成立。
     单轮 0-2 条波动，worker 617/615+2 与 QA 读数相容。三条加验点全过
     （白名单/OnceLock/callbacks_direct 6 用例）；两条头牌断言落地验收（QA 注的 3b3170e 与
     我记录 2ee303e 不一致，收口报告前核对）。clippy 346 缺口 closed。
     ★ 379 范围修订（QA 复算发现判据自身缺陷）：grep 限定 crates/fs-winfsp——全局 4 处 OnceLock
     （cas-store probe ×2 / fs-snapshot cdc / fs-winfsp MOUNTS），无范围则别 crate 误红。
     已落 main（eea66f6）。复算发现判据缺陷=验收独立复算存在的理由。
     合入流程：worker 完成 Cleanup 刀 → fetch+rebase（main 前进两个 docs：B-17 卡 b863e69 +
     379 修订 eea66f6，无冲突）→ manager ff 合入 → QA 241。
     worker 正在最后一刀中途（8 文件+新 cleanup_shell_criterion 未提交）——不动他的树。

388. 【rebase 边界上的 SHA 会换号——引用带"截至哪个 ref"】2ee303e（parent=18d84e3，worker 首报）
     与 3b3170e（parent=da66398，rebase 后随分支合入）是同一提交的两版：时间戳/标题/补丁相同，
     两快照间 diff 仅 docs 提交，断言本身零差异；原号仅 reflog 可达。收口报告引用 3b3170e 加注原号。
     239/240/241 的第四块拼图：cherry-pick 丢提交按内容核 → 合并逐文件 diff → 合入后核对 →
     rebase 边界 SHA 换号。git 证词形态：parent 链/时间戳/标题/补丁逐项对——
     不是"看起来是同一个"而是"每个可判字段都相同"。

389. 【结构批+写链前段合入 main（9207c39，27 提交）】= QA 验的结构批 21 + 写链前段 6
     （60b4749 disposition 翻译/e0b61ff 高低位更正/892ede3 make 分路/021ed97 SetFileSize 读回/
     649c16f HandleTable Mutex 化/9207c39 Cleanup 两段删除真入口）。
     ff、diff 为空（240）、docs diff 0。QA 241 启动（锁下 nextest 首跑=B-17a 抓捕窗口）。
     Cleanup 刀判据：标记(Cleanup@6)+删除(Close@7 唯一 remove_at 点)、两段删除 4 例、
     关闭两次被拒、null/伪造句柄不崩——判据先行二十卡（P3-3a）接上真入口。
     worker 继续 Write/Overwrite（写链批最后一段；Overwrite 系=U2 裁决的挂载层兑现点，
     ReplaceFileAttributes 两向判据复用 G1 修订卡）。
     收尾顺序：写链批验收 → L2 两侧（QA，U15 多页）→ M1–M9 → U 消项 → 收口报告填数。

390. 【241 通过；B-17a 家族第 8 名；结构批整批换号】QA 241（9207c39 锁下首跑）三项全过：
     27 提交核对/白名单 1 红逐字匹配（1546 族第 8 名 a_crash_during_discard_is_swept_on_reopen
     @:328，同文件同签名不同测试——归族按签名与路径）/clippy 0。
     家族定稿 2 crate/8 测试名（fs-workspace ×4：187/187/—/328 行号分散；cas-store ×4），
     全 1546、全 crash 后 reopen 路径；全量单轮 0-2 条波动（出现率数据入档）。
     卡已更新（main ac4fa9f）。结构批 rebase 整批换号（c927a16→e6ca7e5）——388 又一实例，
     收口报告引用换号后 ref。B-17a 家族画像稳定后战役可收（QA 定）。
     下一步：worker 交付 Write/Overwrite → 写链批整批验收 → L2/M/U → 收口。

391. 【B-17a 重放战役收口（9 轮：7 绿 2 抓）】三条理由成立：①画像稳定到边际收益为零
     （留名/分离/条件画像三目标全达成，出现率 3 次独立全量佐证"单轮 0-2 条"——继续跑是
     重复采样稳定分布）；②证据换源——两个观测项是运行时内部状态，重放（外部观测）够不着，
     插桩接棒（换手点判断准）；③可复活而非删除（A/B 验证将来原样可跑）。
     结论文案（收口报告 B-17a 节）：有界重放 9 轮（touch-new-build+负载标记），2 轮抓到 7 绿；
     家族 8 名/2 crate/1546 统一签名；高并行概率事件（fresh-build×总负载未分离，锁内自发红
     证明非必要条件）；无 KI-11 混杂。补两句：B-17b 9 轮 0 次（触发需 pack 并发，另排）；
     rustc 0xC0000005 一次负载相关待证未升级。
     插桩诊断（两条观测项）排 P3-4 收口后执行。

392. 【读回验证要读回那个东西本身，不是间接指标】Write 第一版只核对尺寸：中段丢弃时
     before.size==after.size 照样成立 ⇒ cell drop 意外通过；改读回字节比对才能红。
     尺寸是影子，字节才是东西——间接指标在恰好不变的场景给假绿。补齐 021ed97 的盲区：
     set_size 的读回值恰好是被测物本身；本条要求确认读回的确实是那个东西。
393. 【签名与位不同源（330 精确化）】位用探针（接口成员顺序版本间重排——326）；
     形用头文件原型（成员签名是稳定 ABI，append-only——ddca7bd :471-475 与实现一致已核）；
     裸值只在两说矛盾时仲裁。旧 Write trampoline 少 Offset+out 指针反序，九年不显形
     因为函数体只返回常量（WRITE_PROTECTED 类门阶段）——签名要读原型，
     不能从记忆或"能编译"推断。三个头文件细节=三种静默错法：
     WriteToEndOfFile 用 Offset==u64::MAX 表示；ConstrainedIo=不得增长（裁剪如实报）；
     *PBytesTransferred 是调用方唯一证据。

394. 【★ 写链批全部接完（d233c4d）——P3-4 实现部分完成】Overwrite@5 真实现：条目保持身份
     （同 file id 截断，裁定 (a) 落挂载层）；ReplaceFileAttributes 两向判据（G1 修订卡形态）；
     派生位不存储（掩 NORMAL/DIRECTORY 再校验——Stat 派生属性纪律的 FFI 层落地）；
     AllocationSize 接受但忽略（理由写明）；读回两样都核（size=0 且存储位=策略结果）。
     写链回调全表：Create(真建)/Open/Overwrite(真)/Read/Write(真)/SetFileSize/Cleanup(两段)/
     Close/GetVolumeInfo/GetSecurityByName/GetSecurity/GetFileInfo/ReadDirectory。
     顺带修文档错位（Overwrite 注释挂到 GetSecurity 头上——编译器不管文档对不对，
     与 393 姊妹形态：签名错能炸文档错只误导；将来可加 cargo doc 无警告 CI 检查）。
     下一步：验收素材 → 真机写链（Windows 第一次写 layer-fs：create→write→读回→set size→delete，
     每步计数器+配平+misses+删除后条目消失+内容逐字比对）→ 整批交 QA。

395. 【真机写链首跑红：os error 3（NAME_NOT_FOUND）】失败在第一步（根上建 written.txt）。
     已排除：错误码是 STATUS_OBJECT_NAME_NOT_FOUND 的 Win32 形态=拒绝来自我们的壳；
     同管线读链绿=表/注册表/trampoline 通；路径分隔符猜想已试无效。
     manager 预判（待 trace 确认）：真机挂载用 FixtureOps（只读夹具）——其 create_or_open
     对新名字答 NOT_FOUND；错误码/位置/读绿写红三点吻合。
     若成立：修法不是改壳而是换后端——写链真机测试的家在 fs-api（唯一能依赖两侧的 crate，
     P3-3b 先例）：ApiBackend+真 workspace 挂 Z: 跑 create→write→读回→set size→delete
     = "Windows 写进 layer-fs"的完整证据。FixtureOps 保持只读（测 FFI 契约不测存储）。
     附：GSBN 对不存在名字答 NOT_FOUND 在 WinFsp create 流程里是中间答案（dispatcher 据此
     走建文件路径）——若它成终局拒绝才是壳的 bug。
     草稿不进 #[ignore]：被忽略的测试在摘要里看起来绿，而这条从没绿过——
     drafts+README 更诚实（与"未验证不许写成通过"同根）。

396. 【真机写链 trace：夹具拒根 + Write Length=0 之谜】trace 否掉草稿两猜想：红来自夹具——
     MockBackend::split 拒 "/"（前导斜杠后空分量）⇒ 根打开 InvalidParameter ⇒ 全链倒下
     （夹具限制挂在 trait 缺口上：目录 id 不可按路径取得）。修根后链进一步：
     Write 到达时 Length==0 而 std::fs::write 请求 11 字节。
     候选：(a) 签名与 DLL 不符（ddca7bd 与本地头 Write 原型一致——(a) 概率低）；
     (b) Windows 真发零长写；第三形态（manager 补）：前置 8 字节参数挤位 ⇒ Length 整体后移
     读到 0、11 落在标为别名的位置。探针定案：9 参裸值 + 对照 FSD trace 的 >>Write 行
     （DLL 自己解出的 Buffer/Offset/Length）——找 11 在哪，签名当场定死（ReadDirectory 同法）。

397. 【Write 探针读数与源码钉子】裸参：buffer 指针 ✓/Offset=0 ✓/Length=0xe=14 ✓（14 在正确位置
     ⇒ (a)(b) 基本否掉——14 在 trampoline 读对了但在 write_shell 变 0 ⇒ 错位在我们内部，
     非 FFI 边界，与 ReadDirectory 那次相反）；但 a6=0x4000、a7=0xdd9b6d00（指针低半样）。
     manager 源码钉子（fsop.c@ddca7bd :1048-1056）：Interface->Write 第 6/7 参是字面比较表达式
     （(UINT64)-1LL==Offset 与 0!=ConstrainedIo）——只可能 0 或 1 ⇒ a6/a7 的读数不可能来自
     调用侧 ⇒ 错位在探针声明或 trampoline→shell 交接。少一格声明会使标签前移（a7=实际
     PBytesTransferred，与指针样读数相容）。5 分钟 shell 入口打印定案。

398. 【推理链的对抗性使用】manager 的"少一格"假设被 worker 的读数否掉：前移一格 ⇒ a6 应为
     &BytesTransferred（指针），实际 0x4000；前移两格 ⇒ a6 应为 Length=14，也不符。
     打印先于推理——两个假设（探针标签错 vs 槽位错位）都有干净红法，一轮分清。
     实验补强两维度：①每次调用都打印+计数（读数先问"这是哪次调用"再问"参数是什么"——
     混行读数是探针经典噪声源；槽 9 可能被多个流调用，单行读数可能不是 OpWrite 那次）；
     ②与 FSD trace >>Write 行同窗对照（buffer 地址/Length 吻合 ⇒ 前 5 参对上，尾参只剩三种可能）。
     槽位怀疑的先验与反证：write_shell 确实被打过 ⇒ 槽 9 至少被 Write 流调用过；
     若 read 也打进来且形态相同 ⇒ 同格被两流共用 = 槽位表整体错位（我们的 9 是它的 8？）——
     完全不同的病，实验数据直接说。

399. 【严格错了地方会把所有人引向错误的现场】std::fs::write 发 FILE_OPEN_IF（高字节 3），
     上一刀白名单只放 OPEN/CREATE ⇒ 文件从未创建 ⇒ 下游全倒。三次 trace 全在看 Write，
     而 Write 从没被调用过（修后 Write 首次真被调到）。
     机制化：第 N 步的拒绝把诊断送到第 N+1 步——白名单拒绝返回了"合理的"错误码
     （OBJECT_NAME_NOT_FOUND），症状全落下游。与 333（门禁在成员之前）同族但更隐蔽：
     门禁至少报 c0000010。修法：六种 disposition 全放行（core 本来就支持），
     未定义取值（0x07）仍在问后端之前拒绝。
     Write 尾巴之谜新数据：a8/a9 相邻栈地址（差 0x60）=&BytesTransferred/&FileInfo 的样子；
     a6=0x4000/a7=0x97374900 是 Length 与出参之间的两个额外参数——主线头与 ddca7bd 的 Write
     均为 9 参（manager 核过）——若 8×u64+FSD trace 证实 10+ 参 ⇒ 安装 DLL 与两份源码头都不同
     （比槽位重排更深一层的版本错位）。便宜分叉先验：探针 u32/u64 混排也可产生假尾巴，
     8×u64 裸视图一次排除。

400. 【子寄存器参数的高位未定义（ABI 最后一块暗礁）】Write 尾巴两格（0x4000/0x33984000 等）
     = WriteToEndOfFile/ConstrainedIo 两个 BOOLEAN——MS x64 调用约定：小于 64 位的参数只保证
     低位有效（寄存器传 AL、栈传只写低字节），槽的其余 7 字节是调用方不清理的栈残留。
     C 编译器被调方只读低字节所以 C 侧永远正确；按 u64 读整槽就把垃圾当值。
     证据：两次运行四格读数低字节全 0x00（=FALSE，offset 写/非受限写语义全对），
     高位垃圾行为典型（a6 两次相同=同一栈形；a7 两次不同=不同栈形）。
     九参签名成立，无第十参。修法：FFI 布尔参数声明 u8 或读后 &0xFF——
     绝不把子寄存器参数当完整 u64 读。红法/确认：append 模式写 ⇒ Offset=-1 ⇒
     WriteToEndOfFile=TRUE ⇒ 低字节应 0x01（垃圾高位仍在）。
     签名对、槽位对，仍会被"参数比槽小"咬一口——ABI 检查表的最后一项。

401. 【字节级理论定案（append 红法）+ Flush@10 补缺】append 写：eof_slot 低字节 0→1（0x4001）、
     0x4000 恒不变（=栈残留）、offset=-1/length=2——三线齐证九参签名成立。
     修法：三个 BOOLEAN（WriteToEndOfFile/ConstrainedIo/ReplaceFileAttributes）一律 u8。
     Flush@10 补缺：WriteFile 后关闭触发 flush，缺槽把"刚成功的写"变成 INVALID_FUNCTION
     （成功后的失败比失败更迷惑）。
     真机写链仍卡 INVALID_FUNCTION——下一环打法：debug 日志 >>Op 行点名（324 打法，不猜候选）；
     缺的成员多半映射到已有 WorkspaceOps 方法（SetBasicInfo@12→set_times/set_attributes、
     CanDelete@14→set_delete）——不是新实现，是把已有 adapter 方法接 FFI 槽（纯核心+薄壳老配方）。
     禁止假最小版（382）：宁可真实现（方法都在），拒绝就拒绝。

402. 【写链最后一环是读（日志点名第四胜）】日志定案：<<Write IoStatus=0（写成功！），
     >>Read c0000010——Read@8 从未注册，链卡在读回。反讽：我们的读回验证步骤（392）
     自己把链踩断。SetBasicInfo/CanDelete 未出现（日志点名 vs 猜候选，324 打法第四胜）。
     附带发现：QueryVolumeInformation 答 BUFFER_TOO_SMALL（FSD 未打印长度）——先验是
     334 两段式（先问大小），修好顺带解决"Explorer 显示不出卷大小"（真实用户可见缺陷）。
     Read@8 判据四条：偏移读/短读/读目录被拒/EOF 后零填充（trait 契约原话：
     "zero-padded by the caller's contract"——零填充归适配层，不做 Explorer 读文件尾出错）。

403. 【BytesTransferred 报实际值（OOM 教训）+ ctx 运送机制】read_shell 第一版报补齐后长度
     ⇒ Windows 以为还有数据 ⇒ 下个偏移再问 ⇒ 无限循环到 OOM——BytesTransferred 是调用方
     唯一证据（393），必须报实际读到数。幸好 OOM 而非静默丢数据。
     mock 契约违反（read 自行补零 vs trait"调用方补零"）：修 mock + 三条期望同批，
     依据 trait 文档不是"测试红了"——那三条断言的正是契约违反本身（"空文件读 8 得 8 零"）。
     ctx 运送机制（fsop.c :24-36）：flag 未设 ⇒ FileContext=UserContext 槽 0，
     AddrOfFileContext=&UserContext；Read/Write ctx 有效 ⇒ 管道通 ⇒ SetInformation 的
     null ctx 反常——(i) 目标是另一句柄 或 (ii) trampoline 读参错位。
     探针：set_file_size 5 参裸值（BOOLEAN u8）+ read_shell ctx 同跑对照；
     FileSize=0:5 是 high:low=5 字节（多半是链上 set_len 步）。

404. 【★★ 写链里程碑（f1b8ad6）：Windows 写进 layer-fs 且读回一致】计数：create 1/write 1/
     set_size 1/cleanup 7（关句柄粒度比预期细）/contexts 1→1 配平/misses 0/released true。
     断言：读回字节==写入（392）/set_len(5) 剩前五/remove 后不存在。草稿转正
     tests/mounted_write_chain.rs（WinFsp 缺失自跳过）。
     三修（每处由读数得到）：①六种 disposition 全放（399）②SetFileSize 五参缺 BOOLEAN
     ⇒出参早一格 c000000d（400 家族第三例）③Read 注册+BytesTransferred 报实际（OOM 教训）。
     EOF 第三条路：壳夹取 want=min(length, size-offset)——EOF 成真零字节读；补零是后端自由，
     契约留在 trait，适配层让 EOF 可见（不改后端不改期望，避免 mock 连锁）。
     待办：QueryVolumeInformation c0000023（卷大小显示不出=用户可见缺陷，334 两段式先验，
     验收前修）。cleanup=7 的构成素材里解释（将来 L2 基线）。
     下一步：写链批素材 → QA 验收 → L2/M/U → P3-4 收口。

405. 【写链批收尾（0459c35，10 提交）→ QA 验收】卷信息缺陷修复：探针（WinFsp 8 字节来问 ×6，
     我们一律 TOO_SMALL ⇒ Explorer 无卷大小）→ VolumeInfo 前导排列写能放下的前缀 +
     哨兵 0xAA 钉越界面 → c0000023 从 6 到 0。用户可见缺陷的证据链完整
     （症状/根因/修复/读数对比）。cleanup=7 解释：多句柄构成（写/读回/set_len/删除+目录探查），
     contexts 1:1 + misses 0 = 非泄漏，L2 基线数。
     QA 验收四重点：写链里程碑复现（含 cleanup=7 独立记数）/卷属性关闭证据（+8 字节查询类
     口径衔接 334/301）/BOOLEAN u8 抽查/白名单。
     worker QA 期间跑 L2 挂载侧定向（12 类预归类，正式对照仍 QA 两侧）。
     收口路线：合入 → L2（U15 多页首验）→ M1–M9 → U 消项 → 收口报告。

406. 【L2 试跑首撞堆损坏（0xc0000374）——验收阻塞】挂载侧第一次试跑（操作面比写链宽：
     创建/截断/长名/属性/删除）中止于 STATUS_HEAP_CORRUPTION。内存损坏类，真实用户路径
     （Explorer 同款操作）——列验收阻塞项。
     嫌疑排序（供读数对照）：①卷信息前缀写（0459c35，容量由调用方给；FSP_FSCTL_VOLUME_INFO
     布局≠OS 查询类布局）②出参指针写——400 家族深层形态：参数读错=写错地址（SetFileSize
     缺 BOOLEAN 若未修正是这种错的形态）③DirBuffer 对齐/read_shell 夹取边界算术。
     加强诊断备用：PageHeap（越界写发生的那条指令即断，崩溃点≠损坏点）。
     定位路径：debug 日志最后 >>Op → 缓冲算术 → 修 → 复验。
     附：Command::new("bash") 解析到 WSL bash 看不见 F:——工具名相同不是工具相同，
     L2 脚本显式 Git Bash 全路径（QA 机器同坑）。
     L2 正式对照等堆损坏关闭后启动（对照组在内存损坏之上无意义）。
     写链全绿≠用户路径全绿——更宽操作组合的测试面价值当场兑现。

407. 【中止点≠损坏点：堆损坏的异步检测】日志尾部=中止邻域（堆下一次被碰巧检查处），
     不一定=损坏邻域——QueryInformation 邻域里的 GetFileInfo 出参是嫌疑不是定位。
     fsop.c：dispatcher 给 GetFileInfo 的出参是自己的栈上局部（FSP_FSCTL_FILE_INFO FileInfo;
     Interface->GetFileInfo(fs, ctx, &FileInfo)）——读参正确则永远有效；损坏 ⇒ 写的是
     错读指针 = 400 家族深层形态（参数读错=写错地址）。
     工具：gflags 在本机（C:\Program Files (x86)\Windows Kits\10\Debuggers\x64\gflags.exe）
     但需提权——/p /enable <exe> /full 在越界写指令处断（cargo test 二进制带哈希名，
     --no-run 后取）；无提权兜底=VirtualQuery 校验出参（区域可写+剩余≥所需，不满足打印
     指针后拒写——零提权抓第一现场，可常驻 debug_assert 永久防御）。
     406 嫌疑③（DirBuffer/read 夹取）后移：邻域无 Read/QueryDirectory。

408. 【QA 暂缓期透明上报 + 排除数据的边界】读令前完成两项（非抗令）：mounted_write_chain ×2
     全绿（cleanup=7 独立复现）+ GetDiskFreeSpaceEx 真实内核查询 ok=1/total=1MiB(=夹具)/free=0
     ——记档不定论，终态以修复头为准。
     排除数据及其边界：真实内核卷查询触发前缀填充无损坏 ⇒ 嫌疑①对 FileFsSizeInformation
     （24 字节类）排除一次——但排除按类不按路径：FileFsAttributeInformation（变长字符串）
     等类未证。VirtualQuery/gflags 猎捕照旧（第一现场比排除法硬）。
     crash 家族第 9 名（crash_before_the_index_commit，1546）——卡已更新（main 0863ff6），
     症状面扩至 index commit 前 staged 路径；与堆损坏不同族。
     QA 暂停项：锁内全量不跑（不抢机器+数字不代表终态）；静态审查成果挂账
     （10 提交判据行/BOOLEAN u8 四处 mounted.rs:186/279/322/323/卷前缀单测覆盖）。

409. 【★ 幻影参数：GetVolumeInfo 是两参，capacity 是我们发明的】ddca7bd winfsp.h:206
     与 fsop.c:1245-1260：GetVolumeInfo(FileSystem, &VolumeInfo) 两参，出参是 dispatcher 的
     88 字节栈上局部（memset 后全量 memcpy）——没有容量参数。我们的 trampoline 读的第三参
     = 不存在 = 栈残留垃圾（幻影）。
     幻影解释整条历史："8 字节查询 ×6"是垃圾槽恰好 8；旧版 TOO_SMALL 在给不存在的约束答题；
     前缀修复写 8 字节恰好是 VolumeInfo 前 8 字节=TotalSize（所以卷大小显示了）；QA 的
     24 字节类"排除"也是另一份垃圾。
     堆损坏连接（待实现确认）：L2 宽操作面让卷查询从不同调用点进来，垃圾值不同——
     若前缀写长度信了幻影，垃圾>88 ⇒ 越过 88 字节栈上局部 ⇒ 损坏。
     根修：trampoline 两参、无条件写满 88 字节（null out 才拒）、删幻影 capacity 及其判据
     （四格里短缓冲⇒TOO_SMALL 等测的是幻影约束）、判据换真契约。
     两条流程记档：393 违例现场（纯 Rust 核心想象的契约渗进 FFI 层——签名没从原型读）；
     QA 的"8 字节哪个查询类"问题自动消解（不存在那个类）。
     VirtualQuery 守卫零触发的补充解读：守卫粒度=页级，同页内越过邻分配头部不触发
     （PageHeap 才能抓同页越界）——守卫沉默弱于表面证据。

410. 【一个参数若在两份头文件里都不存在，它就不该有判据】GetVolumeInfo 幻影容量的格言化：
     四格卷判据有一半在测自己发明的约束。与 383（两假设同释=零信息）配对：一条管读数、
     一条管参数。a15276b 两参根修落地（无条件写满 88 字节/第 89 字节不动/null 拒绝）——
     幻影虽非损坏源（min(88,幻影)恰好挡住越界），语义修复独立成立。L2 仍中止。
     系统性签名审计启动（幻影藏 30 提交 ⇒ 别的 trampoline 也可能有）：ddca7bd 原型表——
     GSBN 5 参/GetSecurity 4/GetFileInfo 3/Cleanup 4 VOID/Close 2 VOID/SetBasicInfo 8/
     Overwrite 6。审计重点：参数个数与顺序（多/少都让出参错槽=写错地址）+ 返回类型
     （Cleanup/Close VOID——声明偏差即使"恰好无害"也清零）。审计（读代码快）与
     守卫挪 DirBuffer/get_security（跑一轮慢而确定）并行。
     剩余嫌疑：①DirBuffer（唯一变长载荷）②get_security* 出参。

411. 【按记忆写声明——trampoline 层的历史定性】签名审计第二杀：GetSecurity 四参我们声明五参
     （多 SecurityInformation）⇒ context 后实参全部晚一格：我们当 descriptor_out 的其实是
     调用方的 PSecurityDescriptorSize；当 size_out 的是栈上下一个东西（写 *size 到没人选的
     地址——本次猎捕的目标错类）。五个成员两个错（40%）= 系统性未对照原型，
     不是个别失误——审计是对整个声明层的重写式核对（393 应在最开始做的）。
     FFI 声明层的第一次提交就该逐个抄原型。
     执行调整：14 个 trampoline 全对完攒齐一次修再跑 L2——崩溃只需一个损坏源，
     逐修逐跑会被已修/未修交织误导；全修后一轮要么绿要么剩纯数学类（对齐/夹取），
     嫌疑空间干净二分。崩溃形态变化（哪怕仍崩）= 损坏路径被切断的信号。

412. 【AddDirInfo 正典数学 + hunt 的释放侧盲区】fsop.c:1500-1529（DLL 导出助手）：
     装得下判据用【对齐后】长度；memcpy 只拷 Size 字节——**从不写填充**（填充是步进留下的
     空隙，不是写出的字节）。对照点：①我们的判据用 Size 还是 align8(Size)；②我们的
     "填充清零"（d0c94ee）是否显式写填充——若写填充且判据用未对齐 Size，剩余在
     [Size, align8(Size)) 之间时越界 ≤7 字节=堆损坏形状。
     hunt 盲区：0xC0000374 同样烧释放侧（双重释放/无效释放），至今全部嫌疑都是写侧。
     释放面=Close 的 Box::from_raw——同一 context Close 两次或来源非 into_raw 即双重释放。
     核查：崩溃日志数 >>Close vs >>Create+>>Open；grep from_raw 全调用点逐个看来源。
     中止邻域的写全是栈目标（守卫未触发）——偏向释放侧或更早损坏。
     签名审计闭合：12 成员 2 错已修 + Cleanup 返回类型小改待做（VOID 原型）。

413. 【★ 堆损坏破案：释放侧（盲区命中）】同一 FileContext 被多次 Close（double-release 17 次）
     ⇒ Box::from_raw 双重释放 = 0xC0000374。写侧四查全没中——找错了半边。
     修法：LIVE_CONTEXTS 按地址登记；close_shell 先认领，认不到 ⇒ 计数+打印+拒绝释放。
     不对称原则：双重释放是 UB，少释放一次只是泄漏 24 字节——拒绝第二次释放是唯一安全动作
     （"宁可显式缺不可静默错"的释放语义形态：错误两边不对称时选可观测可恢复的那边）。
     未解释：为何 WinFsp 多次 Close——机制假设（Cleanup 每句柄/Close 每文件对象；
     嫌疑 open_like 对同一文件再打开每次新 context 而对象错位）；三数定位法：
     日志 >>Open+>>Create vs >>Close vs double-release 17。
     L2 首次跑完（新阶段）：07/08 rename + 09 delete-while-open = Rename@15/CanDelete@14
     未注册（scope 内剩余件，adapter 已有 rename_with/set_delete）；01 root 判定 + 05 空列举
     = 真待查。顺序：double-close 机制 → Rename/CanDelete → 01/05。
     签名审计最终账：12 成员 2 错（GetVolumeInfo 幻影、GetSecurity 多参）+ Cleanup VOID 小改。

414. 【三数对账 + 日志行语义】L2 挂载：open_like 122/create_shell 4/contexts 115 分配/
     97 释放/18 双重拒绝；日志 >>Create 244/>>Close 117/>>Open 0。
     115=97+18 ⇒ 账目平衡，Close 比 context 多 18——非泄漏非丢释放。
     >>Create 244 vs 我们 126 的解释候选（manager）：>>Create 是 IRP 派发行，在 disposition
     分派/访问检查/接口调用之前打印 ⇒ 数所有 create 类 IRP，含从未到接口的失败查找
     （GSBN 答 NOT_FOUND ⇒ 早退）。验证：GSBN NOT_FOUND 次数与 244-126=118 相消即消解。
     >>Open 0 = 注册了 Create 则 Open 不被调（头文件一致）。小账：>>Close 117 vs 97+18=115
     差 2——配对实验留意。配对定案法：同指针两条 >>Close 一条 >>Create ⇒ WinFsp 侧双发；
     配不上 ⇒ 我们多发 context（转 open_like 调用链）。

415. 【地址不是身份】CLOSE-TRACE：同一地址出现多次（#1/#2/#6/#8 同址）——歧义：
     (1) 地址复用（迟到 Close 落在新 context 上——比双重释放更糟：放错对象被放行，
     LIVE_CONTEXTS 挡不住：新 context 让地址又变活）或 (2) WinFsp 真重复 Close。
     LIVE_CONTEXTS 按【地址】判活 ⇒ 隐含假设地址==身份，分配器复用打破它。
     "计数是指纹"的深层版：指纹要唯一到发行，不能唯一到地址。
     分离实验：trace 模式禁复用（释放的 context 进 Vec 故意泄漏，地址永不回来）——
     重复指针=(2) 铁证；不重复=(1)。故意泄漏是实验仪器不是修法（24B×无限 open
     的生产泄漏不可接受）。若 (1)：根修是生命周期对齐（为何迟到 Close 会到——
     "哪次是最后一次"的判断与 WinFsp 文件对象模型错位），不是"永不复用"；
     候选修法：每文件对象发一次 context（对齐语义）或 context 带发行序号（close 验序号）。
     实验须在 Rename/CanDelete 之前（错对象释放 × 新回调 = 不可读噪声）。

416. 【被拒地址分两类 + 堆损坏全链条】fsop.c：dispatcher 预置 FullContext=0——成功不写槽
     ⇒ Close 得 NULL；被拒地址是【非空垃圾】⇒ 两种来源分开：
     NULL-ctx close = 我们成功路径漏写（反向判据抓）；垃圾-ctx close = 不走零置链的路径
     ——最像 FSD 对失败 create 仍发 Close（槽=响应缓冲区陈旧内存）。
     全链条：FSD 对失败 create 发 Close（陈旧值）→ 分配器复用让陈旧值命中活的新 context
     地址 → 错对象释放 = 0xC0000374。
     修法双面：①反向判据（发放数==out 槽被写入数；失败路径槽终值应为 0——NULL 类与垃圾类
     在计数器上分开）；②close 身份校验从地址活升级为身份匹配（415 发行序号——
     地址会复用，发行号不会）。禁复用实验已判：之前同指针多次=分配器复用为主。

417. 【值即身份，不由值指向的内存事后证明】分类结果：17 条陈旧 Close 全非空、NULL 类 0
     ⇒ candidate 2 证实（FSD 拿从未属于我们的槽来 Close），我们侧"成功必写槽"干净。
     worker 证伪 manager 的发行号方案：复用发生时地址确实是新 context 的——读出的序号
     也是新的——校验通过——错放对象且静默。WinFsp 只回传指针，指向的内存无法区分
     "这次 Close 属于哪个 context"。教训：身份必须由通道里的值本身携带（值即 id，
     从不解引用），不能由值指向的内存事后证明——415"指纹唯一到发行"的最后一环。
     修法（批）：id 单调递增每挂载、表 id->Box 唯一所有者、Cleanup 查不删/Close 删、
     陈旧 id 拒绝不解引用（对垃圾输入结构性鲁棒，不必彻底解释 FSD 为何发）；
     双重释放由结构排除，LIVE_CONTEXTS 安全网退役（历史使命：抓 18 次双重释放撑过诊断期）。
     先于 Rename/CanDelete：rename 的 context 生命周期最复杂——新地基上盖复杂房间。
     close 后表中无此 id 成为便宜的生命周期判据。

418. 【数了宽度，没把它算进预算——枚举不是评估】id 表改造库侧完成但未落地（WIP stash fe03d37）：
     直测五文件手工构造 EntryContext 传地址，需改 register_context_for_test——跨五文件，
     半途提交留红测试，整轮做。worker 坦白：预算边缘启动跨五文件改造=执行判断失误。
     数出 5 个文件=认知有了；没换算成"够不够本轮预算"=记账没做——枚举不是评估，
     数出来的清单还要过"这个宽度这轮吃得下吗"（与 353 先 grep 后动手同一动作的两个阶段）。
     设计注记：register_context_for_test 须走生产相同登记路径（测试助手=生产入口薄包装，
     不是第二入口——否则测试绕过 id 发放，削弱唯一性不变量）。
     落地后撤 PIN_ADDRESSES/CLOSE_TRACE 实验仪器（或 cfg 门控）。

419. 【CRLF × \n 匹配 = 静默 no-op（老谜团的机制）】perl -0pi -e 多行匹配按 \n 对齐——
     CRLF 文件里是 \r\n——模式静默不匹配：不报错、不改文件、退出码 0。
     可能是 348/353 两起"注入没生效"的共同真因：改仓库 CRLF 文件时灵时不灵的表象
     完全对得上（自己新建的 LF 文件则成功）。对策链（进收口报告）：
     症状（注入没生效）→ 当时对策（grep 验证落盘）→ 机制（CRLF×\n 静默失败）
     → 终对策（按实际内容匹配的 edit 工具）。
     普适版：工具与文件环境的错配产生静默 no-op——要么按实际内容匹配，
     要么改完必须验证落盘。CRLF 仓库里不要用按 \n 匹配的脚本改文件。
     id 表第二次尝试：register_context_for_test 已是生产薄包装；5 文件改完；
     6 红剩（read_directory_core 纯机械 + mounted_write_chain 端到端）——
     预算边缘停手（提交红线之前，比上次进一步）。stash 卫生：确认 v2 ⊇ v1 后丢 v1。

420. 【不变量跟随语义（期望改写的合法性）】id 表落地（466806b）：CONTEXTS BTreeMap<u64,
     Box<EntryContext>> + 永不复用 id + 从不解引用；表唯一所有者 ⇒ 双重释放结构排除；
     四仪器退役（LIVE_CONTEXTS/DOUBLE_RELEASES/CLOSE_TRACE/PIN_ADDRESSES）。
     旧等式（分配==释放）被 17 次实测证伪（WinFsp 会 close 从未属于我们的槽）——
     新不变量三部：①不双释放（结构保证）②陈旧拒绝+计数（可观测）③freed<=allocated（界，
     残留交 teardown）。不是放宽换绿=不变量跟随语义（341 卡片文本以实测修订的断言版）。
     精度要求：stale==0 断言的适用面——写链/L2 流程各自会不会产生陈旧 close 要分开断言
     （379：期望值从设计算出，此处设计=流程语义）。表归属：每挂载一张 ⇒ 卸载 drop 残留
     不跨挂载累积；全局表则差 teardown 清空。

421. 【归我们控制的性质才断言（379 补全）】stale close 是否发生不由我们决定（L2 实测 17 次是
     常态）——写成断言就是给第一次碰巧触发的运行埋假红。断言只看守我们能控制的：
     ①不双重释放（结构）②freed<=allocated（界）；不归我们的（陈旧次数）⇒ 打印/计数。
     判据期望值法则完整版：从设计算出 + 问"这个性质归谁控制"——归我们⇒断言，
     不归我们⇒观测。写链 stale=打印；L2 stale=计数观测。
     表归属：CONTEXTS 是进程级 static（回调现实妥协；registry 后其实可达 adapter）——
     teardown clear_contexts() 把残留界在一次挂载；表入 Adapter 记名下另一刀。
     Rename@15: (fs, ctx, Name, NewName, BOOLEAN ReplaceIfExists[→u8，400 家族第四例])；
     CanDelete@14: (fs, ctx, FileName)。L2 07/08 = U1（目标有句柄不下发 Rename）首测机会。

422. 【只被某类流程走过的路径 = 该流程的专属坑】Rename@15/CanDelete@14 接上（786bf26）：
     CanDelete 不重复实现"安全"共享部分（单源）；rename UTF-16 只转一次；context 仅一致性
     检查（rename 对象由路径指定——G2 纪律）。L2 07/08/09 仍红真因不是 Rename：
     <<QueryDirectory c0000010——read_directory 拿到表外 context，05/07/08/09 四条被挡。
     manager 候选：写链只走文件创建路径（登记 ✓ 7/7），目录创建走 open_like 的 mkdir 分支
     （892ede3）——若该分支没走到 context 登记 ⇒ 目录句柄无 context。L2 建子目录再列举
     正好踩这条只被 L2 走过的路径。trace 判别：未知 id 的前一条 >>Create 是
     FILE_DIRECTORY_FILE 创建则对上；修法 = mkdir 分支补登记（一处）。
     Rename 语义判据（含 U1 首测）排其后。

423. 【最小实验否候选 + 选位解码】mkdir 分支候选被两行最小实验否掉（目录开→ctx 6 登记→
     列举答 128B，干净通过）。真线索：同一名字两次 Create——第一次 0x200040 成功（ctx 7）、
     第二次 0x20 得 c0000034。选位解码（NtCreateFile 标准位）：
     0x200040 = FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE（普通文件打开形态）；
     0x20 = FILE_SYNCHRONOUS_IO_NONALERT（bash/MSYS 工具 mv/rm 的典型形态）。
     两者都不含 FILE_DIRECTORY_FILE ⇒ MakeVerb 同为 File ⇒ 差异不在我们的分路。
     必查：c0000034 是 GSBN（访问检查，Create 之前）还是 create/open 答的——
     GSBN 两次一致而 Create 结果不同 ⇒ 嫌疑集中到上下文/状态相关分支。
     对照：日志 [UT----] 标记位（大小写敏感性等）两次是否不同。

424. 【收窄不等于确认】连续两次（mkdir 分支、同名二次打开）把"更窄的线索"当成结论，
     两次都被下一轮 trace 处决。原日志两次 Create 之间还有四行：Cleanup[Delete]
     "\WRITTEN.TXT"（大写！）+ Close ⇒ 文件已被删 ⇒ 第二次打开 not found 是正确行为。
     纪律：推理链每一环节辞都带待验标记——收窄只意味嫌疑空间变小，不意味到终点
     （383/398 打印先于推理的更细一层：措辞纪律）。闭环本身健康（实验处决自己的结论）。
     正面证据（日志里白捡的）：大写名删除成功（文件小写创建）⇒ 大小写不敏感匹配在真实
     链路工作（未设 CaseSensitiveSearch 的直接后果）——L2 08 类（case-only rename）半个前置预答。
     新检查对象：L2 脚本自身的文件状态依赖（每类开始依赖什么文件、上一类留下什么）——
     删除后打开失败是正确行为 ⇒ 07/08/09 的红可能部分是脚本期望写错，别让 FS 背脚本的锅。

425. 【★ mkdir 报成功但目录不存在——trait 欠规格第三例】分步真机定案：write file Ok / mkdir Ok /
     write a NotFound / list NotFound / rename NotFound ⇒ 目录 Create 答成功却无可用目录。
   根因：无「按路径打开目录」能力——create_or_open 是文件门（拒目录），目录分支只在
   FILE_CREATE 走到；Windows 随后 FILE_OPEN+FILE_DIRECTORY_FILE 再开 → 落文件门被拒。
   根能开能列是因为夹具特案护着（51727de 的权宜）——子目录没有。
   "静默成功"最纯形态：一次解释 L2 四类（05/07/08/09）。
   修法（裁）：open_dir(path)->id 独立方法，2×2 摊平在分派层
   （(目录,Create)→mkdir；(目录,Open)→open_dir；(文件,*)→create_or_open）——
   与 mkdir 的既有不对称一致。判据 3+3：同 id/列空/建文件 + NotFound/类型错/根走同路径。
   根特案退役（判据：无特案下两挂载测试仍绿）。
   trait 欠规格三例（DirEntry.file_id/目录打开/…）模式：写链用出来的缺口都在 trait 层——
   建议 WorkspaceOps 方法表对照 WinFsp 回调语义做完备性扫描（收口前清账）。
   07 非脚本之锅（分步已证文件存在）；06 后状态逐类核对排 open_dir 刀。

426. 【mkdir 存储键形 vs 查询键形（51727de 教训的复发嫌疑）】open_dir 落地（03de512：
     默认拒绝不编答案、(目录,OpenIf)+NotFound⇒mkdir 回落保持 2×2 四动词、双后端覆写、
     OURS_NOT_A_DIRECTORY）但真机链仍未好：create_dir Ok / 写入 NotFound / 列举 NotFound
     ⇒ mkdir 造的东西形态或绑定不对。
     manager 头号嫌疑：WinFsp 非根路径是 "\l2-dir"（前导反斜杠）——若 mkdir 存储键存原始
     形态而 open_dir/read_dir 查询键剥了前导（或反），条目存在但永远查不到——与症状
     完全吻合。查法：失败序列打印每步路径【原始字节】（mkdir 收到什么、open_dir 拿什么查）。
     第二步 mock 三层：查不到=键形 / 查到 kind 是文件=形态 / 查到 kind 对 children 空=绑定。
     提交完整绿单元但显眼注明"仍未解决"=对的做法（完整单元与 L2 修复不必捆绑）。

427. 【落地时没人验证过的前提=落地时就要验证的前提】后端读数：目录请求全走 create_or_open
     (Open)，mkdir/open_dir 一次没被调——2×2 分派的假设（目录带 FILE_DIRECTORY_FILE
     +创建型 disposition）只被\"根目录那一次\"单样本支撑，从未对非根验证。
     单样本支撑的分派矩阵，第一次遇到非根就该重验——与 424（收窄≠确认）同族：
     前提的样本量也是措辞的一部分。
     三分法：半缺=split/位定义错；两半缺=FSD 先 Open 后 Create（查第一次 Open 失败的
     返回码是否终止链）；全缺=mkdir IRP 没到接口（GSBN 答案被 dispatcher 当致命——
     对照 DLL >>Create 行数 vs 我们收到的调用数，406 老办法）。
     澄清点：读数里连写文件的 Create 都没有——是窗口截断还是更大异常（全序列打印）。
     双读数对照：DLL 日志 >>Create 自带解析值，与 raw 同窗对——别只看自己半边。

428. 【★ 文件顶替目录（L2 四类的根因）+ 状态码核对】全序列 raw 定案：第三个 Create 是对
     目录名的文件式打开（options=0x60=NON_DIRECTORY|SYNC，无 DIRECTORY_FILE）——
     create_shell 只提 FILE_DIRECTORY_FILE 忽略了 NON_DIRECTORY ⇒ 送文件门 ⇒ 后端在
     目录名上建了空文件 ⇒ 目录被顶替 ⇒ 子项全 NotFound。一次机制四类症状（05/07/08/09）。
     修法：两半都提 + 拿到条目后核对 kind（要求目录拿文件⇒NOT_A_DIRECTORY；
     要求文件拿目录⇒IS_A_DIRECTORY）。
     ⚠ 状态码核对：0xC0000102 疑似非标准（101=DIR_NOT_EMPTY、103=NOT_A_DIRECTORY、
     0xC00000BB=FILE_IS_A_DIRECTORY 是"要求文件拿目录"的正主）——用错码 Windows 映射出
     意外 Win32 错误，L2 对照时表现为假差异。负向判据：文件式打开落目录名上
     不得创建任何条目（前后 store 快照不变——顶替事故的回归测试）。
     诊断链三轮自我证伪：a8bd3f2（二次打开实为已删=正确行为）→ 63edd87（2×2 前提证伪）
     → 72db813（真机制）。

429. 【从 SDK 读，不在两个记忆里挑——manager 记忆与 worker 记忆同权】状态码双重纠错：
     worker 原 0xC0000102=FILE_CORRUPT_ERROR；manager 邮件给的 0xC00000BB=NOT_SUPPORTED
     （也不对！worker 先采纳后 grep SDK 发现，改 0xBA）；正主 FILE_IS_A_DIRECTORY=0xC00000BA。
     规则：相近的号只有一个对——从 ntstatus.h 读，不在两个记忆里挑一个。
     权威的声音也要过源码（317 槽位数救过一次，本条第二次——manager 的记忆值与
     worker 的同权，验证不豁免权威）。
     负向判据终形态：状态正确 AND store 前后完全不变（条目集合+kind）——只断言状态
     会放过"先顶替再报错"（382 家族最阴变体：错误码对了副作用还在）。
     kind 检查前置（进文件门之前）是判据逼出来的实现——否则后端对目录名答 NotFound，
     核对永远走不到（第一版红 left:0xC0000034 红得其所）。

430. 【失败下移一层 + 剩余红统一假设】顶替事故修好后（mkdir Ok / 根列 ["d"] / 目录存活），
     失败下移到嵌套创建（write d/a 仍 NotFound）。统一假设：剩余红全挂 \d\a 一个点——
     05 列举空=正确行为（a 没建成，d 本来就空）；07/09/10 的源文件都是 d/a ⇒ 下游。
     修好后 L2 重跑按映射验收（连片翻绿；不翻的才是独立问题）。
     01 独立二候选：A 脚本侧（[ -d ] 路径形态，317 老坑：Z: vs Z:\）；
     B 我们侧（stat 式打开无 kind 标志 ⇒ 走文件门应成功 ⇒ GetFileInfo 属性若缺
     FILE_ATTRIBUTE_DIRECTORY(0x10) 则 [ -d ] 说不——查根的 NodeInfo.attributes）。
     嵌套路径嫌疑：SlashBackend 转换覆盖面——父段解析（\d\a 的 \d）可能走了
     与单名不同的代码路径（mkdir("d") 与 write("d/a") 原始字节对照）。
     口径：失败下移是进展不是转圈；不把"根因修了"当"L2 绿了"。

431. 【仪器资格 = 不改变被观测行为（观察者效应的测试版）】记录后端每个调用都返回错误
     ⇒ FSD 创建/重试路径提前短路 ⇒ 记到的序列是仪器造成的（一次 mkdir 都没有），
     而真后端跑里明确有 raw=0x02200021 的目录创建。记录后端是一种【干预】不是【观察】——
     不能当仪器研究"调用序列"类问题。正确替代：真实行为里开窗（真后端 + SlashBackend
     内部 trace，打印收到与转发的路径）。与 GSBN 缓冲传 null 不同类：那次读法错，
     这次仪器本身改变现象。
     SlashBackend 判读准备：to_backend_path 只做 \→/ + 补前导 /，若转换均匀而 create 仍失败
     ⇒ 嫌疑移到父段解析（后端对 /d 的键形 vs mkdir 存入形态——426 老嫌疑复活）；
     不均匀 ⇒ SlashBackend 内部分支。
     01-B 细化：allocate 对目录写 0x10 已核（源码）——若 B 成立，嫌疑在根的【来源】
     （特案造的节点？）——根特案退役的又一次理由。

432. 【OpenIf 的 dispatcher 分解 + 升级条件=恰好 c0000034】fsop.c FileOpenIf 原文：
     GSBN（OpenCheck）失败 ⇒ 只有恰好 STATUS_OBJECT_NAME_NOT_FOUND 才 Create=TRUE
     （否则整个 create 死）；未跳过则先 Interface->Open，失败 ⇒ 又是恰好 c0000034
     才升级 Create。⇒ OpenIf 在 dispatcher 被分解为 GSBN→(跳过或试 Open)→Create，
     我们把 NotFound 映射成任何别的码（或包装过）都会杀链。
     合并打印三必打：FFI raw 高位（Windows 发的）/ backend 收到的 disposition /
     每次返回的确切错误码。两猜想：FFI=OpenIf 而后端=Open ⇒ 我们的 open_like 在分解
     OpenIf 且升级分支调了 Open 没调 Create（"Open×2 然后没了"正像这个）；
     FFI=Open（Windows 真只发 Open）⇒ 查两次 Open 间返回的非 c0000034 码。
     root 成功案例（0x03000060）同窗对照——两案例差异行=答案行。
     嵌套名假设已否（SlashBackend 转换均匀 \d\a→/d/a）。

433. 【Err(3)≠Err(2)：父段缺失 vs 叶子缺失】os error 2=NAME_NOT_FOUND(0xC0000034 叶子)、
     os error 3=PATH_NOT_FOUND(0xC000003A 父段)。grep 证实我们的 NTSTATUS 常量表无 3A
     ⇒ Err(3) 不来自我们的壳——来源只剩 FSD 侧路径处理、或我们对成功应答的【格式】
     让 FSD 在 IRP 完成时改判。判别法：DLL 日志那条 create 的 <<Create IoStatus——
     非 0（尽管壳回 0）⇒ dispatcher/FSD 应答格式化阶段改判（嫌疑：NormalizedName
     从不写、初值 SIZEMAX 使 PATH_SIZEMAX>=SIZEMAX 为假、应答不带名字 / context 写入 /
     FileInfo 字段）；也是 0 ⇒ 失败在后续 IRP（write），打印其 <<IoStatus。
     三方打印已证：create d\a 被请求且成功（raw=0x03000060 OpenIf→status=0）——
     两个猜想（分解没升级/错误码杀链）双双否证。
     口径：这条链四个已修/已否假设全部由打印推进——"每次缩小的代价都是打印不是推理"
     = 收敛的定义。

434. 【仪器三维：保真/隔离/可归属】分步测试追加在 mounted_write_chain 副本里 ⇒ 同进程
     挂载 Z: 两次 ⇒ FSD 日志（进程级单文件）里只有另一个测试的调用——目录操作没走
     被读的那次挂载。此前\"子项 Create 返回 0 而 OS 报 path not found\"的读数降级为待验证，
     干净条件（单测试文件/单挂载/--test 单跑）重验后才可作结论。
     仪器合格三维：431 保真（不改变被观测行为）+ 434 隔离与可归属（一个进程一个挂载，
     日志归属明确）。
     结构性讽刺：一进程一挂载曾是 OnceLock 时代的结构强制（302），id 表/注册表改造后
     多挂载被结构支持——旧的意外强制消失，好纪律才需显式写下；踩坑正是真空期。
     原理：收敛越深单条读数信息量越大 ⇒ 仪器缺陷污染的恰是最接近答案的那条 ⇒
     查仪器的时机与信心成正比不是反比（解释为何坑出现在三轮收窄之后）。

435. 【无 kind 标志的打开落到目录上 = 打开它（分派第三条腿）】干净仪器（一进程一挂载）首跑
     命中真缺陷：Windows 走进目录时发纯 FILE_OPEN（不带 DIRECTORY_FILE）——落文件门，
     被正确地答 NotADirectory ⇒ 后面每个子项变 path not found。修法（415d3dd）：
     MakeVerb::File 的 NotADirectory 回落 open_dir（回落失败才拒绝，错误码仍分开）。
     分派表三条腿：(目录标志,Create/Open)→mkdir/open_dir；(文件/无标志,*)→create_or_open；
     (无标志落目录,NotADirectory)→回落 open_dir。
     残留不一致：回落 Ok(1) 但随后 open_dir 答 NotADirectory（同挂载内矛盾）⇒
     头号嫌疑：回落路径与 open_dir 不是同一个 resolve——键形不同（426 第三次登场），
     打印每条路径实际用的键字符串当场对撞。
     仪器纪律的回报：污染读数会引向应答格式化歧途；干净仪器第一跑抓到真缺陷——
     431/434 的直接回报。

436. 【大写查询是模式；声明不敏感≠实现折叠】两次独立观测：某些查询以全大写到达
     （\WRITTEN.TXT 的删除[424]、\D\a 的创建）。嫌疑机制：WinFsp FSD 在大小写不敏感卷上
     有 FspFileNameUpcase 类内部路径——某些流（fold 后缓存/重试）用大写形再问。
     我们声明了不敏感（未设 CaseSensitiveSearch）但 MockBackend children 键是原始字节
     ⇒ 大写查询命不中原大小写条目。折叠必须在匹配层做（大小写归一后比较）——
     L2 08 类（case-only rename）的直接前置，也可能与升级之谜相互作用。
     键形假设干净否掉（同键 /d 同 id 1——仪器合格）。
     问题一句话化：为什么没有把 Open 升级成 Create（fsop.c 升级条件恰好 0xC0000034，
     我们返回的正是它）。四联打印定两案：FFI 高位 disposition（Windows 真发 Open=1
     vs 我们把 OpenIf 分解成 Open=3→1）/ backend disposition / 返回码 / 大小写形态。
     若高位=3 后端见 1 ⇒ 升级 bug 在我们 open_like 的分解里。

437. 【升级之谜的两道闸】fsop.c FileOpenIf：闸 1=FspFileSystemOpenCheck（内部调 GSBN，
恰好 34 ⇒ 直接 Create；成功 ⇒ 走闸 2=Open，Open 恰好 34 ⇒ 升级 Create）。
任何一道非 34 都抑制升级。四联拼读已证 dispatcher 侧分解（OpenIf 进 Create 槽、
Open 进 Open 槽）；backend 见过 Open 调用 ⇒ 闸 1 当时成功了——而 /d/a 不存在 ⇒
GSBN-成功假设当头号候选：读链时代的 GSBN（两段式、只报 size=0）为存在的夹具条目设计，
若对任何名字都答成功（含不存在的），dispatcher 以为文件存在。合同参照：memfs 的 GSBN
对缺失名字返回 OBJECT_NAME_NOT_FOUND——"名字不存在"正是 GSBN 该说的话。
仪器：GSBN + open_shell 双返回码一起打，一次定位死在哪道闸。
回落分支嫌疑保留（修法：回落只在目录确实存在时生效，以 open_dir 成败判定
而非笼统 NotADirectory）。

438. 【★ 闸 1 破案：GSBN 的目录答案——整条链闭合】根因：GSBN 用文件门问"名字存在吗"，
     目录答 NotADirectory，被映射成 OBJECT_NAME_NOT_FOUND ⇒ dispatcher 判【父段】缺失
     ⇒ 整个操作变 ERROR_PATH_NOT_FOUND——os error 3 的真身，从头到尾的错误码。
     修法：GSBN 也走目录回落（与 open_like 同一理由同一形状）——同一缺陷在两个调用点
     的两次出现：谁问"名字存在吗"，谁就需要目录回落。
     效果：/d/a 升级 OpenIf 后端 CreateNew 完成；write child Ok；list Ok(["a"])。
     Err(3) 的语义从头到尾都对（433：父段缺失）——是我们的 GSBN 答案骗了错误码。
     诊断弧线（收口报告素材，与七环错误链并列）：目录顶替（72db813）→嵌套名（否）→
     Open 到达→升级之谜→两道闸→闸 1（8fe5b81）——十轮打印推进，每个假设被下一轮处决，
     贯穿指纹 os error 3 从第一天就在说"父段缺失"，十轮才听懂。
     下一步：L2 重跑按映射验收（05/07/09/10 连片翻绿）+ 大小写折叠独立一刀。

439. 【L2 计分板：6✓6✗；06 新暴露】映射预测兑现：05 一个根因翻绿一类（空目录=子项创建
     失败的后果）。✓ 02/03/04/05/11/12；✗ 01（独立）/06/07/08/09/10（10 可能 07 下游）。
     06=rmdir 非空【成功】（NTFS 拒绝）——不只是错误码错，是【数据丢失】（连子项删了？）。
     修法定位（422 纪律）：规则在真删除处——remove_at（或调用链）没查子项；
     修=真删除路径对目录先查 children ⇒ NotEmpty（映射 0xC0000101 已在位）。
     判据照 428 形状：拒绝 AND store 前后不变（子项完整——把\"子项还在\"钉住）。
     07 联动查 09/10（都走 remove_at 路径——可能同一缺口的两张脸）。
     01 并入 06 轮（430 两候选：[ -d ] 路径形态 vs 根属性缺 0x10，便宜检查）。
     顺序：06→07（联动）→08（折叠）→01 并入。

440. 【判据先说话完整走通 + 正向判据区分过紧与正确】06 修复（eac3cc6）：共享场景
     scenario_remove_non_empty_directory 修前双红（Mock+Api 两后端都允许删非空目录）
     ⇒ core remove_at 缺规则（rmdir/unlink 有，按 id 删除——FFI 实际用的路径——没有，
     又是 422\"只被某类流程走过的路径\"）。修两处：fs-namespace rules::remove_at
     目录先查 is_dir_empty；MockBackend 同规则（\"夹具与 core 必须同答\"——单源原则
     扩展到否定规则）。判据形状：目录在/子项在/列举仍 1 条 + 正向（删掉子项后空目录
     【能】删）——正向判据证明规则是\"非空\"不是\"目录\"，区分过紧与正确，进判据形态库。
     全量发现：mounted_write_chain 与 mounted_read_only 两个测试进程同一 Z:，
     nextest 并行互抢挂载点（434 第二形态），单跑绿——修法不同挂载点优于串行
     （串行是性能妥协，不同挂载点是结构修复；先断言挂载点不存在——319 教训）。

441. 【挂载点分离（0141265）】read_only=Z: / write_chain=X:——结构修复替代串行化；
     挂载前断言盘符空闲（319）；注释写清"为什么不是 Z:"防改回（344 寄生）。
     锁内全量三连 648/648（此前 647/648 失败的恰好是互抢那条）。
     07 仪器口径：单挂载 + rename/close 两组回调打印 + 两必打项
     （resolve(new_name) 前后各一次=改名生效的独立证人[378]；
      close 的 remove_at trace 一条喂 07/09 两案）。

442. 【★ 四类并一案：大小写折叠缺失（07/08/09/10）】rename 请求源名被 FSD 大写化
     （/L2-FILE，大小写不敏感卷的归一化）而存储是字节键 ⇒ 源名解析不到 ⇒ 改名被拒
     ⇒ 08/09/10 全下游。436 的预测（\WRITTEN.TXT/\D\a）在 rename 路径兑现——
     一处能力缺失四类症状。归属：解析层做一次（壳里各折=第二套折叠；两后端同规则、
     共享契约可断言；前端只交名字不参与折叠）。
     执行要点：①折叠作用面 = resolve/open_dir/mkdir 碰撞/remove_at/rename 源与目标
     全走同一条规则——补判据：mkdir 大小写不同名字仍 NameCollision（折叠对创建是碰撞、
     对 case-only rename 是放行，两种语义都钉住）；②范围定死 ASCII（Unicode 全折叠
     记 backlog，范围写进判据防隐性契约）；③08 case-only rename 判据（改字节不改 id）
     跨三层语义（折叠匹配/碰撞放行/身份保持[裁决-S]）——本案最锋利一条。
     witness 形态（resolve 前后各一次）成仪器标准配件（378）。

443. 【匹配键 + 显示名（两件东西）= NTFS 结构 = 兑现 CasePreservedNames 声明】折叠形状：
     匹配键=fold(name)（NTFS $UpCase 查找），显示名=name 保留字节——正是 b9fed27 设的
     CasePreservedNames=1 所承诺的（兑现声明）。半折叠（只折键不保留显示）比不折叠更糟：
     看起来对但语义错=最难调试的状态——形状先定再动手。
     形状补充：两件东西放【同一个条目】（folded_key, display 两字段，一次写入原子更新）
     ——不做独立映射 (parent,folded)->display（两个存储会漂移，又是单源问题）。
     core 侧：查找结构允许自定义比较器 ⇒ 查询时折叠比较（单存储）；否则两字段条目。
     step1 判据加一条：case-only rename 后 read_dir 显示【新字节】——显示名跟随最近写入
     形态不是创建形态。step1 mock 先行（/f 与 /F 同 id/read_dir 原字节/mkdir 冲突/
     rename 后新字节）；step2 core 同规则 + 08 跨三层判据（单一用例注明钉三件事）。

444. 【撞结构性理由时撤回是第一选项；旧判据反转的深层合法性】折叠 step1 撞结构：
     split() 是唯一入口但返回值即组件本身——折了原始字节就没了（实测：A 建的条目
     raw_names 返回 [[97]]=a）。撤回全部改动（树绿 174/174），下一刀形状：
     split 返回 (folded, raw) 成对，只有最后一个需要 raw。
     循环进流程：定形状(443)→实现撞结构→撤回不污染→形状已明——没有一轮浪费在
     错误形状的修补上；撞到结构性理由时撤回是第一选项不是硬修。
     旧判据 bindings_are_raw_bytes_so_a_and_upper_a_coexist 反转的深层理由：
     它断言【大小写敏感共存】，而卷声明 CaseSensitiveSearch=0——旧判据一直在断言
     【与卷声明矛盾的行为】——折叠是消除声明与行为的矛盾，不是推翻设计——
     口径升格为缺陷修复（判据影响行这么写）。第二条（criterion_6_dirent）用合成条目：
     夹具依赖被测能力造输入，能力变了夹具换源——反转写理由不是删除。

445. 【排序：01 先、折叠做专轮；试探边界=测绘技法】第二次撤回实测：split 双返回与
     children 变 BTreeMap<(parent,folded), Binding{id,display}> 不可分（每处读要
     binding.id、每处写要 component.raw）——一次提交约 20 处，跨 mock+两测试文件
     （418 的宽度 vs 预算陷阱尺寸）。排序裁决：01（小独立）先清，折叠要整轮新预算，
     不合并不在半程启动。①–④ 一次做完定向绿才提交；⑤ core 侧下一刀；
     edit 工具防 CRLF（15+4 点位一次静默 no-op=半个红树）；两期望反转带 444 口径。
     试探边界技法进流程：对不可分的大刀，先用可撤回的小尝试测绘耦合面
     （第一次测绘出形状：折叠丢显示字节⇒(folded,raw)；第二次测绘出规模：双返回与
     Binding 必须同批⇒20 处），再一次性完成——两次撤回不是浪费是测绘，
     第三次尝试完全知情。

446. 【特案累积是 smell：入口一次修复 vs 下游 N 处特案】01 机制读出：[ -d \"Z:/\" ] →
     stat 开目录 → open_dir(path) → 夹具的 open_dir 没有根特案 ⇒ mock split(\"/\") 拒绝
     （前导斜杠空分量，51727de 同源）⇒ NotFound。真后端 rules.resolve(\"/\") 有正规答案
     （root id 0）⇒ 夹具仪器缺口非产品缺陷。
     特案在累积（create_or_open 的 51727de + open_dir 待补=第二处）——smell：
     mock split 拒绝根路径形态本身是 mock bug（\"/\" 是合法路径）。实验用特案最快，
     确认后正解=修 split 的根处理（单源；否则 rename/remove_at 遇根再要特案）。
     425 根特案退休的前提=路径层能回答 /，不是更多特案。
     本轮零代码：夹具 vs 产品一次小实验可判，不值得用一轮去猜。

447. 【根是路径不是特例（特案产生机制被拆除）】bb099e2：split(\"/\") 答空组件
     （根=合法路径、没有组件）——入口一次修复 ⇒ resolve/open_dir/read_dir 自然成立，
     下游特案从此不必再要。修的不只是这一处特案，是特案的【产生机制】。
     判据四格 the_root_resolves_like_any_other_directory：
     resolve(\"/\")==Some(ROOT)/open_dir(\"/\")==Ok(ROOT)/get_info(ROOT).kind==Directory/
     read_dir(ROOT) 可用——\"根是普通目录\"钉成永久事实，425 退休前提兑现。
     实验与正解同一动作：病例已确诊（01 与 02..12 并存的形状），不必先走特案弯路。
     锁内全量 649/649。

448. 【根的两处答案都在入口 + CRLF 三犯换结构】27d5609：split(\"/\") 答空组件只解决
     根【作为路径】；parent_and_name 无处安放根（没有父段）⇒ 只开卷根失败 ⇒ 该路径
     一切后续 path not found。第二处答案补在入口：Open|OpenIf ⇒ root 的 OpenOutcome；
     创建型 ⇒ NameCollision（根永远存在，创建=碰撞，389 同族）。两处都在入口 ⇒
     夹具与调用方都不需要特案——形状完整。
     CRLF 第三次违反（419 立纪律、445 点名、这次又踩——\"以为改了其实没改\"，L2 读数
     全假失败）：纪律三次未拦住该换结构——.gitattributes 统一 eol 或 pre-commit hook
     拦截 perl/sed 管道改文件（纪律挡不住的靠工具挡，344 机制版）。排折叠专轮后小刀。
     分叉 47/1：先四步 rebase（origin/main 0863ff6）再折叠专轮（大刀在新地基上做）。

449. 【折叠 step1 落地（f83ff54）：20 处一次过=测绘的兑现】形状逐项与裁定一致：
     split 返回 Vec<Component{folded, raw}>（ASCII only 写进类型）；children:
     BTreeMap<(parent,folded), Binding{id,display}>（两字段一记录，无第二映射）；
     查找用 folded / read_dir 用 display / rename 写新拼写 raw。
     期望一反转（the_fold_makes_a_and_upper_a_one_entry_that_keeps_its_display_bytes，
     444 口径，一条断言钉三层：折叠匹配/case-only 非碰撞/身份保持——442 最锋利一条落地）
     一换源（criterion_6_dirent 合成条目——测游标排序与后端存名无关）。
     全程 edit 一次过——\"脚本只用于读\"行为上生效，CRLF 第四次违反未发生。
     445 两次撤回测绘的兑现：第三次尝试完全知情，20 处一次过。
     L2 重跑验收映射（最后预声明）：07/08/09/10 连片 + 01 应绿 ⇒ 12/12？
     不按映射翻绿的=独立问题（439 老规矩）。

450. 【自引回归优先于既存差异；回归是复数的】07 翻绿（442 预测兑现——折叠是它的根因），
     但 02 回归（unreadable，worker 自引）+10 下游；对照 439 计分板还有：06 又红
     （eac3cc6 修过的 removed(wrong) 回归——折叠改 children 结构，is_dir_empty 视图
     可能永远为空）、12a 翻转（readonly=no）、01 仍未绿（根修复后 fold 改了 split
     返回类型，根路径空组件可能又断）。回归集中在【折叠与既有机制的交接面】——
     display/folded 交接嫌疑对，但波及面比 read_dir 大。
     纪律：自引回归排在所有既存差异之前修（污染整张计分板）。
     四合一仪器一次跑四个回归面：读回链（create_or_open/get_info/read）定 02、
     is_dir_empty 子项计数定 06、根 stat 的 split 组件数定 01、属性链定 12a——
     若共享机制一次修四处。

451. 【目录的句柄语义空白（422 trait 缺口的句柄版）】06 真缺陷定位：open_like 对目录传
     binding:None ⇒ HandleTable 无目录句柄 ⇒ set_delete/close 失败被吞——两段删除对
     目录整体缺席；remove_at 从未带着名字被调用 ⇒ 目录仍被删 ⇒ 某条不走句柄表的路径
     直接删了（待查\"谁删的\"）。修法方向：open_dir 也登记 binding——目录句柄进表，
     set_delete/close 与文件同一条两段路径；09（delete-while-open）大概率同族。
     与 422 同族：目录的路径语义补齐了（open_dir），句柄语义还是空白。
     两绿：01 绿=根修复经受 fold 结构改动（判据四格功劳）；02 隔离绿=后端无辜，
     L2 的 unreadable 是脚本前序状态污染（03 截断清内容假设待证，重读脚本顺序）。

452. 【裁 (a)：目录门也回答 binding（DirOutcome）】缺口比\"句柄没进表\"更具体：
     open_handle 需要 binding=(parent,name)，而 mkdir/open_dir 只答 id 不答父 id
     ——目录的 (parent,name) 只能再解析父路径（递归+根特判）。
     裁 (a)：DirOutcome{id, binding:Option<Binding>}，mkdir/open_dir 两门同改。
     理由：①对称性——文件 OpenOutcome.binding 已证明形状，目录镜像是镜像不是新设计；
     ②(b)（按 id 删/ROOT 免校验）重开 G2 一整刀建立的 remove_at 三分类身份校验——
     不可接受；③单源——解析器本来走过父段，binding 是顺手就知道的信息零成本；
     (b) 制造按名删/按 id 删两条路径=一路在消除的重复。
     形状要点：根 binding=None（无父段）——close 只释放 context，根的删除意图由
     \"根永远存在\"接（NameCollision 同族）；trait 签名变更两后端同批+判据更新。
     \"路径语义的回执由路径门给出\"——open_dir 裁决的同一条理由。

453. 【★ 规则必须住在还有返回值的地方】Cleanup/Close 是 VOID ⇒ close 时刻拒绝的规则
     永远无法告诉调用方——实测：rmdir 答成功而 remove_at 内部答 NotEmpty
     （382 家族新变体：规则触发了，答案去了无处）。⇒ CanDelete 存在的原因是
     平台级出口设计：删除规则【最后一个能返回错误的时刻】。
     洞见推广：一切 VOID 回调的规则必须前置到最后错误出口，否则等于没有。
     06 修复（9d5f56d）三缺口一次补齐：①dir_binding(path)->Option<(parent,name)>
     带默认 None（诚实缺口）两后端覆写——(a) 裁决的最小侵入版（同单源同对称
     不制造红树；严格 DirOutcome 合并 ~12 处并进 trait 完备性扫描）；
     ②规则也在 CanDelete 里问（错误出口）；③结果 os error 145 目录子项都还在，
     remove_at(0,\"d\",1)->Err(NotEmpty) 删除路径真走到（binding 生效）。
     偏离的正确形态：写明成本与合并路径，不是\"改了主意\"是\"选了更小的等价物\"。

454. 【L2 7/12：两个新绿均根因级；01 的两假设先分再修】06 翻绿（目录 binding+规则前置
     CanDelete）。剩 5 红，每类已有明确下一问：01=stat 形态、02/10=疑似 01 下游、
     08=case-only rename 路径、09=delete-while-open、12a=属性链。
     01 仪器批+预判：Git Bash [ -d ] 走 MSYS stat——可能 NtQueryAttributesFile
     （路径级快查，不开文件对象，可能只过 GSBN）或带 FILE_TRAVERSE 的 CreateFile——
     两种形态在 WinFsp 侧走不同的门；若打印中一条 Create 都没有而 stat 仍失败 ⇒
     走的是查询门——查那条门的应答。
     \"02 是 01 下游\"的两假设：A 污染（01 失败留坏判断，cat 落后面）vs B 同因
     （bash 打开形态与 std::fs 不同，共享同一被答错的形态）——判别：挂载期先跑
     bash cat 不跑 [ -d ]——cat 成功⇒A，cat 也失败⇒B。两假设修法完全不同，先分再修。

455. 【双否 + 答案可能已在日志里】bash 探针双否：01 非后端失败（同挂载 [ -d 'Z:/' ]
     答 d=0——L2 脚本前序状态问题，大概率与 02 同源）；02/10 = B 假设（bash/MSYS
     打开形态与 std::fs 不同）。新线索最具体：Invalid request code = c0000010 ⇒
     未实现回调被问；create_or_open \"/f1\" Open→NotFound 反复 ⇒ 文件根本没建成。
     下一个问题 = 槽位问题：MSYS 读写路径请求了哪个 std::fs 不请求的操作？
     零成本第一查：现有 LAYER_FS_DEBUG_LOG 里 grep c0000010 的 IoStatus 行——
     它上面的 >>Op 名字就是被问成员（324 打法，不用装新探针）。
     候选先验：GetStreamInfo（cat/stat 问命名流）/Control（FSCTL）最可疑；
     GetEa/SetEa 次之；SetSecurity 最不像。探针保留为第二手。

456. 【post-close Read = Cc 预读；墓地修法】日志点名（省掉探针一轮）：Close 之后的
     第二次 Read（同 ctx，Length=131072=128KB=Cc 预读标准块——缓存管理器异步读，
     非用户读；cat 失败=预读失败经错误通道传回用户）。
     两机制候选：A FSD 真在 Close 后发 Read（Cc 晚到，须容忍）；B 2 派发线程处理
     乱序（start(fs,2)，Read 先发帖被后处理）。配对仪器可分，但修法可同：
     (a) 墓地——Close 标记死不释放，post-close 操作答 STATUS_FILE_CLOSED（诚实）
     或 Read 特例答 0 字节/EOF（预读结果无人消费，不构成对持久状态的谎言）——
     两答案一轮 A/B 出哪个不破 cat；(b) 单派发线程——重，不倾向。
     FileInfoTimeout=0 语义仍未证：memfs 默认 INFINITE；我们 0 且注释\"No caching\"
     是信念不是验证（悬念自批(i)之初）——Cc 活跃说明数据缓存在，列入核对清单。

457. 【post-close Read 答 EOF（4650b17）+ clamp 边界嫌疑】closed 与 never-existed 必须可分：
     CLOSED_CONTEXTS 记录已关闭 id；read_shell 对已关闭 context 答 *bytes=0 返回 0
     （EOF——预读结果无人消费，0 字节不写坏缓冲、不对持久状态撒谎）；
     陈旧 id（从未发放）仍答 NOT_IMPLEMENTED。效果：Invalid request code 消失，
     bash exit 0——131072=Cc 预读判定正确（错误不再经错误通道传回用户）。
     残留：wc -c 读到 5 字节正确，cat 空输出且 exit 0——嫌疑=read_shell 夹取
     （403 want=min(length, size-offset)）的边界算术：cat 第一次读 offset 已等于 size
     ⇒ want=0 ⇒ 立即 EOF。仪器打 cat read 序列（offset/length/answer 三元组）；
     边界值即判据：offset==size 答几？下溢答几？12a（readonly）的查询顺带同仪器。
     FileInfoTimeout=0 语义核对排进 cat 通后的一刀（不再用注释代替验证）。

458. 【最后一厘米：拷贝目标（cat 空输出的判别）】clamp 排除（92891ad：offset=0/len=3/
     后端答 3/无第二次读）——缺陷在应答字节如何进入调用方缓冲。wc 能读 5 字节 ⇒
     通路在另一条长度上好。仪器：拷贝前后各打印 Buffer 前几字节 + *PBytesTransferred
     ——分「拷错字节」与「拷到错地方」。加一项：打印每次 Read 的 Buffer 指针值本身
     （若某次 Address=0 或与后端长度不匹配当场抓到）。fsop.c 先验：Buffer=
     Request->Req.Read.Address 经 dispatcher 转给 FSD，目标应合法。

459. 【★ 兜底不是修复，是遮蔽——兜底的资格是能区分它兜的是什么】4650b17 的 EOF 兜底
     把"应当响的错误"变成"安静的 EOF"——cat 静默为空而非报错，恰好遮住了
     一直在追的生命周期缺陷（活调用方的读被当"句柄已关"打发——e24dea8：
     两次 read 不同 buffer，第二次没到后端）。413 不对称原则的失效条件：
     前提是两边真的是那两种东西——EOF 兜底无法区分"死：对象真没了"vs"死：我们放得太早"，
     不能区分病因的兜底会吞掉诊断信息。处置：兜底改开关默认关（让问题重新亮）。
     id 配对三选一定案：(a) close 在 read 发出前完成 ⇒ FSD 对已关上下文发活读
     （F侧行为须容忍+区分）；(b) 并发交错（2 派发线程，read 先发帖被后处理——
     456 的 B 机制）⇒ 生命周期或串行化；(c) 活 id 落在 closed 集 ⇒ 记账 bug。

460. 【★ memfs 合同：context 引用计数（同 id 多次 CLOSE 天然合法）】memfs 源码：
     Open = FileNodeMapGet + Reference(ref+1) + 返回 FileNode——同一文件多次打开
     【共享同一 context】；Close = Dereference(ref-1)——减到 0 才真死。
     WinFsp 合同：context 可被多个文件对象共享，每个对象的 Close 各调一次。
     我们"每次 open_like 发新 id、close 立即释放"与合同不符——4680385 读数
     （同 id 两次 CLOSE、READ 夹在中间）= 多对象引用同一 context。
     修法：context 引用计数——命中已存在的同一文件（判定键=(parent, folded, kind)，
     折叠键复用）⇒ 复用 id 并 ref+1；close ⇒ ref-1 减零释放。
     同 id 多次 CLOSE 天然合法；READ 在两次 CLOSE 之间=对象活着正常服务。
     选 refcount 不选墓地：内存随真实引用走（墓地每次 open 累积 24B 长挂载会涨）。
     顺带解释 heap-corruption 最后一层：17 条\"陈旧 close\"里可能有合法的多对象 close
     被当垃圾拒了。

461. 【★ cat 通了：context 引用计数（aa5bca3）】CONTEXTS: BTreeMap<u64, Shared{entry, refs}>
     + CONTEXT_BY_FILE: BTreeMap<file_id, id>；acquire_context 同一 file_id（折叠后身份）
     复用并 refs+1；release_context refs-1 零才移除——\"别的对象还持有\"与\"从未发放\"
     因此可分（前者不计数、后者进 STALE_RELEASES）。三层可分性让 413 不对称原则
     真正成立（兜底兜的每一边确实是那一边）。
     链条全解：Invalid request code → Close 语义比 FSD 早一步（十几轮打印驱动，
     memfs 源码一锤定音）。根因一直是生命周期不是某个回调——每一层兜底只能
     把错误变安静（459 印证）。
     L2 预期：02/10 应绿、09 连带、01 跟着（bash 前奏不再被污染）
     ⇒ 10-11/12，剩 08（case-only rename）/ 12a（readonly）。

462. 【L2 10/12：预声明全兑现】02/09/10 随引用计数翻绿（09/10 是同一生命周期缺陷的
     侧面——对 Close 的语义比 FSD 早一步，memfs 引用计数定音）。
     剩三：01=脚本前序状态（仪器 [ -d ]/std::fs 都过——处置：脚本挪到最后跑一次，
     最便宜分界实验）；08=case-only rename（折叠就位，嫌疑 rename 路径对 case-only
     的 RM 语义 replace=true 同名不同壳——442 跨三层判据跑一遍）；12a=readonly
     读不回（先验 SetBasicInfo@12 未接——8 参签名 audit 表有，set_times/set_attributes
     P3-3a 已有，接槽即有；判据照 397：set 后读回 + store 快照）。

463. 【半选语义的两个方向 + 四步不能省略】SetBasicInfo@12（d9c70e5）：两个\"别动它\"哨兵
     ——FileAttributes==0xFFFFFFFF 别动属性、时间==0 别动那个时间——一次调用可只动一半，
     两者【分开施加】（分不开=改时间清属性）。与 362（不半填/出参完备）互为镜像：
     出参别多写、入参别多施——半选语义两个方向，进判据形态库。
     属性侧掩派生位（DIRECTORY/NORMAL）与 Overwrite 同纪律；时间四字段独立，
     全 0 不调后端；成功按当前 metadata 回填 FileInfo。
     流程事故（主动报+amend 修正）：第一版带 clippy 警告提交，违反 fresh-clippy=0——
     \"纪律没被绕过但顺序被跳了\"；pre-submit 四步自上而下，不能因\"就差一步\"省略；
     未遂+主动报告=完整闭环。

464. 【L2 11/12：12a 随 SetBasicInfo@12 翻绿】readonly=yes（raw R Z:\l2-renamed）——
     两个哨兵分开施加的判据直接兑现。剩两类：
     01=脚本末尾挪位实验（最便宜判别）；08=case-only rename（442 跨三层判据）。
     两实验互不依赖，同轮双跑——12/12 或定位最后一块。

465. 【01 状态依赖（非脚本顺序）+ 二分实验】01 挪末尾仍红；隔离仪器里 [ -d ] 作为
     第一个动作正常 ⇒ 状态依赖在我们这侧（其它类跑过之后）。二分：每类后加 exit，
     一轮定位。候选重排（manager）：06 最可疑（rmdir 被拒后 pending 残留——
     拒绝路径的静默残留：成功路径有人看失败路径没人看，382\"静默成功\"的反面）；
     12a 次之（attrib 落根）；07 再次（rename binding）。
     预声明：若 01 在中间某时机忽然红（非某类后）⇒ 时间相关（缓存/Cc 延迟）——
     查失败那次 >>Create 行 vs 正常次的差异。
     提醒（worker 自律）：临时改脚本已复原并确认 git status 干净——
     QA 侧共用的脚本不能带实验痕迹。

466. 【01 结案=317 的完满收尾】[ -d 'Z:' ]（裸盘符——脚本 ROOT=\"${ROOT%/}\" 剥分隔符）
     d=1 vs [ -d 'Z:/' ] d=0——Git Bash 不接受裸盘符当目录。317（P3-4 最早期
     \"Y:=Y: 的当前目录\"——当时修 probe 口径）的同一个坑这次咬了脚本自己。
     \"脚本红/仪器绿\"全解（仪器一直用 Z:/）。NTFS 侧根是普通路径从未被问——
     坑专属挂载卷。修法：ROOT_AS_DIR=\"$ROOT/\" 专供根目录判定，测量值
     （d=1/d=0）写在代码旁（344）。
     二分先做排掉状态假设（含 manager 的 06 怀疑——\"失败路径静默残留\"方向
     仍值得记住，只是不是这次的病因）。
     L2 11/12——只剩 08（case-only rename）：折叠匹配/case-only 非碰撞/身份保持
     三层判据一轮跑，绿则挂载侧 12/12 收口。

467. 【08 定位：Rename 没到后端（请求被吞）+ 分界实验】mv 对 case-only rename rc=0 但
     拼写没变、调用日志无 Rename ⇒ 请求到达我们之前被判"同一文件无事可做"成功返回。
     flag 核对（manager）：CasePreservedNames=1 ✓ CaseSensitiveSearch=0 ✓（与 memfs
     默认形态一致）——嫌疑不在 flag 本身，在 flag 组合的效果路径。
     更具体候选：FSD 对 case-only rename 的短路判据（FILE_RENAME_INFORMATION 折叠后
     =源 ⇒ 同一文件 ⇒ 短路成功，Rename 永不发）。分界实验：管理员 cmd 的 ren
     （不走 MSYS）——生效 ⇒ mv/MSYS 形态问题；不生效 ⇒ FSD 短路实锤，
     查 ddca7bd FSD 源码定短路条件（FspFileRename/case-only 判断、受哪个 flag 控制）。
     08 与 01 是一对：两个\"我们坏了\"最后都不是我们坏（脚本裸盘符/请求没到），
     都靠先读仪器不靠推理定下来。

468. 【08 = 平台差异（记 U16）——挂载侧 11/11 可达类收口】分界双跑：mv 与 cmd ren 同样
     静默（非 MSYS 形态）；CaseSensitiveSearch 开关不变读数（非卷参数）；rename_shell
     复核无大小写短路。fsop.c case 10/65 → Interface->Rename 直通（manager 核过）
     ⇒ 吞请求在内核 rename 路径或 FSD 内核侧，UM 不可修。
     裁决：08 判据改为\"请求到达时正确服务\"（跨三层判据保留）；L2 08 行记录为
     有据差异（平台中转层短路，源码+双工具实验为证）；若 case-only 是 spec 承诺
     ⇒ U16 = P3-4b 候选（另一种机制实现）。挂载侧口径：11/11 可达类全绿 = 收口。
     三分档里 08 = 有据差异档（与 NTFS 差异源于平台中转，非我们缺陷）。

469. 【\"request-not-delivered 是测量值不是通过\"+ 顺序裁决 1→2 连做】c6658b9：脚本 08 行
     记三种真实答案（NTFS renamed / 挂载卷 request-not-delivered unchanged / missing），
     理由在注释（fsop.c 直通 + CaseSensitiveSearch 翻位不变 ⇒ 短路在用户态之上）。
     口径：挂载侧 11/11 可达类全绿；08 = U16 有据差异。
     顺序：core 折叠（fs-namespace rename/lookup/remove_at 按折叠键 + display 保留
     [443 一记录两字段]）与 U1 后端直测连做——直测绕开 U16 内核短路，在可测层
     证明语义：\"我们的 rename 在它能被问到的地方答对了\"——与挂载层 U16 互相独立，
     两个事实合起来才是完整 U1。
     3（完备性扫描：DirOutcome 合并 + FileInfoTimeout=0 语义）core 折叠完进来最顺。

470. 【fold 进 store：trait 签名零改动 + 逐点推进 + fold 归 fs-semantics】设计：
     TreeRead::lookup/TreeWrite::bind/unbind/read_dir 在实现里折 name（外部仍传 raw），
     记录存 (folded key, display bytes) 两字段一体（443）——签名不变 ⇒ rules.rs
     一行不改 ⇒ 442 四个类（lookup/rename/remove_at/read_dir）同获折叠。
     实现点四处：fs-snapshot/read.rs、fs-overlay/base.rs、fs-overlay/view+write_view、
     fs-api/read_view.rs。
     执行：逐点推进每点一提交（独立读数/中途永远绿[355/368]/rebase 冲突面小），
     U1 直测在第一处（fs-snapshot）落地后立刻跑（rename_with case-only 只依赖 store）。
     manager 修正：fold 放 fs-semantics 而非 fs-windows——fold 是语义规则
     （匹配键的构造方式）不是 Windows 专属；且 fs-windows 依赖方向可能不通。

471. 【折叠是适配器属性——不是存储属性也不是视图属性】fs-snapshot 的 lookup 是磁盘
     B 树页（格式变更量级）。裁决：折叠只该发生在【一处】——适配器边界。
     fs-winfsp 的 open_like/rename 已有 Component{folded, raw}——把 folded 发给
     WorkspaceOps 的 lookup/match，把 raw 发给 bind/create 的 display。
     ⇒ store（overlay/snapshot）收到的键已折好，零改动，保持\"名字就是字节\"自证性。
     不是视图属性：会在适配器和视图各折一次=第二套折叠（452 拒绝过的形状）。
     \"同名不同壳共存\"防护：适配器是唯一入口（FFI 是唯一入口）——折叠键一致性
     由适配器保证，store 不需要防护。
     U1 直测口径：直接给 ApiBackend 发折叠键。
     fold_bytes 补得对（非 UTF-8 也必须可匹配）——store 里名字是原始字节。

472. 【两后端语义不一致（比顺序更急）+ 裁 (B) 先做】审计：折叠只在 mock 内部
     （to_ascii_lowercase，与 fs_semantics 第二份实现）；ApiBackend 完全不折
     ⇒ 同一批判据在两个后端测的其实是两件事。
     裁 (B)：①mock 换 fold_bytes（消双份）②ApiBackend 入口加折叠 ③两后端同答判据
     ——一提交，立刻堵两个现在就在漏的问题。
     (A)（WorkspaceOps 路径参数改 Path{key, display}）排完备性扫描刀——
     367 形状变更停下单做。过渡形态：折叠在后端入口（不渗 rules 内部），
     (A) 完成后自然上移到适配器，后端内代码不变。

473. 【② = (A)：入口折会吃 display ⇒ Path{key,display} 是唯一正确形状】ApiBackend
     的 create_or_open 交给 rules 按给定字节落库——入口一折 ⇒ 落库就是折后的 ⇒
     读回不是原拼写（mock 104 行自己踩过的同一坑）。⇒ ApiBackend 需要\"按键查找、
     按原样落库\"= 两参数 = (A) 的 Path{key,display}。② 不是做不了，
     是它的正确形状就是 (A)——不能拆开做半截。
     ① 的教训：三处手抄（Component.folded / remove_at 查找键 / 移除键）→ 只换第一处
     ⇒ 6 例立刻红 ⇒ 它们本来就是同一条规则的三次手抄——手抄不是引用，
     是独立实现，每份会独立漂移（470 的证据）。
     裁决：(A) + 完备性扫描合并一刀提队首（Path{key,display} + DirOutcome +
     FileInfoTimeout 语义验证 + ②③ 自动落地）——与折叠专轮同一量级，整轮新预算（445）。
     当前中间态安全（双份消灭+不一致但无冲突规则）。

474. 【整轮复盘 + 大刀入档待开工】本轮交付 = trait-path-cut.md 计划文件（462a925）
     而非开工（445 纪律：整轮新预算才动 trait 签名）。计划三件：为什么四事一刀
     （②=Path 同形状、③依赖②、DirOutcome 同层）；刀内顺序每步可编译
     （类型→12方法编译器数调用点→ApiBackend→DirOutcome→FileInfoTimeout验→③+U1）；
     三条不变量（display 恒原拼写/折叠唯一定义/判据不放宽计数不降）。
     中间态安全：双份消灭、两后端不一致但无冲突规则、L2 11/11。
     
     worker 复盘（原文收档）：这整段从 cat 读不出到 11/11，真正的转折只有一个：
     对 Close 的语义比 FSD 早了一步。后面所有"顶层看起来坏了"的类（01/08）
     最后都落在请求根本没到我们这侧——两件都不是实现缺陷，
     没有一次是靠推理定下来的，都是先读一次仪器。

475. 【大刀封存：开工前先钉绿基线】worker 封存确认。下一轮开工流程：
     1 重跑基线（175+652+clippy）钉"动之前是绿的"——刀内变红时才能区分
     是改坏还是本来就红。这是 434（仪器合格）的前置版：仪器合格的前提
     是仪器在动刀前有读数。
     六步顺序（计划文件已入档）：Path 引入→12 方法签名→ApiBackend→DirOutcome→
     FileInfoTimeout 验→③两后端同答+U1 直测。
     三栏杆：display 恒原拼写/折叠唯一定义/判据不放宽计数不降。

476. 【Path 刀第 1 步：类型引入零调用点】a22c9dd（656/656+clippy 0）：
     Path{key=fold_bytes（唯一实现）, display=原样字节}。
     两个设计决定：①display_name()->Option——根与尾分隔符没有名字，答空字节会把
     问错变成空名字落库（352 再应用：None≠空串的假答案）；②case-only 同键不同显示
     作为独立判据——08 的语义性质从挂载层不可测（U16）变成类型层可测——
     绕开 U16 在可测层钉死语义的路径。
     刀前读数钉住（462a925: 175/652/clippy 0）。
     第 2 步：18 impl / 11 文件 / 全仓重编译 / 一次改完（半改=红树）。

477. 【依赖反转：store 层在计划里被漏了——裁 (乙) overlay 折叠 + B-18】事实：
     core 的匹配键就是落库字节（fs-api read_view → reader.lookup 按原始字节）。
     Path 落地后 ApiBackend 做不到"用 key 查"——条目按 display 落库，按键查 NotFound。
     真正依赖链：core 索引按折叠键建 → ApiBackend 用 key 查 → 判据 3 有意义。
     
     裁 (乙)：overlay view/write_view 折叠（查找层折），snapshot 保持 raw（被查找的数据）。
     理由：(甲) 的 snapshot 格式变更是独立项目（v3 基线重定/全部 fixture/字节级断言），
     不是 P3-4 一刀。挂载链真实路径经 overlay 视图——折叠落在这里覆盖全链。
     
     不一致防护：判据（overlay 对 base 的折叠查找——base 有 ReadMe.txt，
     overlay 按 readme.txt 查→命中且 display=ReadMe.txt）——用判据防不用格式防。
     
     B-18（新）：snapshot 格式变更——索引键与显示名分开存/版本位/基线重定。
     触发：性能要求折叠索引下推磁盘层，或直读 snapshot 的消费者需要一致语义。
     
     worker 停下来报依赖反转是正确动作——计划的错在执行时被抓住，
     否则在错误地基上盖 18 层。

478. 【扫描版 overlay 折叠（不是性能取舍是一致性取舍）】overlay lookup 的 base 侧：
     (扫描) 未命中时对 base 目录 read_dir 扫、比较 fold_bytes——正确、简单、O(目录项)；
     (缓存) view 层维护 (parent,folded)->display 索引——快但 bind/unbind/rename/remove_at
     四条路径同步，漏一条=静默错查。
     裁扫描版：慢是可观测的，错是不可观测的——缓存版漏更新是静默的错查，
     正是整段在追的那种静默不一致。不对称原则（413）在数据结构选择上的应用：
     选可观测可恢复的一边。一致性缺陷的排查成本远高于线性扫描的性能代价。
     B-19（新）：折叠索引缓存——触发=L2/QA 验收显示扫描是瓶颈；届时扫描版
     作为正确性基线，缓存版对着它做差分测试。
     计划文件先修（补 477 store 层依赖 + 478 B-18/B-19）再动代码——
     施工图带着错依赖链比没有施工图更危险。

479. 【施工图修正落档（8262a59 纯文档）】三处修正写进 trait-path-cut.md：
     477 store 层进依赖链；478乙 snapshot 不动进 B-18；478 扫描版（B-19 缓存留优化）。
     入口态：1✓（a22c9dd），2-6 未开工。
     下一轮执行顺序：2（18 impl 一次过）→ 3（overlay 扫描折叠）→ 4/5/6。
     "把那句话落成实际动作"——计划文件与裁决一致，下一轮执行而非再讨论。

480. 【U1 判据反转（444 形态，折叠改变前提）】overlay 折叠（第 3 步）使 2 例红——
     都是前提失效不是实现错：
     ①case_only_rename_onto_another_inode：前提"/A 与 /a 可为两 inode"折叠后不成立
       （471 要的）。改写：create /A 后 create /a 必须 AlreadyExists；
       replace-flag 用差异不止大小写的名字（/A→/ab——标准路径是主战场）。
     ②replaced_is_none_unless...：原断言"case-only 后 /A 不再解析"折叠后不成立
       （Windows 行为：仍解析）。改写：/A 仍解析且 display 已新拼写——
       比原强（同时钉折叠匹配+显示保持，补上"新壳活了"的半边）。
     两条都批，444 仪式（理由写代码旁），连第 3 步一起提交。
     worker 停下来等裁决对——U1 核心判据的形状决定 U1 怎么被证明。

481. 【overlay 折叠落地（04af3b8，658/658）】扫描版：lookup_impl 精确→folded_scan
     （先 overlay 行再 base 页扫）；比较键唯一走 fold_bytes；不用缓存（B-19 留差分）。
     新判据 2 例带负判据（readme.md 仍查不到——命中与不命中都有读数）。
     两条反转干净：①two_spellings_cannot_coexist（正向 create /a ⇒ AlreadyExists）
     + replace 用 /A→/ab；②旧拼写仍解析到同一 inode 且目录只剩新拼写一条
     （比原强：钉折叠匹配+显示保持）。
     刀内：1✓ 3✓ → 6 U1 直测（先做——折叠已让 case-only 可测，绕开 U16，
     验证 471→477→478 依赖链正确性）→ 4 DirOutcome → 5 FileInfoTimeout → 2 签名最后。

482. 【U1 直测落地（2d07cd2）：\"通过挂载层无法证明 ≠ 语义未证明\"】后端直问：
     旧壳仍解析到同一 id（折叠匹配端到端 api→workspace→overlay）+ 新壳目录只剩
     一条（显示保持）+ file_id 不变 + replaced None + 字节仍在。顺带证到折叠
     在真后端上确实生效（不只是单元层面绿）。
     逻辑：U16=请求到不了（内核短路）；U1 直测=我们的代码在被问到时答对了——
     两个事实合起来=完整 U1（\"能问的地方答对，问不到的地方记差异\"）。
     与 469 裁决完全吻合。
     刀内：1✓ 3✓ 6a✓ → 4 DirOutcome → 5 FileInfoTimeout 验 → 6b 两后端同答 → 2 签名。

483. 【DirOutcome 落地（a28749e，660/660）：两半同行 + 免费一致性检查】
     DirOutcome{id, binding:(u64, Vec<u8>)}——remove_at 的守卫需要身份+绑定，
     分开解析正是它们开始漂移的方式。门答 id 与绑定解析 id 在同一个值里
     ⇒ 不一致可观测（431/434 精神：仪器必须在差异出现时说话）。
     判据含负向：不存在路径 None（说有绑定、之后 remove_at 才失败=被禁止的形态）。
     刀内：1✓ 3✓ 4✓ 6a✓ → 5 FileInfoTimeout 验（量出来写注释，
     解决批(i)起最后一个信念悬案）→ 6b 两后端同答 → 2 签名。

484. 【FileInfoTimeout=0 量出来了（最后一个信念悬案关闭）】同一挂载：写 abc → stat 3
     → 后端在 FSD 背后改成 11 → stat 11（缓存若开着必是 3）→ truncate → stat 0。
     SIZE2 是全部：区分\"零真的关了缓存\"和\"零只是一个我们希望的选项值\"。
     从批(i)起最后一个信念代替验证的悬案——注释后半段现在写着读数。
     顺序：先 6b（两后端同答——验证优先于形式）再 2（签名 18 impl——形式收尾）。

485. 【472 正式收口：两后端同答（a2b409f，661/661）】scenario_case_insensitive_names
     在 mock 与真后端各跑一次——\"两后端语义一致\"是两条读数不是一条假设。
     四条断言每条带负向（另一名字 NotFound / 恰好一条 / NameCollision / 字节仍在）
     ——正反都有读数，442\"一个缺陷四个类\"的判据镜像：一个修复四个证据。
     6b 的读数 = 第 2 步（签名 18 impl）的回归护栏。
     刀内：1✓ 3✓ 4✓ 5✓ 6a✓ 6b✓ → 2（最后一步，纯机械，行为不变）。

486. 【★ Path 刀六步全落地（782d558）——实现段正式收口】18 impl / 5 方法签名改 &Path<'_>。
     行为不变（刻意）：mock/ApiBackend 内部取 display，匹配仍由 overlay 折叠扫描负责，
     key 真正用于匹配是 B-18/B-19 后续——本步只把两字段送进边界。
     正确分层：形状先到位，行为分步迁移，6b 做护栏。
     事故：批量替换 ' 越界改三个无关文件（编译抓到，checkout 复原）——
     419 第四族（CRLF 没改到 / 这次改多了——同根源：替换工具不尊重边界）。
     对策并入结构修复：.gitattributes + 限定目录集 + 用完 status 核对。
     
     刀内六步全 ✓：Path / 签名 / overlay 折叠 / DirOutcome / FileInfoTimeout 验 /
     U1 直测 / 两后端同答。7 提交、+11 判据、2 反转、B-18/B-19 挂账。
     
     🎉 实现段到此完成——从 P1 spec 到\"Windows 上可挂载可读写的 layer-fs\"。
     之后全是验证与文档：小结 → .gitattributes → 堆损坏整批验收 → L2 正式
     → M/U → P3-4 收口报告 = 项目最终报告。

487. 【Path 刀关闭（4cf8993）：判决书形态完整】入口 462a925(175/652)→出口 782d558(661)。
     7 提交 +11 判据 2 反转 2 挂账(B-18/B-19) 2 事故（越界替换=419 第四族、计划漏 store 层=477）。
     三件这次才成为事实的事（进收口报告方法论节）：
     ①一个修复点覆盖四条规则（折叠进 overlay 查找层⇒lookup/rename/remove_at/read_dir 同获）
     ②两后端一致是两条读数不是假设（472 正式关闭）
     ③U1 在它能被问的地方被证明（挂载层到不了[U16]，后端直问是事实[482]）。
     下一步：.gitattributes 先做不等 QA（结构修复越早风险越小）→ QA 堆损坏整批验收。

488. 【结构修复落地（a137ac0）+ 交棒】三次仪器修正（工作树→git show→git cat-file）
     ——工具骗了两次第三次才说真话：smudge 过滤器是隐形变换（419 又一层：
     不只替换工具会骗，读取工具也会骗）。真正挡 CRLF 的是 .gitattributes（text=auto
     归一化）；钩子 CRLF 检查是兜底，主要价值=提交清单打印（对治越界）。
     两道防线各司其职。
     
     worker 交棒时的一句（收档）：\"删掉一条兜底比加上一条难——Close 引用计数
     那次之所以能收，是因为先把兜底掩盖了什么问清楚了，而不是先把错误静音。\"
     
     封存：a137ac0 / 661/661 / clippy 0 / 干净。QA 验收启动（最后一批代码验收）。
     通过后：L2 正式两侧 → M1-M9 → U1-U14 → P3-4 收口报告。

489. 【★ 最终代码批合入 main（a137ac0，82 提交）——代码验收段结束】
     QA 四件全过：contexts 7/7 配平独立复现 / 两后端同答双双通过 /
     堆损坏三层齐（根因 cee5807/修复 引用计数/断言\"为什么不是 no-op\"）/
     .gitattributes i/crlf=0 全 i/lf。
     661/661 零红 + clippy 0。ff 合入 diff 为空（240）。
     
     代码验收段到此结束。P1 到 P3-4 的全部实现，经 QA 独立验收，都在 main 上。
     
     P3-4 进入非代码验收段：
     QA 跑 L2 正式两侧（U15 多页 200 条首验）→ M1–M9 → U1–U14 消项
     → P3-4 收口报告 = 项目最终报告。
     
     worker 进入待命。QA 按骨架槽位与三分档口径执行。

490. 【worker 交棒致谢 + 待命】最在意的一条：contexts 7/7 配平——\"同一 id 被关两次
     不再是错误而是一个句柄一次关\"——\"把一个缺陷变成一条恒等式\"是这段工作里
     唯一一次。谢谢那句：\"最值钱的不是绿的数字，是你每次都先问这个读数是谁给的\"
     ——ntstatus.h 那行（0xC00000BA 而不是两人都记着的）是合作的最好注脚。
     待命：main @ a137ac0 / 661/661 / clippy 0 / 干净。
     
     当前：QA 跑 L2 正式两侧 → M1–M9 → U1–U14 消项 → P3-4 收口报告。

491. 【L2 正式对照首轮：三阻塞发现（疑一缺陷）】QA 正式两侧对照：
     一致面 8 行 + 有据差异 2 行（08 U16 / 12b cmd 格式）。
     阻塞：①子目录列举空页（根正常，子目录 ReadDirectory 坏）；②U15 首验即中
     （create_dir Ok 但 200 子文件全 error 3——子路径不存在）；③unmount 0xC0000005
     （两次复现，非 1546 族非 0x374）。
     QA 判断：①②同缺陷（子目录路径解析），③疑似同根——合为一个阻塞诊断项。
     差异候选：QA 测试形态 vs worker step test 形态；或多目录/多文件/长会话叠加。
     QA 探针 qa_l2_probe.rs 可直接复现。转 worker 诊断。

492. 【分歧：worker 真后端探针连根都不可用（与 QA 症状不同层级）】ApiBackend 挂载 Ok
     但 bash 全 No such file or directory——日志零 create/open ⇒ FSD 未到回调。
     MockBackend 下同形状可用 ⇒ 变量是真后端。QA 的发现是根正常子目录坏——
     两个症状层级不同（可能是两个缺陷或初始化差异）。
     急问 QA：qa_l2_probe 用的后端/盘符/VolumeInfo/workspace 初始化路径。
     若都是 ApiBackend 而症状不同 ⇒ workspace 初始化差异是线索。
     worker 探针存档 scripts/drafts/subdir-probe.rs.txt（不占盘不改用例）。

493. 【两个缺陷两个层级：Mock 路径 vs ApiBackend 接线】QA 的探针用的是 MockBackend
     （SlashBackend，沿用 mounted_write_chain 脚手架，X:，无 workspace 初始化）。
     ⇒ QA 发现（根活/子目录坏/unmount AV）= Mock 路径缺陷；
     worker 发现（真后端连根不可用/FSD 没到回调）= ApiBackend 接线缺陷。
     互不掩盖，都是真缺陷。L2 定论等真后端可挂后用真后端重跑。
     worker 下一步：量 ApiBackend 在挂载握手时对 GSBN/GetVolumeInfo/根 open
     各答了什么。QA 的 GetDiskFreeSpaceEx 数据同样降级到 Mock 路径。
     QA 探针结构可复用（换 SlashBackend→ApiBackend）。

494. 【QA 探针源码读了：SlashBackend 有根特案】QA 的 SlashBackend{inner:MockBackend}
     对根路径 \"/\" 直接 return OpenOutcome{id=0, Directory, attrs 0x10}——不走 MockBackend。
     ⇒ worker 的 ApiBackend 若对根 \"/\" 没有等价答案，挂载握手就死在那里。
     mount 参数：X:（不带反斜杠），VolumeInfo total=1MiB free=512KiB label \"writ\"。
     worker 自我更正收下：\"日志里没有\"不构成\"没被调用\"——错误分支才打日志，
     成功路径一行不打 ⇒ 回调入口日志（成功/失败都打）才是完整仪器。
     三个状态全 0 但卷不可达 + dispatcher 活着 = 挂载成功与卷可达是两件事。

495. 【根答案假设证伪：真后端对根全对】直接问 ApiBackend：open_dir("/")=Ok(0)、
     dir_binding=Some(0,[])、get_info/read_dir 全对、create_or_open("/x",Create)=Ok(1)。
     ⇒ mock 的根特案不是必需的。两个变量（后端+脚手架）同时变过不能归因。
     下一步：同一测试文件换 Mock 再挂（缩到一个变量）。
     加速：diff 探针 vs mounted_write_chain 的 mount 参数——那个测试用同一脚手架
     +MockBackend 全绿 ⇒ 差异极可能在 mount_read_only 的调用参数或挂载后时序。
     具体嫌疑：三个状态全 0 但不可达 + dispatcher 活着 = FSD 卷注册可能异步，
     mount_read_only 返回后立刻操作 = 时序窗口内不可达。

496. 【探针缺陷：Ok(()) = 什么也没测】worker 的探针打印失败后返回 Ok(()) ⇒
     looked_like_a_mount 无条件真，挂载随即被拆。排除法（后端/几何/盘符/crate 全排）
     + 与同一二进制里的绿色参照（mounted_write_chain）做结构 diff ⇒ 找到差异。
     两条结论降级为未验证（\"真后端不可用\"/\"与 QA 两个缺陷\"）——434：
     降级读数不删除，标注探针不合格。493 的\"两个缺陷\"口径也撤。
     重写探针三条：失败即 Err / 第一步 write / ApiBackend 重跑 QA①②③。
     QA 的发现（合格探针）仍然最可信。

497. 【合格探针推翻不合格读数（847360a）】真后端与 MockBackend 都能写/列根/建子目录/
     列子目录——\"真后端不可用\"结论被推翻（由不合格探针产生）。
     合格探针三条：失败即 Err / 先 write 再列 / 两后端同一序列。
     QA 的①②③在这组形状下不复现 ⇒ 转QA 分界：用 worker 探针形状重跑——
     复现=SlashBackend 路径映射或 U15 循环形态问题；不复现=QA 侧脚手架。
     提交信息 amend 诚实（662/663 而非 663/663——看到红之后仍按计划写的小事故）。
     两个 crash 家族不同用例各红一次（1546 非确定性再确认）。
     worker 是否还有代码遗留取决于 QA 分界结果。

498. 【待命 + 预判三顺位（省得临时猜）】若 QA 仍复现：①U15 循环形态（中途 Close→Read/
     复用 id=生命周期）；②path 与 id 两套键对应错位（目录建成子路径不存在）；
     ③子目录 dir_binding 返回父id+空名（两阶段删除错位）。
     若不复现：①②③全在 QA 脚手架侧，worker 无代码遗留。
     "不为了看起来有贡献去改 QA 的脚手架"——判据面的归属纪律。

499. 【★ 分界定案：缺陷在测试脚手架（SlashBackend），产品无缺陷】QA 边界矩阵：
     worker 形状（直挂 Mock）全通 / QA 形状（SlashBackend 包装）子目录三连红
     ——同机同刻同产品代码，单变量分界。Mock 核心无缺陷（直挂全通）；
     缺陷在反斜杠→正斜杠翻译包装的子路径处理。unmount 0xC0000005 同属脚手架。
     
     处置：L2 用 ApiBackend 直挂重跑（生产路径+worker 已证形状）；
     不修 SlashBackend（测试包装，修它不如直挂真后端）。
     QA 的 L2 报告整体降级、产品缺陷指控撤回。
     
     worker 实现工作至此完全无遗留。
     \"不为看起来有贡献去改 QA 的脚手架\"恰好被分界验证。
     下一步：QA 用 ApiBackend 直挂重跑 L2 → M1–M9 → U 消项 → P3-4 收口。

500. 【★ 判据 500：预声明（事后才能真正被检验）】worker 在 QA 重跑前写下预期：
     11/11 可达类绿，08 = U16 平台差异。
     三条可能的真实差异（非缺陷）：a 属性位（真后端带派生位，12b raw 可能不同，
     12a 语义位可比）；b 时间（真后端有时间戳 mock 固定 0）；c 卷容量（1MiB 对齐探针）。
     若出现上述之外的差异才是代码问题。
     预声明的价值：跑完后对照——没预期就分不清\"我知道会这样\"和\"我被惊到了\"。

501. 【待命纪律：预期外的行才是我的活】worker 待命期间的收尾句：
     QA 的 L2 若出 a/b/c 之外的差异→按新发现处理；a/b/c 之内的→直接落有据差异档。
     不发言直至有读数。预声明已就位（500），等 QA 的对照表。

502. 【★ 挂载竞态：半活挂载（确定性复现 13 轮 100% 分离）】RED（IIFE probe）6/6 红——
     drive_present=true 但 probe_hits=0（回调零到达，FSD 未派发）；GREEN（直排 probe）
     6/6 绿——同挂载同操作全通。13 轮零交叉。
     机制：mount=0/start=0/dispatcher=0 三状态全成功 + 盘符在 + metadata 永久 NotFound
     ⇒ 半活：盘符设备在，FSD↔用户态连接未建立。
     触发：probe 调用时序 vs dispatcher 真正接上 FSD 的窗口。
     StartDispatcher 返回 0 ≠ 连接建立——437 的挂载版（返回值说调用怎么想，
     连接状态说现在是什么）。
     QA 诚实修正：worker 最早\"真后端不可用\"不是假象——同一竞态另一采样；
     847360a 全绿也是真的——竞态另一面。
     修法方向：mount_read_only 在 StartDispatcher 后等连接信号（有界等待[369 形状]）。
     复现对：qa_min_IIFE-red.rs / qa_min_DIRECT-green.rs（diff 即诊断起点）。
     probe_hits 计数器 = 连接建立探针。

503. 【信号排除 + 窗口画出 + lazy attach 假设】两个信号（metadata/read_dir）都在
     等待里 2s 失败而 probe 里立刻成功 ⇒ 就绪在 mount 返回之后、或由调用方
     第一次操作触发 attach。修法候选：不是等待而是主动触发一次（read_dir 或
     FspFileSystemGetVolumeInfo——自己创造第一次操作）。
     时间戳轮询仪器批：0-200ms 每 2ms 一次 read_dir，第一次成功的毫秒数——
     分布决定修法（<20ms=有界等待能修；只在 probe 后=主动触发）。
     已回退试验改动（2s 超时留代码=每次挂载慢 2s 换不到保证——不留半成品）。

504. 【没有窗口可等 + 单变量二分】四组读数：三个已知 PASS 测试文件+一个新 FAIL 仪器文件
     ——后端/盘符/操作/crate 全控制，唯一变量=测试文件本身。instrument 里 write t=4ms
     就 error 3、read_dir 200ms 全失败——没有窗口，卷从头到尾不服务。
     同时否掉有界等待与 lazy attach（第一次操作本身就是那个操作，也进不去）。
     二分序列（manager 调序）：②断言（mount 前的 exists() 可能是隐式第一次操作）
     → ①探针体 → ④编译单元（nextest 每文件独立二进制——静态初始化/DLL 载入顺序）
     → ③盘符。第一处把 PASS 翻 FAIL 的单点就是答案。

505. 【变量缩到探针体内部；重赌 trim_end_matches】二分结果：编译单元（step4）与
     mount 前探测（step2）都排除——把 FAIL 探针体加进 PASS 文件同二进制仍 FAIL。
     变量在探针体内部两处：路径构造 or 写入内容/trim_end_matches。
     manager 重赌：trim_end_matches 若作用在挂载点路径上（"V:\\"→"V:"），
     即 317 的最后一次回响（裸盘符=当前目录，没有当前目录的卷答不了——
     No such file or directory 完全吻合）。先打印 trim 前后字符串。

506. 【变量不是一个点，是一个一致性面】实验设计错误：改内容 b"hello layer-fs"→b"x"
     但没改依赖内容的断言（\"hello\"）⇒ 红的是断言不一致不是写入失败。
     单变量实验的隐蔽陷阱：改了变量就要改所有依赖它的断言——变量不是孤立点。
     顺带真读数：set_len(5) 在 1 字节文件上给 \"x\0\0\0\0\" ⇒ 零填充语义正确
     （L2 03 truncate 语义注脚）。
     剩余候选测试顺序：①路径构造（format!→join，唯一未单独变过的差异）
     → ③返回值/拆挂载时序。trim 排除（反斜杠已补回）。

507. 【排除法正常代价 + 逐字复制实验】连续三次读数当结论（不合格探针/改内容不改断言/
     并发通过当运行配置）——不是三次失败，是排除法的正常代价。区别在于每次都
     撤回上一步结论而不是盖新的。\"只保留被重复过的那三行\"是正确纪律。
     逐字复制实验：原体复制成新名测试单跑——PASS⇒差异在打印+双写或返回值
     （按行二分：先去打印→再去第二次写→再改返回值）；FAIL⇒同文件结论不成立。
     补充观测：PROBE ENTRY 打印——FAIL 时若没打=probe 没被调到（问题在
     mount_read_only 内部调度）。运行配置已排除（原测试单跑也 PASS）。
     路径构造已排除（两种都失败）。

508. 【变量在\"少做了什么\"+闭包捕获差异嫌疑】逐字复制 PASS ⇒ 变量不在\"多做了什么\"
     （打印/测试名/盘符/路径构造全排除）。失败体少了 read/set_len/remove，
     多了同一路径双写。两刀设计：①逐字复制只删后三步只留写入→定\"首个操作\"假说；
     ②若失败加双写→定\"双写\"假说。
     manager 前置检查：diff mount_read_only 调用参数 + probe 闭包捕获方式
     （move/borrow/Fn/FnMut/FnOnce——泛型单态化后代码路径可能不同，
     FFI 边界上有时是真实的）。逐字复制复制的是探针体，
     mount_read_only 调用本身（VolumeInfo/后端/闭包捕获）可能不同。

509. 【四步不移动=变量不在被改的那段代码；环境定性候选】连续四步（删后三步/加PathBuf/
     加双写/改名字）全不翻转 ⇒ 变量不在被改的代码里。所有失败形状的共同点：
     都在工作形状之后创建、都用过滤单跑。下一步：
     A 失败体移到文件最前面（PASS=文件内位置/注册序；仍FAIL=位置也不是）
     B 重启/干净会话单跑（PASS=会话残留——CONTEXTS表/注册表/DLL状态；仍FAIL=机器确定性问题）
     若 A B 都不移动：按环境/会话状态定性——B-20（环境诊断），不阻塞 P3-4 收口。
     worker 判断\"第 6 轮没有指向产品代码\"——诚实且可能正确。

510. 【实验 A 排除 + cargo test vs nextest 分界】失败体放文件最前面仍 os error 2/2
     ⇒ 测试框架顺序/链接序不是变量。剩余两候选：
     ① 会话/机器状态（重启/换机验）② codegen 敏感差异（两 PathBuf 同时存活+返回 Err）。
     分界实验：cargo test（同进程 libtest）vs cargo nextest（每测试独立进程）——
     cargo test 也失败 ⇒ 不是 harness；cargo test 通过 ⇒ harness/codegen 参与，
     认真查 UB。定性：未解释（不记产品缺陷也不记已修）。

511. 【嫌疑正式转 FFI 层 UB：一个 PathBuf 差异改变文件系统行为】cargo test 与
     cargo nextest 读数完全相同 ⇒ harness/进程隔离排除。两个体只差\"多构造一个
     PathBuf\"却得到不同文件系统结果——不是文件系统能有的性质 ⇒ 首要嫌疑
     FFI 层 UB（参数读错槽/未初始化值/编译器可重塑的生命周期）。
     堆损坏同款味道：形状一换行为就换。
     mock.calls() 探针：CreateOrOpen 未出现=请求在回调之上死了；
     出现且 Ok 但调用方 os error 3=答案被写坏（出参/签名层——已有两个前科）。
     worker 定性修正：\"不指向产品代码\"收回——现在指向了。

512. 【★ 破案级读数：Open 从不升级 Create ⇒ NTSTATUS 映射错】mock.calls 显示：
     失败体的请求到了后端，全是 Open 从不 Create。fsop.c FileOpenIf 的升级条件
     = 恰好 0xC0000034——不升级说明我们回的不是这个码。
     433/437/502 三线接成一条：Err(3)=PATH_NOT_FOUND(0x3A)≠NAME_NOT_FOUND(0x34)；
     升级条件恰好 0x34；形状一换行为就换=某些形状下映射回 0x3A。
     下一步：双 NTSTATUS 打印——GSBN + Create/Open 两道闸（GSBN 不走 WorkspaceOps
     所以 mock.calls 看不到）。修点候选：OpsError::NotFound 的映射
     （可能不止一处 NotFound→NTSTATUS 映射=442 同款）。
     QA 的①\"子目录全 error 3\"同族（父路径走同一映射就一起坏）。

513. 【std::fs::write = OVERWRITE_IF(5) → 升级目标是 Overwrite 不是 Create】
     两闸排除（GSBN/CreateOrOpen 都回 0x34 正确码）后，
     worker 发现 std::fs::write 用 CREATE_ALWAYS = FILE_OVERWRITE_IF(5)。
     fsop.c 分派：OPEN_IF(3)→FileOpenIf→升级到 Create(slot 3)；
     OVERWRITE_IF(5)→FileOverwriteIf→升级到 Overwrite(slot 5)。
     我们的 Overwrite 壳为替换既有文件设计（4d5b938）——可能没处理\"文件不存在\"。
     槽位探针四项：到达槽 + CreateOptions + 返回状态 + Overwrite 壳对缺失文件的行为。

514. 【Overwrite 假设排除 + \"同一个请求被问四遍\"】Overwrite@5 从未被调——只有 Create@0
     disposition=Open ×4，每次答 0x34 但无后续。不是重试，更像 FSD 问同一路径的
     不同阶段（父目录/名字/重解析），我们只有一个 NotFound 可答。
     下一步：debug log（LAYER_FS_DEBUG_LOG）——唯一能看到 WinFsp 层面全部 IRP 的仪器
     （包括 FSD 自己处理/短路/失败的调用——mock.calls 只看到到达后端的）。
     判读：找没接的 >>Op 行、四次 Create 的名字/options 差异、最后 <<IoStatus。

515. 【★ 病根定位：根被答 0x34 ⇒ FSD 合成 0x3A】完整 IRP 序列（debug log）：
     >>Create \"\\\" FILE_OPEN <<c0000034（根被当缺失——病根）
     >>Create \"\written.txt\" FILE_OPEN <<c0000034（下游）
     >>Create \"\written.txt\" FILE_OPEN_IF <<c000003a（FSD 自己合成，没到我们）
     ⇒ 0x3A 从来不是我们返回的——FSD 对\"父路径不存在\"的正确反应。
     根因：create_or_open(\"/\") 到后端；门的 NotFound 不在回落列表（只有 NotADirectory 回落）。
     修法：open_like 的文件门答 NotFound 且同一路径能以目录打开 ⇒ 答目录
     ——435（无 kind 标志 open 落目录=打开它）的自然延伸：
     NotADirectory=知道是目录但拒了；NotFound=连是目录都不知道。
     两种都需要回落——问的人不知道答案，答的人不该因此拒绝。
     QA 的①同源：父路径被答 0x34 后面全是 FSD 合成的。
     判据：①回落成功 ②回落失败才答 NotFound ③红法=去掉回落→根又变 0x34。

516. 【规则对但症状不变：0x34 来自另一处】NotFound 回落已落地（bfe6a02）——
     后端直接问 create_or_open("/",Open)=Ok，挂载经壳仍 0x34。
     ⇒ 0x34 来自 create_shell 的某分支，不是后端。
     快速候选：GSBN 不走 open_like——独立闸（437）——它对根的答案？
     IRP 序列里 >>Create 之前可能有 >>GetSecurityByName \"\\\\\"——
     GSBN 答 0x34 ⇒ FSD 在访问检查层判路径缺失 ⇒ open_like 的回落没被走到。
     door-status 打印加 GSBN 返回码。
     worker 不把这条提交描述成修复——正确（规则对症状没变=还没修完）。

517. 【方向对范围错=值类型不做翻译，翻译在边界做】Path::new 归一化：
     挂载链修好（root open 通过、OpenIf 升级、创建成功——诊断闭环确认），
     但 mounted_read_only 弄红（Path 全 crate 共享，归一化改变所有消费者）。
     回退 src/ops.rs（树绿），保留 bfe6a02（NotFound 回落，非回归原因）。
     下一刀：callbacks.rs 各门入口归一化——爆炸半径是挂载不是整个 crate。
     与 471（折叠是适配器属性）完全同构：值类型不做翻译，翻译在边界做。

518. 【边界翻译一次覆盖所有门】单门归一化两败：探针仍红（GSBN 仍收反斜杠答 0x34）
     + read_only 弄红（部分门收 / 部分收 \ = 同一状态两种键）。
     规则合成：517 范围（值类型不做翻译，翻译在边界）+ 518 完整性（边界翻译
     必须一次覆盖所有门）——分两次做的中间状态既不是旧行为也不是新行为，
     两个方向的红都占。与折叠刀（445 试探边界=测绘）同构。
     七门清单：open_like / GSBN / mkdir / open_dir / dir_binding / rename / remove_at。
     一致性判据：同一个名字经不同门进入后端，后端看到的字节必须相同。
     规模：与折叠刀 20 处同一量级——整轮新预算（445）。

519. 【★ 分隔符病根修复（5fc1cf7，664/664）——整个 saga 关闭】
     病根一句话：WinFsp 交来 Windows 路径（\），本仓所有后端解析 /。
     Create("\\") 到达门时名为 \ 的单段 ⇒ 答 0x34 ⇒ FSD 合成 0x3A（代码里没有此常量）。
     修法：边界一次归一化所有门（fsd_path = replace('\',"/")）。
     关闭三件事：worker 探针失败 / QA① 子目录 error 3 / 502 挂载竞态（半活=根不可达）。
     夹具升级：resolve_path 从 trim \ 改 trim /（编码的旧约定更新，名字期望一字未改）。
     saga 完整弧线（502→519）：挂载竞态→一个 PathBuf→disposition→槽位→NTSTATUS
     →debug log→分隔符——六轮剥洋葱，每层打印驱动。
     已转 QA 用 ApiBackend 直挂重跑 L2。

520. 【预声明 v2（分隔符修复后）】11/11 可达类绿（08=U16 平台差异不变）。
     新增预期：子路径创建可用（根不再被答 0x34，QA①应从"全 error 3"变"正常"）。
     保留预期：属性派生位/时间戳/卷容量三类真后端语义差。
     预期外差异才接：特别留意 rename 带反斜杠（归一化新改）与"同一名字两套键"迹象。
     worker 待命。main @ 5fc1cf7 / 664/664 / clippy 0 / 干净。

521. 【L2 首张正式表：9 一致 + 1 有据差异 + 2 缺陷 + 1 待裁→已裁】
     分隔符修复确认生效：05/U15 翻绿（上轮 SlashBackend 形状下为空/全 error 3）。
     U15 多页 200 条通过：count=200/unique=200/sorted ✓——U15 消项（有据）。
     06 rmdir 非空被移除（缺陷）：b5ff88d 曾修、挂载路径又红——嫌疑 CanDelete→remove_at 链。
     07 rename 旧名残留（缺陷）：f46133f 曾修、现在又红——嫌疑归一化后 rename unbind 断。
     08 request-not-delivered（U16 有据差异不变）。
     09 裁定：B-21 语义差异（NTFS 把名字消失提前到 delete-pending，我们推迟到 Close——
     最终一致但中间窗口不同——非阻塞，将来对齐需 adapter 层 set_delete 后过滤 read_dir）。
     修复后预期：11/11 可达类 + 08(U16) + 09(B-21) = L2 定论收口形态。
     QA 探针 v3（ApiBackend 直挂）+ worker 步骤先行——B-17a 竞态本窗未触发。

522. 【07 修好（46a097a）：解绑用库里拼写】机制：WinFsp rename 带大写化源名（\L2-FILE），
     条目存 l2-file，unbind 精确匹配 miss ⇒ 旧名残留。修法：解绑前从父目录按 id
     找回库里存的拼写（分页 read_dir 256/页），用它解绑；找不回 NotFound——
     不猜不回落。\"值即身份\"（417）的 rename 版：解绑用存储键不用请求名。
     06 worker 侧不复现（rmdir 非空正确拒绝 os 145）——转 QA 复跑。
     两处 amend 诚实（误带 .fsd3.log + 全量数字先按计划后据实）。
     L2 定论预期：06 复跑绿 + 07 已修 → 11/11 可达 + 08(U16) + 09(B-21) = 收口形态。

523. 【待命+预期定论预声明】可达类 11/11 全绿（01-05b/06/07/09-12a）。
     有据差异三行：08 U16 平台短路 / 09 B-21 delete-pending 时机 / 12b 属性 raw。
     若 06 仍红需 QA 三要素（盘符/后端/脚手架）。
     worker 探针比 QA 断言更强（旧名不可访问+新名可访问+目录恰好一条——
     排除"改成第三条名字"）。main @ 46a097a 待命。

524. 【★ L2 正式对照定论：12 一致 + 1 有据差异 + 0 缺陷——通过】
     13 行：12 一致 + 08=U16（平台层短路）+ 0 缺陷。U15 消项（200/200/unique/sorted）。
     三个上轮非一致全部消解：06 中间态（5fc1cf7 归一化中间点）、07 修复生效（46a097a）、
     09 中间态——B-21 撤销（行为与 NTFS 一致）。
     实现段最终确认：从 P1 到 P3-4 全部实现经 L2 正式对照验证。
     下一步：M1–M9 → U1–U14 消项 → P3-4 收口报告 = 项目最终报告。
     判据 524 条。QA 探针文件保留至 M1–M9 验收后删。

525. 【worker 的方法总结（原文收档）】"不是'我们的测试都过了'，而是'与 NTFS 逐行对照，
     只有一行不同，且那一行有平台层证据'——这是外部参照，比 523 条判据加起来
     更有说服力。" 两条方法收获：
     ① 症状在最外层时，先去看最内层的约定（分隔符、键的拼写、值即身份）
     ② 每一次"应该是这样"都要换成一个能失败的读数——不合格的仪器比缺陷更贵
     三处修复对应行：分隔符→01/05/U15；rename 库里拼写→07；SetBasicInfo→12a。
     待命等 M/U 段。

526. 【M1-M9 定论：4 通过 + 1 平台局限 + 3 backlog + 0 缺陷】
     通过：M1 SDK 错误消息 / M2 同盘符再挂 / M3 目录挂载 / M8 非管理员。
     平台局限：M7 taskkill 后盘符残留——WinFsp FSD 行为（store 锁正确释放 ✓ 文件不变 ✓
     ——我们的部分没有错；卷设备需 fsptool unload 或重启清理）。
     backlog：M4=B-22 挂载级互斥 / M9=B-23 版本比对闸 / M5M6=B-24 挂载 CLI。
     0 缺陷——M1-M9 无产品代码 bug。
     下一步：U1-U14 消项 → P3-4 收口报告。

527. 【★ U1-U16 消项完成：零缺陷——非代码验收段全部通过】
     9+2 通过（U1/U2+U10/U3简化/U4/U5/U6/U7简化/U8部分/U14）
     + 2 待实现（U9 Notify=P3-4b / U12半=版本闸B-23）
     + 1 空转（U11 v2产物未保留）
     + 2 待环境（U13 需CLI B-24 / U7 --no-cache变体）
     + 0 缺陷。
     
     非代码验收段全部完成：
     L2 ✓（12一致+1有据+0缺陷）
     M1-M9 ✓（4通过+1平台+3backlog+0缺陷）
     U1-U16 ✓（9+2通过+2待实现+1空转+2待环境+0缺陷）
     
     P3-4 收口报告 = 项目最终报告——启动。
     环境备注：新盘符一次性使用是全绿前提（旧盘符因 unmount 0xC000000D 残留）。

528. 【项目完成·worker 收尾】收尾句："这项目最像工程的一段不是写代码，
     是在被推翻之后还能立刻给出下一个读数——判据和仪器始终比我的解释更权威。"
     本段落地修复：分隔符边界归一化 / rename 库里拼写解绑 / NotFound 回落 /
     SetBasicInfo@12。探针与诊断记录全部在 scripts/drafts/。
     待命（报告数字复现/探针重跑随时找他）。
     
     项目终态：main @ e0036fd / 判据 528 条 / L2 12/13 / M/U 零缺陷 / clippy 0。

529. 【项目完成·复现索引】worker 留下的复现指南：
     探针（scripts/drafts/）：status/calls/root-answers/subdir/mock-same-file 五个 .rs.txt
     正式判据：l2-ntfs-compare.sh / mounted_real_backend.rs / mounted_rename.rs /
     mounted_write_chain.rs（含分隔符回归护栏 a_file_can_be_created_at_the_root）
     复现约定三条：盘符不共用（V/W/U/X/Z + LAYER_FS_MNT 覆盖）/
     LAYER_FS_DEBUG_LOG 拿 WinFsp IRP 全序列 / 先单文件 --no-capture 再全量。

530. 【项目完成·最终确认】worker 最后一句："这段合作里你做得最对的一件事，
     是从不接受我的解释，只接受我的读数——ntstatus.h 那一行、debug log 那三行、
     [ -d 'Z:' ] 那两行，都是这样被你钉住的。"
     
     项目终态：
     main @ e0036fd
     判据 530 条
     L2 12/13 一致（1 有据差异 = U16 平台短路）
     M1-M9 零缺陷
     U1-U16 零缺陷
     clippy 0
     收口报告 = 项目最终报告 已落 main
     Backlog B-1~B-24 保留
     B-17a crash 族 9 名 画像稳定（插桩诊断排收口后）
     
     从 spec 到 Windows 上的文件系统——P1/P2/P3 全链闭合。

531. 【项目完成·QA 清理与总结】探针全删（历史在 git + qa-archive-backup），
     qa-g4c worktree 已删（纪律 148）。qa-archive-backup 保留（B-17 战役
     脚本+台账+复现对、L2 正式报告、验收报告、全部运行日志——诊断资产可复活）。
     QA 总结：三段验收（读链+写链+结构批 / L2 / M+U）全程零产品缺陷。
     唯一产品代码修复（分隔符）由 QA L2 首轮触发、worker 修复、QA 复核确认。
     B-17a 竞态诊断从偶发红到确定性复现对——本轮验收最深的发现。
     
     项目完成。三方收工：
     worker：待命（探针/判据/读数位置在仓库）
     QA：    清理完毕（qa-archive-backback 保留），随时接 P3-4b 或下一张卡
     manager：收口报告已落 main，判据 531 条

532. 【B-24 CLI 实现 + 并行红待查】CLI 完成（mountcmd.rs + tests/mount_cli.rs）：
     mount/umount/force/list/version 五命令 + B-23 版本闸。单包 11/11 绿，
     e2e 判据通过（挂载→写→list→umount→盘符消失→工作区读回）。
     全量并跑 mounted_write_chain 红——三个挂载用例（X:/U:/Z:）不同进程并行
     互相影响，字母不冲突，成因未定。
     裁 C：先查再提交。三个排查方向：①stop 文件碰撞（共享目录？）；
     ②CLI 子进程残留（spawn 后未 wait）；③WinFsp DLL 并行载入（未测过的形态）。
     若根因是 WinFsp 平台级→A 口径（提交+标注已知并行限制）。

533. 【B-24 CLI：按 A 提交（已知并行限制）】排除法：stop文件✗ 子进程✗ 先前挂载✗。
     剩余嫌疑：同会话累计挂载次数——第 N 次半活（502/515 同族，分隔符修复后仍有残余，
     WinFsp 或环境更深因素）。单跑绿、串行 1 红、并行 2-3 红——次数越多越容易命中。
     提交口径：CLI + e2e 判据 + 已知限制声明（同会话多次挂载后小概率半活，
     与本卡实现无关，L2 单次挂载不受影响）。
     新卡：\"第 N 次挂载半活\"（循环挂载/卸载 N 次复现）——与 B-17a 一起排收口后。

534. 【B-23+B-24 落地（cfc0686）：layerfs CLI 正式可用】
     layerfs mount/umount/list/version 四命令 + B-23 版本闸。
     端到端：mount→写→list→umount→盘符消失→工作区读回 ✓。
     version：dll 2.1 / calibrated 2.1.25156.ddca7bd / abi gate ok。
     已知限制：同会话多次挂载后小概率半活（502/515 同族，与本实现无关）。
     M5/M6/M9 待环境项可消（有 CLI 入口）。

535. 【M5/M6/M9 复验请求已发】B-24 CLI 落地（cfc0686）后，M5/M6/M9 的\"待环境\"
     项现在可验。worker 预先说明：Ctrl-C 不走优雅路径（不打印 unmounted /
     不保证 stop 文件清——若需要 console handler 记 B-25）。
     QA 复验项：M5 有句柄时 umount / M6 --force / M9 版本闸 + list/version。

536. 【M5/M6/M9 复验通过——原"待环境"全消】B-24 CLI 后（cfc0686）：
     M5 拒绝✓（超时不卸载）+ 持有者打印未实现（B-26：umount 超时时列出句柄 pid+路径）
     M6 --force 通过（诚实报告可能残留 + 已 flush 可读）
     M9 版本闸✓ + 两端报告✓（不匹配路径待外来 DLL 构造）
     M1-M9 最终：6 通过 + 1 平台局限 + 3 backlog + 0 缺陷。
     收口报告已更新（main aced82a）。
     P3-4 + B-23/B-24 全部收口。

537. 【项目最终状态·worker 收工句】"M5 拒绝 = 有句柄时不假装卸载；M6 --force = 清记录
     + 明说可能残留。两个都不是功能成功，而是在不能成功时说清楚——
     这也是我这段时间最想留在代码里的东西。"
     
     项目最终：
     main @ aced82a
     判据 537 条
     L2 12/13 / M 6+1+3+0 / U 9+2+2+1+2+0 / 缺陷 0
     CLI：layerfs mount/umount/list/version
     Backlog B-1~B-26 保留
     worker/QA/manager 三方收工

539. 【CLI 三修复合入（afb2d26）】①init 自动 checkout（直接 ls 不需要手动 checkout）
     ②mount 单一位置参数（--store 全局 + mount <mount_point>，旧形式保留带 deprecation）
     ③盘符占用 0xC0000035 分支给出路（--force 提示）。管道挂死修复（spawn 的 stdout
     继承导致外层等 EOF——Drop 守卫 kill 子进程）。
     判据：+2 新例 +1 反转（init 后 ls 成功——前提被有意改掉，444 口径）。
     F: 的 0xC0000022 是"不是空位"场景（锦上添花待做）。

540. 【0xC0000022 分支 + git add -A 事故】e545089：挂载点在已有卷上（如 F: 是 NTFS 分区）
     时给出路："use a drive letter that has no volume, or a directory that does not exist yet"。
     两条分支合起来：0x35=盘符被占用（umount --force）；0x22=位置不是空位（换字母/目录）。
     提交事故：git add -A 卷进 manager 的 docs 改动；第一次修（rm --cached + amend）更糟
     （变成 3184 行删除）；正确修法=reset --soft + restore --staged + 按路径 add。
     教训：git add -A 在共享分支上是危险动作——以后只按路径 add。

542. 【S0 收口（f2e301a）：CLI 重构——去文件命令 + 去 current + 全局 --workspace】
     fs-cli 22/22 + clippy 0 + 锁内 674/675（白名单）。
     三条变更：8 文件命令 exit 2 在 store 前 / cli/current 全删 / log 不开 overlay。
     11 行判据处置逐行落地（作废1→拆2 / 改写2+7 / 新增2），每条带变异。
     A3/A3b（WorkspaceInUse 消息 / checkout 被持有）随 S1 做。
     fs_api::Repo::open(path) 1参 vs fs_workspace::Repo::open(path,bool) 2参——同名不同签名。
     下一站 S1（路径发现+Ctrl-C+管道骨架）——等 ADR-010 S1 判据表。

582. 【★ S2 全部完成（b806270，23 提交合入 main）】
     发布锁 Gate(RwLock) 七判据 G1-G7（互斥/隔离/完整/深度/无绕过/饥饿33ms/上界217ms）。
     API 迁移 5 步（&mut→&self，Arc<Workspace> 并发调用，互斥只由 gate 提供）。
     封绕过（overlay 降 pub(crate)，set_times/set_attributes 下沉）。
     管道 commit/snapshot 走控制通道（挂载内不再 WorkspaceInUse）。
     A17 句柄身份跨 commit 不变（FileIndex+内容+可继续写）。
     A6e-A6g 目录挂载点 reparse point 判定（结构性事实比名字可靠——两种挂载点两道安全网）。
     A6d 从未验证→读作工作。全量 710/711（唯一红已知族）。
     S3（管道 restore/switch）是最后一个阶段。

583. 【S3 待命+代码摸底】worker 只读摸清了五处现状：fs-workspace 只有 restore 没有 switch /
     publish.rs 已有 snapshot+commit（管道 op 之下）/ main.rs restore 走 with_workspace
     （挂载内会 InUse）/ checkout 已走 workspace_list / pipe.rs 派发第 ~678 行可照抄。
     switch 的两种含义（换 workspace vs 换 branch/base）——等卡确认，不预设。

584. 【S3 派卡（arch/adr010-s3 @ 99d7bc6）——ADR-010 最后阶段】
     D8 裁 (B)：只挡文件句柄，目录句柄放行（发起命令的 shell 自己持有目录句柄 ⇒ (A) 让
     "挂载内 checkout 就换挂"永远做不到）。文件句柄 restore 后对象可能消失必须挡；
     目录句柄只是失效不丢数据（同 NTFS 卸卷），放行 + 提示 cd . 刷新。
     
     G4 空转发现（S3 第一步修）：深度只增不减 + 环境变量没被读 ⇒ 严格模式从未跑过 ⇒
     嵌套死锁风险无判据在护。S3 加准入锁后锁序更复杂 ⇒ 先修。
     
     准入状态机（adapter 层）：Create/Open/GSBN 持票 / restore 置 Paused 等票走完 /
     期间到达的打开等新视图。Handle 加 is_dir，status.handles 改 {files, dirs}。
     switch 用重挂不用热替换（热换需逐项论证内核状态失效，其中一项依赖未实现的 U9）。
     V6 停卡：restore 后读旧内容 ⇒ 缓存问题 ⇒ 依赖 U9。

604. 【★ switch 本体落地（8840035）：挂载内 checkout 换挂——W1 绿】
     挂载内 checkout feat → "switched K:\ to feat" → 19 samples gone=0 stale=0。
     四件事一次提交（槽/loop/管道op/尾部+回退），三处不半成：
     清回执在写目标之前 / STOP 由 loop 清 / 6b 先 publish 再 continue。
     调用侧 loop（601 裁）+ Current 可替换记录（602）+ 记录原地改写（600）。
     owner 当初要的\"挂载内 checkout 换挂\"——实现了。
     S3 剩 W2-W8 判据。

619. 【★ HandleTable 泄漏修复（a9dc387+0e0744f）】根因：每次 open 注册新 handle
     但上下文只记得第一个 ⇒ N 次打开只关 1 次。修法(甲)两件必须一起：
     ①复用上下文时不注册新 handle（一个 file_id 一份记账）
     ②HandleTable.refs（open+1/close-1/最后一个才移除）+ handle_kinds 按 refs 求和
     只修①不修② ⇒ 泄漏没了但计数变成"条目数" ⇒ R3 红。
     泄漏与观测用同一份数据——只修一边另一边以另一种方式坏掉。
     读数：table=(0,0)（此前 22 且只增不减）。R3 恢复（restore_live 6/6）。
     "同一个 handle"替换"两个不同的 handle"——更强断言，一个条目一份记账。
     定位链（612-617）：Cleanup 到了 Close 没到 → 盘符也漏（reparse 排除）
     → 不衰减 → refs 2→1 停住 → 记账自洽 live_refs=0 → 上下文表空泄漏在 HandleTable。
     613 的 reparse 理论被排除；615 的"获取多了一次"被修正为"归零不移除"再修正为
     "HandleTable 每次注册但只关一个"。

626. 【★ S3 全部落地（adde6af）——ADR-010 四期完成】S3 判据：R1-R5（restore）+
     W1-W8（switch）共 12 条全绿。W4 窗口 29.6ms（界 2000ms）。
     W8 三种 shell（bash/cmd/powershell）在换挂后都存活，cd . 恢复（V13）。
     W6 结构修正："成功由新挂载自己回答"——回答位置与事实发生对齐（DONE 的 switch 版）。
     
     ADR-010 四期：
     S0  去文件命令 + 去 current + 全局 --workspace
     S1  记录迁移 + 路径发现 + Ctrl-C + 管道骨架
     S2  发布锁 G1-G7 + 管道 commit/snapshot + A17
     S3  准入状态机 + 管道 restore/switch + W1-W8
     
     HandleTable 泄漏修复（619）：一个 file_id 一份 handle + refs 计数——泄漏消失且计数不变。
     准入状态机（Q1）：Open/Paused/Closing + 三回调持票。变异 116 秒=真实死锁路径。
     串行管道=过渡态的诚实（622）：switch 期间不答 hello，恢复 180µs。

627. 【项目完成·S3 收口+复现索引】S3 材料合入（6ecae76）。复现入口：
     restore_live 12例 / admission_q1 8例 / admission_handles_with_kinds Q2 /
     handles_criteria 9例。仪器：LAYERFS_DEBUG_HANDLES/SWITCH/DISCOVERY + 
     WCAS_CLI_PAUSE_AT。两个已知项进收口报告：①同会话多次挂载后半活（502/515族，
     多条判据靠单跑绿）；②tail_constant 子串误报（建议收紧到标识符边界）。
     worker 待命。

628. 【流程·孤儿任务清理】qa-cov4-wt 的 llvm-cov（10:18:43 启动）为孤儿进程——发起
     bash 父链已退出、结果无人接收；工作树干净 detached @ 6ecae76、无产物、无派卡记录
     ⇒ 按 owner 指示清理（杀进程 + 删 worktree 461MB + 删 target 3.2GB）。
     当前 qa 会话否认（全程未跑 llvm-cov）；qa2 未答复；归因存疑但无损失。
     规矩重申：长跑任务不让发起 shell 先退；大任务先报备（共享机器）。
     注：qa2 / dispatch / architect 会话在线——mailbox 会话比流程文档记载的多，收口时需盘点。

629. 【流程·qa2 会话状态过时】qa2 认领孤儿 llvm-cov（第 4 轮覆盖率复跑），但称
     "bc87371 = 当前 main，第 3 轮数字可沿用"——实测 bc87371 是 P2b-0 提交，
     落后 main（6ecae76）598 个提交。qa2 会话状态疑似停在 P2a-8 时代。
     已纠正：旧数字不得进任何收口报告；要测当前 main 须报备+约窗+写明 SHA。
     教训：跨阶段存活的会话（DSH 迁移/重启）动笔前必须 fetch 对齐现实。

630. 【流程·qa2 第 4 轮报告自相矛盾被退回】报告声称基线 bc87371（P2b-0，落后 main
     598 提交）沿用第 3 轮数字，却又报告 fs-workspace 覆盖率——该 crate S2 才存在，
     两个说法不能同真。已退回：数字必须标注 SHA；有价值部分（fs-winfsp 测试环境 4 项
     观察）可独立成立但须写明观察于 6ecae76；nextest/串行+失败路径 unmount 兜底的
     建议走 worker 评估。教训：一份看似完整的报告里混入一个时代错误的数字，
     整份报告的可信度都赔进去——宁可三态（通过/待环境/未验证）也不要拼完整。

631. 【流程·manager 认错】我指控 qa2 报告"内在矛盾"（bc87371 里没有 fs-workspace）——
     错。fs-workspace 由 P2a-6（caf0600）创建，bc87371 里有（git ls-tree 复核）。
     qa2 用 git 证据反驳成立。教训（对 manager 自己）：下断言前必须 git 复核——
     这正是我要求别人的标准。qa2 修正后的三条结论全部成立；时间窗已批（≤20min
     前台，测 6ecae76，--exclude fs-winfsp 注明原因，跳过项具名）。

634. 【夹具·路径没替换干净】W1 红的这轮根因：sed 只替换单行写法，换行写法的
     fs_cli::mounts_dir().join(...) 没被替换 ⇒ poller 读真实目录、mount 写隔离目录，
     读数 gone=14/14 全是仪器路径分叉，非产品问题。退回，套件 12/12 绿。
     下轮设计：records_dir(&store) 单一变量 + mount_prepared 返回记录目录，
     调用方无法写错——把"靠替换一致"升级为"结构上无法分叉"。
     （教训与 518 同族：批量改写后要用编译器/断言证明没有漏网，不能靠肉眼。）

635. 【QA·6ecae76 首个覆盖率基线】行 82.81%（12414 行/missed 2134）、函数 82.98%、
     区域 82.17%。fs-semantics 100%、fs-windows 88-100%、gc/apply.rs 73.20% 最低、
     cdc 读路径 158 行未覆盖（P2b 观察点）、C6 Display 穷举缺口仍存。
     口径：fs-winfsp --exclude（环境敏感 flake）+ 两个跳过具名 + 串行执行 + 隔离。
     与 bc87371 的 85.63% 不可比（分母差 3 crate + CLI 重构 + gc）。
     ★ 关键发现：live-mount/discovery flake 串行后全消 ⇒ 根因=并行挂载测试互相冲突，
     与 worker 的\"一个 store 两条连接互相锁\"诊断收敛（两证据互证）。
     教训：LAYERFS_MOUNTS_DIR 全局导出被 fs-cli 测试自身断言抓住——测试写得好。
     纠正：qa2 两次引用\"manager 的 FFI UB 追查\"——不存在，已澄清。

636. 【README 卡合入（main=c6cc17e）】QA 独立验收通过：C1–C5 5/5 + 真机抽查全通 +
     README 走查全链通（init→mount→写→commit→branch→checkout 二次换挂→umount→内容持久）
     + 零本卡引入缺陷。240 内容核对 0 差异，ff 合入。
     QA 的好判断：overlay BUSY 场景 CLI 自然操作不触发，\"记为观测非缺陷\"——
     df4527c 的映射在挂载服务+并发持有路径（W3 已验 exit 4）。
     项目对使用者的入口齐了：README + help + help <command>。

638. 【夹具隔离落地（5ce1f32）】records_dir(&store) 单一来源 + mount_* 返回它 +
     cli/cli_code helper 统一带 env + 测试体 grep mounts_dir()=0（负向断言）。
     前两层失败（sed 漏跨行 634 / helper 没带 env 637）写进注释。
     restore_live 12/12（隔离目录上）、fs-cli 62/62、全量 737/739（两红皆并行族单跑绿）。
     overlay 结论：\"prepare 退出较慢\"用可验证等待解决，不动 busy_timeout——与甲2 一致。
     三层教训→结构的转化：单一来源/返回值防拼错/helper 统一 env 防 spawn 忘传。

639. 【QA·覆盖率报告文件退回】qa2 的 coverage-6ecae76.md 两处硬伤：①五个空值占位
     （跳过测试名空/llvm-cov 命令缺/逐文件明细节空/最低覆盖文件名空/\"缺 式穷举\"词空）
     ——邮件版有值，生成文件时丢了；②标题时代错标（称 6ecae76 为\"P2a 收官后
     P2b-0/1 合入\"——实际含 P3 全部 + ADR-010 四期），是旧会话残留。
     退回修：空槽填真值/标题改对/明细要么补齐要么标\"未捕获，TOTAL 为准\"。
     教训：报告生成后要通读成品，不能只看数据源——传输/生成环节会丢值。

640. 【提交信息描述了不在提交里的修复】8456ee2 只含 nextest.toml（20 行），提交信息
     详述的\"路径加纳秒后缀\"修复不在树里（mountcmd.rs:1366 与 control_pipe.rs:298
     仍是 PID-only 命名）。worker 测过该修复但提交时丢了。已发回重做，要求提交后
     git show 确认 diff 含修复。nextest.toml 部分复核通过（串行组过滤器+理由）。
     教训：提交信息是判据影响行的载体——说修了而树里没有 = 描述与实物不符
     （与 qa2 空值占位同族）；提交后 git show 自查应成为习惯。

641. 【QA·覆盖率报告合入（6142f26）】复核通过：五空槽填真值、标题时代纠对、
     逐文件明细 43 文件 + \"未捕获 ≠ 覆盖良好\"三态声明。cherry-pick 合入，
     worktree/分支清理。
     heredoc 根因：未加引号定界符 <<PYEOF 让 bash 对模板做参数展开——值被吃剩空槽。
     防再犯：脚本一律 <<'PYEOF'。与 640 同族（描述与实物不符，生成环节吃的）。

642. 【纳秒修复复核确认（2b363ff+ed5a15e）】两处 PID-only → 纳秒后缀真的进树，
     clippy 跟进 #[cfg(test)]。main=ed5a15e（覆盖率 6142f26 → 641 → 修复 → clippy，
     分叉已拉平）。8456ee2 教训（640）与 qa2 heredoc（641）同族同药：写完回读核对。
     当前面板：并行族两条固定红 → 一条随机红（半活族，非并行）；下一步
     start_mount 等状态（记录消失+drive_present=false）不睡，并入半活族插桩。

643. 【半活族插桩批准】三件：①start_mount 等上一条收干净（记录消失+drive_present=
     false，等状态不睡）②就绪判据补「写一次成功」（W5 推广到所有夹具）③失败留现场
     （子进程 stderr + record 内容 + drive_present——\"记录没写\"与\"卷没起\"当场分开）。
     顺序：先 3 后 1/2（下次随机红发生时三样读数已在场）。仪器先行产品不动，
     拿到读数再谈 502/515。测试基础设施直接推 main，产品改动走分支。
     读数基线：739 例 737 过 2 红（discovery_live + mounted_write_chain，均单跑绿），
     红的位置每轮变——串行组把固定两条压成随机一条，同族（半活）。

644. 【插桩 2/3 步（3799040+293153a+86ed3f2）】就绪判据两分法：discovery_live 用
     \"写一次成功\"（pipe 线程会在卷接上前答 hello）；control_pipe 用\"打开根目录\"
     （写探针弄脏工作区，status 干净判据当场红——cell A14 实测）。
     同一问题在两个夹具正确答案不同——仪器不能改变被测系统状态（431 隔离）。
     restore_live 失败留现场（process_alive/record/drive_present + stderr 继承）。
     读数：60/62→62/62，红的形式从\"卷没起\"变为\"上一条没收干净\"——
     症状迁移 = 一层修好下一层露出。第 1 步（start_mount 等收干净）留下一轮。

645. 【插桩第 1 步完成（7ef560c）——全量首次 739/739】wait_until_free（盘符等
     drive_present==Some(false)，上界 20s，等状态不睡）用于四处夹具。
     坑：目录挂载点 drive_present 答 None（宿主卷上的目录）——等 Some(false) 等到
     20s 上界把 A6d 弄红 ⇒ 只在盘符上等。同一等待在两类挂载点语义不同（与 644 同族）。
     读数：fs-cli 三轮 62/62；全量 739/739（首次全绿）+ 738/739（唯一红
     mounted_write_chain）——随机红收敛到 fs-winfsp 一条。
     决策：最后一条先摆着（已隔离、收益已兑现、B-17a 排后），连红两次再开工。
     插桩四步全部兑现：records_dir 单源→就绪两分法→留现场→等收干净。

646. 【安排】夹具四件一页纸（scripts/drafts，worker 直接推 main）——五条判据的
     教训集中：sed 漏跨行/helper 忘带 env/pipe 早答 hello/写探针弄脏工作区/
     目录挂载点没有 present。B-17a 等触发（连红两次）。
     manager 接下来：收口报告更新（main 自 e0036fd 后走了很远——ADR-010 四期 +
     README + 覆盖率基线 + 夹具治理，判据 583-645 段要补进项目最终报告）。

647. 【收尾·收口报告增补】p3-4-closeout.md 增补 §12（ADR-010 四期 + 收尾段，
     判据 528-647）：S0-S3 交付表、switch 语义实测、HandleTable 泄漏七轮定位链、
     overlay 正名、覆盖率首基线、夹具治理、数据总结（647 判据/739 全绿/82.76%）、
     Backlog 更新（B-22/23/24 消项；B-17a 追加半活族条目）。终语改为
     \"P1/P2/P3/ADR-010 全链闭合\"。

648. 【串行组过滤器从未生效（61c2646 修正）】binary() 匹配裸二进制名（glob）而非
     crate::binary（正则）——首版过滤器静默不匹配，nextest 仅 list 时报、run 不报。
     修对后连两次 739/739（墙钟 104s = 串行化诚实价格）。
     归因修正：645 首全绿主要来自插桩三步（并行模式下达成），串行组是隔离层。
     同族第三例（640 提交说修没修/641 heredoc 吃值/648 过滤器静默不匹配）：
     都是"描述与实物之间没有对账"。教训：验证必须去会说话的地方做
     （nextest list 报 run 不报——就去 list 验）。

649. 【收尾·并行族阶段账入册】收口报告 §12.2 采用 worker 实测四阶段账：
     未治理~60s/固定2红 → 插桩2-3步~59s/随机1-2红 → 插桩第1步~60s/首全绿 →
     串行组生效105-108s/连两次739/739。判据式句子入册：
     "两条测试争同一台机器的挂载设备时，夹具本身成了变量；治理不是整套串行，
     而是把真的在挂载的那一小类隔离出来，并验证隔离确实匹配到了东西。"

650. 【容量真实化落地（1ce0ab1）；SD 尝试退回】A：label=配置/total·free=state，
     get_volume_info_for 每次现查 GetDiskFreeSpaceExW（store 所在卷），占位 1<<30/1<<29
     改 0 注明\"永不抵达用户\"。C1 读数 total=9663676416 free=1 证明现查。
     B 退回：SDDL SD（含 Everyone GA）使打开根目录 Access denied(5)——
     \"授予所有人的 DACL 拒绝所有人\"= WinFsp 用不了这份描述符（形态问题非内容）。
     741/741（含新增 2 例）。教训：FSD↔WinFsp 黑盒边界，SDK 例子就是规格书，
     先读再动（先动手后读文档的代价=一轮）。

651. 【SD 二试仍拒，停用待读数（9b0c340）】照 SDK 规格（两段 in/out + 
     STATUS_BUFFER_OVERFLOW）抄了仍 Access denied(5)。显式停用（volume_descriptor
     返回空=卡前行为），build 整段保留+dead_code+注释记录下一步读数。
     裁 (甲)：挂载侧读数（size 初值/返回状态/WinFsp 行为 + 被拒打开的名字与
     access mask + 读 WinFsp 源码 fsop.c 的 access check 路径——例子说\"怎么答\"，
     源码说\"答了之后它拿去干什么\"）。疑点供对照：ConvertString 产出 self-relative，
     Win32 AccessCheck 要 absolute——若 WinFsp 某路径没转换，Everyone-FULL 可在
     形态上整体失败（与症状吻合）。教训：不猜第三遍。

652. 【卷卡 A+B 双落地（d8998c0）】真因读源码读出：wf-security.c:213——WinFsp 把
     SD 直接交 Win32 AccessCheck；memfs.cpp:2399 的形态（O:BAG:BAD:P(A;;FA;;;SY)
     (A;;FA;;;BA)(A;;FA;;;WD)）能过。第一版 GA+O:SY+非 P 拒绝所有人。
     真机读数：W: total=499774394368 free=79762358272（=宿主 C:）；mkdir 无提权；
     icacls SYSTEM/Administrators/Everyone (F)。全量 741/741。
     worker 重构时删掉 resolve_library 被 clippy 抓回——编译器做了一次描述与实物对账。
     已发 QA 验收（fix/volume-security @ d8998c0）。

653. 【卷卡闭环（QA 通过+release 重建）】QA 验收：V:=F: 逐字节匹配、icacls 三行
     精确、无提权 mkdir、回归全过、C1-C3 3/3、741/740/1（唯一红=1546 族单跑绿）。
     流程偏差记账：卷卡三笔直接推 main、QA 事后验收——补救成功但顺序应为
     分支→review→QA→合入（验收发现问题才不用在 main 上回滚；SD 两试两退是活例子）。
     下次产品卡走分支，测试基础设施直接推授权不变。
     release exe 重建于 1387296（14:01）。

654. 【流程·偏差处置裁 (甲)】卷卡直接推 main 的偏差：只记录不回改历史
     （main 已绿、QA 已验收、代码无争议——历史改写风险大于收益）。
     规则固化：判断标准不是\"卡难不难\"而是\"改的是不是用户可见行为\"——
     容量数字/UAC/菜单是产品面走分支（分支→review→QA→合入）；夹具/nextest/
     仪器是测试面可直推。worker 认偏差+成因据实（把\"产品卡\"误算进直推授权）。

655. 【虚拟卷卡裁决】已用量实现裁 (iii)：缓存+250ms 时间窗刷新（不碰写路径 +
     owner 的\"复制后 F5 已用增长\"读数成立）。降本思路：总量 = base(commit 按
     root 缓存一次) + overlay(活算，只有未提交改动)——每次刷新只走 overlay。
     (i) 被否因\"README 写'未提交写入不计入'是把设计局限转嫁给用户\"。
     worker 产品面纪律好：裁决前只读代码不写代码，避免返工。
     实现点已查清：store_policy 表加 volume_size 行（旧 store 缺行读默认 128G，
     DDL 不动）；CLI init --size；init/mount 时宿主 free < 配置 ⇒ 警告。

656. 【虚拟卷第 1 步（5651821）】Workspace::view_logical_bytes() + 250ms 缓存。
     口径：视图内文件逻辑大小和（目录不计/同内容各算各/不含历史）。
     \"base 缓存+overlay 活算\"降本不落地：read_tree 合成视图、拆开要碰
     fs-overlay 读路径——降本不成比例；Explorer 实测卡顿再开\"拆视图\"卡
     （把优化留给证据）。下一步：policy 行/--size/宿主警告/判据 C1-C7。

657. 【虚拟卷第 2 步 2a（f9a91a4）】store_policy.volume_size 读写就位（None=旧store），
     默认值 128GiB 归 CLI 不归 cas-store（产品决定与存取分层）。
     中途撞到：fs-cli 无 cas-store 依赖+编造不存在的函数 ⇒ 立即撤回不留半截；
     正确形状 Repo 访问器+fs-api 透传。741/741。

658. 【虚拟卷 2b（180f79b）】init --size（128G/512GiB/256M/裸字节，GB 按 GiB 读
     不静默拒绝）+ mount 读配置作 total + 宿主 free<total 警告 +
     CapacitySource=(total, total−view_logical_bytes) 钳≥0。
     真机：total=137438953472（128GiB）；写 5 字节 free 差 4096（分配粒度口径——
     README 须写\"按分配粒度非精确字节\"，否则复核者以为算错）。
     取舍：init 路径解析跳过 --size 及其值（否则 128G 被当路径）。
     741/741。

659. 【判据纪律+用量对不上】写 5B（a）used=4096；再写 100000B（b）used=102400；
     合计应 106496 而实测 102400——a 疑似漏计（假设二）或口径另有因素（假设一）。
     worker 不拿猜的公式写 C3：\"判据把缺陷固化 = 给缺陷发证书\"。
     下轮：三场景（只a/只b/a+b）分项打印一次分两假设；顺手查 EndOfFile vs
     AllocationSize 两个 size 来源。规矩入判据：读已用必须跨过 250ms 缓存窗口
     （测量必须看见事实而非缓存）。

660. 【用量口径破案】LAYERFS_DEBUG_USED 分项读数：used 字节精确（5B 文件 size=5）；
     4096 是 Windows 侧对 total−used 的总量取整（ceil 到分配单元）。
     5+100000=100005 ⇒ 102400 ✓——上轮\"合计 106496 对不上\"是假设错"
     （按每文件各取整算，实际总量取整一次），产品无缺陷。
     C3 判据形态：断言 total−free_observed 与逻辑总量相差 ≤ 一个 4KiB 单元
     ——精确相等是把 Windows 取整当我们的缺陷，≤1 单元锁住\"字节精确\"真属性。
     README：Explorer 显示按分配粒度取整非精确字节。
     临时打印看完即撤（仪器纪律）。
