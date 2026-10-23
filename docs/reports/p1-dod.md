# 第一阶段完成定义（DoD）对照

- 日期：2026-10-23
- 卡：T5（P1 收官）
- 依据：`workspace_cas_fs_spec.md` 文末的完成定义

> **第一阶段完成定义（DoD）：** 在不安装文件系统驱动的机器上，SDK 和 CLI 能持久管理真实大小的虚拟文件树，完成随机范围读写、全局相同对象去重、原子 Root 发布与崩溃恢复测试，并有可复现的性能报告。只有达到此条件才进入第二阶段。

本清单把这句话拆成可检验的条目，每条给证据指针。**结论：P1 达成**，附两条不满足的边界说明（§7）。

---

## 1. 拆解

| # | 要求 | 判定 | 证据 |
| --- | --- | --- | --- |
| D1 | 不需要安装文件系统驱动 | ✅ | P1 是库 + CLI，无 WinFsp、无内核组件；`crates/` 下只有 `cas-core`/`cas-store`/`fs-namespace`/`fs-api`/`fs-cli`（P3 才引入 `fs-winfsp`） |
| D2 | SDK 可用 | ✅ | `fs-api` 的 `Fs`：15 个公共方法，`cargo doc -p fs-api` 无警告 |
| D3 | CLI 可用 | ✅ | `fs-cli` 10 条命令；`crates/fs-cli/tests/cli.rs` 17 个集成测试 |
| D4 | 持久管理 | ✅ | 重启一致性：`s2_many_small_files_survive_a_restart`；对象持久化：`cas-store/tests/crash.rs` 11 个崩溃测试（含真实进程终止） |
| D5 | 真实大小的虚拟文件树 | ✅ | 1 GiB 单文件（S3）；100,000 文件（S2，`WCAS_ACCEPT_FILES=100000`） |
| D6 | 随机范围读写 | ✅ | `read_at`/`write_at` 部分读写矩阵；读放大 ≤2 对象/4 KiB 读（`large_file_read_is_bounded` 等 4 个测试） |
| D7 | 全局相同对象去重 | ✅ | `s1_identical_content_at_100_paths_shares_one_object`（100 路径 → 1 对象，100 个独立 FileId）；基线报告 §5（100× 去重比） |
| D8 | 原子 Root 发布 | ✅ | `each_mutating_operation_appends_exactly_one_root`、`published_root_records_never_change`、`s4_published_roots_are_readable_and_never_change` |
| D9 | 崩溃恢复测试 | ✅ | `cas-store/tests/crash.rs`（11 个，含 `PublishStep` 序列与真实进程 kill）；`fs-namespace/tests/atomicity.rs`（6 个失败注入，含 X1–X4） |
| D10 | 可复现的性能报告 | ✅ | `docs/reports/p1-baseline.md`：环境、命令、原始 run、重复次数、方差、测量规则 |

---

## 2. D1 无需文件系统驱动

- 依赖图（`docs/architecture.md` §1）：`fs-cli` → `fs-api` → `fs-namespace` → `cas-store` → `cas-core`。
- `fs-cli` 只依赖 `fs-api`（T4-QA 后的架构修正），`fs-api` 只依赖 `fs-namespace`/`cas-core`。
- 平台 API 只出现在 `crates/cas-store/src/durability.rs`（Windows 目录刷新的 `FlushFileBuffers`），`git grep` 全树仅此一处。
- 无 `fs-winfsp` crate；P3 才需要。

## 3. D2/D3 SDK 与 CLI

| 层 | 公共接口 | 测试 |
| --- | --- | --- |
| `CasStore` | `Store::open/put/get/exists/verify/object_count/identify/opened/layout/is_read_only/metadata_path` | 36 + 11 |
| `Namespace` | `open/head_root/root_record/root_records/root_count/mkdir/rmdir/rename/unlink/stat/read_dir/create_file/content_state/replace_content/replace_extents/referenced_objects` | 29 + 41 |
| `Fs` | `open/create/read_at/write_at/truncate/flush/stat/read_dir/mkdir/remove/rename/verify/stats/read_stats/file_id` | 语义 16 + 大文件 7 + 形状 6 + 洞 6 + 验收 4 + 覆盖补 10 |
| `fs-cli` | `init/put/get/ls/stat/mkdir/rm/mv/verify/stats` | 17 |

CLI 与 SDK **同核**：CLI 的每条命令都经 `Fs`，不含业务逻辑（`crates/fs-cli/src/main.rs` 只有参数解析与输出）。

## 4. D5 真实大小

| 规模 | 测试 | 通过条件 |
| --- | --- | --- |
| 1 GiB 单文件 | `s3_gi_b_file_random_reads_are_bounded_and_correct` | 顺序写（P50 146.7s）+ 200 次随机 4 KiB 读，每次读放大 ≤2 对象，逐字节与写入规则一致 |
| 100,000 文件 | `s2_many_small_files_survive_a_restart`（`WCAS_ACCEPT_FILES=100000`） | 创建 + 随机读 + 重命名 + 删除 + **重启后逐项核对** |

## 5. D7 去重

100 个路径写同一内容 → Store 中**恰好 1 个对象**，同时 100 个 FileId 互不相同、各自元数据独立（改一个不影响其他 99 个）。基线报告另给 100× 去重比与 `stored_bytes`。

## 6. D8/D9 原子发布与崩溃恢复

**原子发布**（D+ 方案，owner 已确认）：
- 每个变更型操作恰好追加一行 root 记录（`each_mutating_operation_appends_exactly_one_root`）；
- 已发布记录逐字节不变（`published_root_records_never_change` + S4）；
- 链无断裂（`root_chain_is_connected`）；
- 操作失败整体回滚（`failure_before_the_mutation_leaves_everything_unchanged` 等 6 个）。

**崩溃恢复**：
- `cas-store`：暂存 → fsync → rename → 目录同步 → 索引提交；11 个测试含真实进程终止与重启后校验；
- `fs-namespace`：`BEGIN IMMEDIATE` 单事务；失败注入覆盖变更前/后/发布阶段三种故障。

## 7. 不满足项与边界（如实标注）

| 项 | 状态 | 说明 |
| --- | --- | --- |
| 历史 Root 的完整树视图 | **不满足（按设计）** | D+ 只保证 root 记录 append-only 与发布原子性；**旧 root 的树内容不可重放**（ADR-003 §4.1）。spec P2.8 的 Snapshot 负责此事。这不影响 D8"原子 Root 发布"——发布是原子的，只是不提供历史树读取 |
| 绩效基线未含原生 FS 对照 | **不满足（按范围）** | 属 P3；T5 卡明确不做 |
| 磁盘型号未记录 | 部分 | 报告记了容量与剩余空间，WMI 未取到型号 |
| 无稀疏文件 | 不适用 | 属功能范围外，已在 ADR-004 §10 列出 |

**为什么"历史树不可读"不影响 DoD 判定**：DoD 要求"原子 Root 发布"，不要求"历史 Root 可读"。后者是 spec P2.8 Snapshot 的明文范围，P1 不提供不构成缺口。

## 8. 结论

**P1 达成**：D1–D10 全部满足，两条边界（历史树视图、原生 FS 对照）均属后续阶段的明文范围，不是 P1 缺口。

按 spec 文末："只有达到此条件才进入第二阶段"——**条件已达到**，可进入 P2。
