# P2b-2：ADR-008 落地核对（实现 vs 文档）

- 日期：2026-10-24
- 核对对象：`docs/adr/ADR-008-pack-gc.md`（main `3490c30` 及之前的 `5d493d2`）、`ADR-002 §7.0`
- 分支：`p2b/pack`，基于 main `5024a11`
- 结论：**无阻塞偏差**。三条需要文档确认的点（§3 表尾、`pack_max_bytes` 校验语义、pack id 生成）如下；
  其余为等价实现或"本卡不在范围"。

## 1. 逐条核对

| ADR-008 | 条款 | 实现 | 证据 | 判定 |
| --- | --- | --- | --- | --- |
| §3 | pack 文件 `packs/<pack_id>.pack`；帧 = ADR-001 规范帧首尾相接 | `layout.packs()` / `pack::encode` | `tests/pack.rs` 往返；`contract` 无 loose 文件 | 一致 |
| §3 | 尾部 `magic "WPK1" \| u32 count \| u64 frames_len \| 32B BLAKE3` | `pack::encode/verify`，**48 B** | `tests/pack.rs::verify_names_a_pack_whose_tail_no_longer_hashes` | 一致（判据曾写 44 B，已裁决为 **48**，属判据笔误） |
| §3 | 单对象读不读尾部，按 `(offset,length)` 读一帧 | `pack::read_frame`：`seek` + `read_exact(length)` | `tests/pack.rs::reading_one_object_reads_exactly_one_frame`（读字节数 == 帧长） | 一致 |
| §3 | 单 pack ≤ 64 MiB（`store_policy.pack_max_bytes`），超出按顺序切多个 | `Batch::commit` → `split(pending, limit)`，`limit` 取自策略 | `tests/pack.rs::pack_max_bytes_splits…`（2/3/5 → k=1/2/3） | 一致 |
| §4 | `object_location(oid,role,length,kind,location,offset)` + `pack(pack_id,bytes,objects,created_ns)`，`STRICT` | 逐字一致 | `schema.rs::DDL`；`tests/schema_v3.rs` 校验旧库列集 | 一致 |
| §4 | `store_policy.schema_version = 3` | `SCHEMA_VERSION = 3` | 同上 | 一致 |
| §4 | 任何 ≠3 的 Store → `UnsupportedSchemaVersion`，不迁移 | `schema::validate` 先查 `user_version` | `tests/schema_v3.rs`：{0,1,2,4} 全拒、两次都拒、字节哈希不变、tmp 未清 | 一致 |
| §4（注） | 被拒时不得改动文件；`configure()` 在 `validate()` 之前是已知缺口 | `open.rs`：**validate 先于 configure** | A/B：还原 `open.rs` → 版本 0 一例即红 | 一致（已修） |
| §5 | 去重：内存 + 索引（`AlreadyPresent` 不写） | `Batch::put`（HashSet）+ `commit`（`published()` 查询） | `tests/pack.rs::a_repeated_object…`（第二批 0 pack） | 一致 |
| §5 | 1–4：tmp → fsync → rename → fsync 目录 → **一个短 `BEGIN IMMEDIATE`**，按 pack 顺序重复 | `pack::write` + `Batch::record_pack` | `tests/sync_probe.rs`（per pack 模板、V1–V7、`sync=2k`） | 一致 |
| §5 | fsync 全在事务外（KI-7）；记录最长单次锁持有 | 事务只含 INSERT | 探针：`m ≈ 3.01 µs/object ≤ S/10`（33× 余量）；`H(1000) ≈ 4.40 ms` | 一致 |
| §5 | `Store::put` = 单对象 batch | `Store::put_observed` → `batch` 一个对象 | `tests/store.rs::every_put_lands_in_a_pack…`（32 put → 32 pack） | 一致 |
| §5.1 | 四个崩溃点 | pack 五步（Staged/StagedSynced/Renamed/DirectorySynced/Committed）逐点注入 | `tests/crash.rs`（5 例，重开 + verify） | 一致 |
| §7 | verify：单对象按 kind 读并校验身份 | `object::get` 分派 loose/pack，`verify_id` + role 校验 | `tests/store.rs`（损坏/截断/消失三例） | 一致 |
| §7 | pack：尾部 BLAKE3；索引 `(offset,length)` 在范围内且**恰好一帧** | `object::verify_pack`：尾校验 + 帧连续性 + 覆盖 `frames_len` + `count == 行数` | `tests/pack.rs`（尾部损坏）；`tests/store.rs`（截尾/截体） | 一致（实现更强：还要求帧连续且无空洞） |
| §6 | GC（mark/sweep/重打包/独占） | **未实现** | — | 不在本卡（P2b-6） |
| §6.5 | `snapshot drop` / `branch delete` / `gc` CLI | **未实现** | — | 不在本卡（P2b-6） |

## 2. 需要文档确认的三点

1. **`pack_max_bytes` 的校验语义（建议补一句 ADR）**：它是**策略值，不是格式常量**——测试与工具可以创建
   更小的 pack 上限（本卡两侧用例用 2400 B）。因此 `validate()` 只校验"存在、可解析为 u64、非 0"，
   **不**与构建默认值相等比较（`max_object_len` 是格式常量，仍要求相等）。建议 ADR-008 §4 末尾加：
   > `pack_max_bytes` 是运行策略而非格式常量：`validate` 只要求它是可解析且非 0 的十进制整数；新建 Store 写入构建默认 64 MiB。

2. **pack id 的生成（差异，需记入 ADR）**：ADR §3 说"随机 128 bit hex（名字不承载内容身份）"。本实现用
   `BLAKE3(pid ‖ nanos ‖ 进程内计数器)[..16]` 的 hex——**唯一性**足够（同 Store 内不撞名），但**不是
   密码学随机**，也不保证跨机器不可预测。若 ADR 要求的是"随机"而非"唯一"，需改一句为"128 bit 名字，
   只需在单个 Store 内唯一/不承载内容身份"；若要求不可预测，请说明用途，我再改实现。

3. **`Batch`/`BatchReport` 的对外形状**（未在 ADR 中）：`Store::batch() -> Batch`、`Batch::{put,len,is_empty,commit,commit_observed}`、
   `BatchReport { new_objects, already_present, packs }`。物化器把它当 `PageSink` 用（ADR §5 的伪代码即此）。
   属"实现细化"，核对表只作记录。

## 3. 实现层面新增的两个内部机制（ADR 已覆盖，附实现说明）

- **staging 有界重试**（ADR-002 §7.0 正式条文，main `3490c30`）：条件三条——每次新临时名、从零重写；
  只包 staging 一段（不含事务/读路径）；只重试 `NotFound`/`PermissionDenied`，耗尽报**原始错误**。
  触发场景：并发 `open` 清扫 `tmp/` 会删掉活进程 staging 的 pack。P2b-6 改为"常规 open 不清扫"。
- **探针 attempt 作用域**（ADR-008 §5.1 验收细则）：`sync-probe` feature 下每次 staging 尝试写一条
  `Attempt(n)`；失败尝试必须零 `TxnBegin`，只有成功尝试参与 V1–V7 与 `2k` 计数。

## 4. 未做（明确不在本卡）

- GC 全部（§6、§9 测试 5/6/7）、`snapshot drop`/`branch delete`（§6.5）→ P2b-6。
- loose → pack 迁移（ADR §8：不需要）。
- 同长 `truncate` 的"尾部已零则不写回"守卫 → P2b-2 的性能检查项（manager 决定不在本卡顺手改）。
