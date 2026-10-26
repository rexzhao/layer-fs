# P3-2 G1 合并报告（原子 disposition）

- 合并范围：`p3-2/gaps` 14 个提交（`6fd1118` … `1e95b0e`），ff 合入 main（`1b09f19`）＋ 文档批次（`3bf5d4d`、`4cad914`）。
- 前置：`5d7c798`（P3-2 卡 v5 + 文档批次）。
- 全量：**445 passed / 0 failed / 8 ignored**，`clippy --workspace --all-targets` **0**。
- 验收：QA 独立复跑（A 机械 / B 语义 / §G 取证分档），Manager review 两轮；所有红法各自实测。

## 1. 交付了什么

| 项 | 提交 | 说明 |
| --- | --- | --- |
| `PACK_TAIL_BYTES` 单一来源 | `6fd1118` | 公开 `pub const`；测试里的本地 `48` 全删；**不导出** magic |
| 五条判据变成会跑的检查 | `ee00d42`…`4c4fd43` | R1 / M2 / M1 / 白名单 F1–F5 / 三级分辨率 |
| 18 格表上移 | `8f71814` | 落 **`fs-semantics`**（零依赖 crate），`fs-windows` 只 re-export |
| `Rules::create_or_open` | `4d02d35` | 判定 + 动作在**同一个 `write` 闭包**（一个 SQLite 事务）；复用 `decide` |
| `TreeWrite::truncate` + 真截断 | `d9d7794` | `TruncateExisting` 在事务内清零；`SUPERSEDE` 只解绑旧名字、**旧 inode 行保留** |
| 18 格三列表 | `4d02d35` | 每格断言 action / kind / FileId；`SUPERSEDE → 新`、`OVERWRITE* → 保留` 各自断言 |
| 原子性结构判据 + 锁探针 | `d3898d8`、`7338e44` | 一事务 / 决定在内 / 拒绝不分配；`HOLD` 与 `MIN_WAIT` **两个独立常量** |
| 端到端截断 | `1e95b0e` | 走公开读路径：`size == 0` **且旧字节读不到**；`SUPERSEDE` 后旧 inode 行仍在 |
| 偶发 `failed=1` | `6bb61f1`、`1e95b0e` | 定位为**判据缺陷**（两个时钟起点不同），改单边下界；删除两条经验性上界 |

## 2. 三个关键裁决（实现与文档已对齐）

1. **表落 `fs-semantics` 而不是 `fs-namespace`**——`fs-namespace → cas-store → rusqlite(bundled) → cc`，
   会让 `cargo test -p fs-windows` 需要 C 工具链、README 三条声明全失守。实测依赖个数 **0 → 1**。
2. **`CreateOutcome { file_id, kind, action }`**——FileId 与 action 并列（不藏变体），`kind` 复用 `fs_namespace::Kind`；
   `outcome` 由 `action` + `Err` 无损重建。
3. **SUPERSEDE 后旧 FileId 仍可读**——只解绑名字、**旧 inode 记录保留**（不可达，下次离线 GC 收走）；
   GC 根集**不需要**活句柄输入，因为离线独占（overlay 排他锁）保证 GC 期无句柄（ADR-008 §6.1、ADR-009 §4.2/:239）。

## 3. 三次“红得对/红得不算数”的教训（已进 `docs/process/criteria-and-acceptance.md`）

- **恒绿判据比没有更糟**：并发负对照“必须出现两个成功”不可达（`bind` 自带前置检查）⇒ 改成结构判据 + 锁探针。
- **循环计数器假装基数断言**：`cells == 18` 只证明循环跑过；表多一行/重复一行不红 ⇒ 补 `TABLE.len()`/`rows`/`columns`。
- **靠“某段开销有界”才成立的边界都不是不变量**：`±100` / `policy + 1000` / `>= measured` 三版上界同源失败
  （16 MB payload 实测红）⇒ 只留“超时 ⇒ 等待 ≥ 超时”，上界留给 **B-12**。

## 4. 未验证 / 待环境（按三态口径）

| 项 | 状态 | 依据 |
| --- | --- | --- |
| 并发原子性 | **未运行期观测** | `Mutex` 覆盖整个写闭包 ⇒ 无可观测窗口；用结构判据 + 锁探针取代，报告不写“通过” |
| SQLite 出错回滚 | **未由 CI 证明** | 只由人工变异（`write` 出错也提交 ⇒ 负对照 +2、生产仍 +1）证明度量有效 |
| `OVERWRITE` 具判别力 | **未验证（依赖 B-12）** | `main_busy_timeout_ms` 是编译期常量、schema 等值校验 ⇒ 测试改不动它，旧 bug 的 5000 恰好小于真实值 |
| 一次全量 `failed=1` | **已定位（判据缺陷）→ 已修** | 见 §1 末行；`volume.rs` 候选经 48 并发进程压测**已排除**（改名属卫生，不是修复） |
| Linux CI | 待环境 | 本机只能证“无平台门控” |

## 5. 已知缺口（进 backlog）

- **B-12** `waited_ms` 的钟起点（CAS 路径含调用前工作）⇒ 修后 `waited_ms <= measured` 成为真不变量；
- **B-13** `doc_refs` 检查落成 `#[test]`（引用 ⇒ 小节存在；两波基线均为 0）；
- **B-8** SUPERSEDE 后旧句柄的读（跨 GC，P3-3 必做）；
- **B-9** 只读挂载期间 GC 必须**拒绝**；
- **B-10** overlay 不可达 inode 行的回收（方案 C：离线 GC 在 overlay 排他锁下扫）；
- **B-11** ACL 强制。

## 6. 判据工程产出（本卡的主要剩余价值）

`docs/process/criteria-and-acceptance.md`：**15 节 75 条**，其中本卡新增的包括——
取证四档、`[read]` 推翻条件、读法污染、三态、证据必须是某个 ref、按形状查不按名字查、
对照必须绕过被测函数、缺口四步、计数不是目标、白名单上限即信号、时间型判据只写下界。
