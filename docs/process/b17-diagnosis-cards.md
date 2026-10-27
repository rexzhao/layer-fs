# B-17a：1546/crash 族诊断卡（不稳定红 · 已留名）

> 状态：立卡（2026-10-27）。owner 裁决"先窗口后专项"已执行——窗口已留名，本卡为专项。
> 不阻塞 P3-4；可在 P3-4 收口后执行。

## 签名（唯一判别键）
```
SqliteFailure { code: SystemIoFailure, extended_code: 1546, Some("disk I/O error") }
1546 = SQLITE_IOERR_TRUNCATE = (SQLITE_IOERR | 6<<8)   // sqlite.h.in:511 源码自证
```
注：早期误判为 DELETE_NOENT（=5898）已更正（判据 372）。

## 家族（2 crate / 9 测试名 / 全部同签名；第 9 名 crash_before_the_index_commit 为写链批读数，症状面扩至 index commit 前的 staged 路径）
| crate | 测试名 | 行 |
|---|---|---|
| fs-workspace::crash | every_commit_crash_point_recovers… | :187 |
| fs-workspace::crash | a_crash_after_the_publication_completes_it_on_reopen | :187 |
| fs-workspace::crash | a_crash_during_materialisation_publishes_nothing | — |
| fs-workspace::crash | a_crash_during_discard_is_swept_on_reopen | :328 |
| cas-store::crash | crash_before_the_index_commit… | — |
| cas-store::crash | crash_while_the_object_is_staged | :177 |
| cas-store::crash | a_crash_between_two_puts_keeps_the_first | — |
| cas-store::crash | completing_the_put_after_a_crash_publishes_exactly_one_copy | — |
| cas-store::crash | crash_after_the_staged_file_is_synced_leaves_it_unpublished | — |

**共同点（诊断入口）**：全部在 crash 模拟后的 **reopen** 路径上，DB 主文件 **truncate 被挡**。

## 触发画像（已确立的事实）
- 单跑必绿（7/7、5/5 多次）；仅全量并行下出现 ⇒ **高并行下概率事件**；
- 环境重叠**非必要条件**（worker 锁内自发 1 红）——重叠放大 概率；
- fresh-build × 总负载 两因素**尚未分离**（QA 战役 1–5 安静绿、6–8 重叠红；第 9 轮 load-marker=3 抓到）。

## 机制假设（候选，未定案）
journal 重置 / WAL checkpoint 截断 DB 主文件时**另一句柄仍开着**：
1. crash 子进程刚被 kill、句柄未及释放的窗口（最可疑——只在 crash 测试、单跑必绿均吻合）；
2. 杀软扫描新文件；
3. 并行下另一连接。

## 诊断步骤（按序）
1. **观测一**：crash.rs:187/:187 失败现场记录 dir 文件清单（-wal/-shm/journal 是否在、谁持有）；
2. **观测二**：reopen 前是否有第二个活连接（连接计数探针）；
3. 若 1/2 不足：Windows 句柄追踪（Process Explorer / handle.exe）在重放红轮上抓现场；
4. 复现工具：QA 重放脚本 `qa-archive-backup/b17-replay.sh`（touch 新构建 + nextest --no-fail-fast，
   包 `scripts/fullrun-lock.sh run --`，每轮 load-marker）；日志 b17-replay-8/9.log。

## 修复方向（定案后）
按根因分：句柄释放窗口 ⇒ kill 后等待/重试目录级；杀软 ⇒ 排除目录；并发连接 ⇒ 序列化 checkpoint。

---

# B-17b：KI-11 族诊断卡（pack 缺失 / os error 2）

> 状态：立卡（2026-10-27）。与 B-17a **分族**（文件不同、形状同：意外不存在）。
> 战役 8+9 轮 0 次出现 ⇒ 触发条件不含"新构建+全量并行"，需 pack 并发负载。

## 签名
```
MissingPack（store.rs:740）／ VerificationReport pack_failures / os error 2（文件不存在）
```

## 现场台账（4 处）
| # | 时期 | 测试 | 构建 |
|---|---|---|---|
| 1 | P2b-6 | 无名 | debug |
| 2 | P3-2 rebase | 无名 | debug |
| 3 | concurrent_puts_of_one_object_store_it_once（store.rs:740） | 带名（JSON 事件流抓捕） | debug |
| 4 | concurrent_opens_wait_then_publish_correctly（pack_concurrency.rs:237） | 带名（ki11_1.log 补获） | release |

## 诊断步骤
1. 重放变体：`--run-ignored` 单独跑 pack_concurrency 两条（唯一带名现场），单独标记、锁下排队；
2. 与 B-17a 的区分判据：错误码/缺失对象（pack 文件 vs DB 主文件）——**归族按签名不按形状**；
3. 证据归档：qa-archive-backup/logs/（legacy-20261024-26/ + p33b_full_KI11-third-occurrence.log）。

## 关联
- 判据 332（证据落 /tmp=没落盘）、366/372（1546 定性与更正）、373（文件锁）；
- owner 裁决记录：先窗口后专项（2026-10-27）。
