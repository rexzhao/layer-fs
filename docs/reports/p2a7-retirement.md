# P2a-7b：P1 直写路径退役报告

对象：`fs-api` 的 `Fs` 面、`fs-namespace` 的 SQLite 表后端、`fs-cli` 的命令集。
依据：ADR-006 §14（P1 组件退役清单）、§13（对外 API/CLI）。本报告逐项核对 §14，并给出
卡里要求的"每个被删测试文件一行：删除原因 + 语义是否在新测试中重现"。

---

## 一、ADR-006 §14 逐项核对

| §14 的退役项 | 状态 | 说明 |
| --- | --- | --- |
| fs-api 直写路径（`Fs`） | **已删** | `Fs` 及其 18 个方法、`file_system.rs`、`FsError`/`ReadStats`/`Stats`/`VerifyReport`（旧定义）、`BLOCK_BYTES` 全部移除 |
| fs-namespace 的 SQLite 表后端 | **已删** | `sql_tree.rs`、`sql_tree_store.rs`、`db.rs`、`schema.rs` 移除 |
| fs-namespace 的 `content.rs` | **已删** | `ContentState`/`Extent`/`replace_extents` 等表后端内容方法 |
| `namespace.rs`（`Namespace<SqlTree>`） | **已删** | 连 `RootRecord` 一起移除；`Entry`/`Stat`/`Kind`/`FileId` 迁到新的 `model.rs` |
| fs-namespace 的 DDL（v2）与 `namespace_policy` | **已删** | crate 不再打开数据库、不再持 DDL；版本号单点由 `workspace_policy.schema_version = 3` 持有 |
| D+ root 链与其访问器（`head_root`/`root_record`/`root_records`） | **已删** | `root` 表随表后端消失；历史改由快照根表示（`fs-snapshot`） |
| 对应测试迁移为 trait 契约测试或删除，逐个说明 | **已做** | 见第二节 |
| `file_id_allocator` 由 `scope_allocator` 接管 | **已做** | 表随表后端删除；serial 分配在 `fs-workspace` 的 `scope_allocator`（P2a-6） |
| P1 库拒绝、无迁移 | **已做** | `MainDb::prepare` 检测到 `namespace_policy` 即返回 `SchemaMismatch{found: "schema 1/2 (namespace_policy); rebuild the store"}`，**即使 `create = true` 也拒绝**（否则一个库会同时描述两套文件系统）；测试 `a_previous_schema_is_named_and_refused` |

### fs-namespace 现在的形状

只剩规则与契约：`Path`、`FileId`、`Kind`、`Entry`、`Stat`、`InodeView`、`TreeRead`/`TreeWrite`/
`TreeStore`/`OpLabel`、`Rules`、`resolve_in`/`resolve_parent_in`、`NamespaceError`。
`cas-store` 依赖保留（`NamespaceError::Store`），`rusqlite` 依赖与其 `From` 实现**已移除**
——这个 crate 不再知道 SQLite 存在。

---

## 二、删除的测试文件清单

基线 `0ac6698`；"重现"一列说明该文件测的语义是否在新测试里有等价断言。

### fs-api（15 个文件，104 个测试）

| 文件 | 测试数 | 删除原因 | 语义是否在新测试重现 |
| --- | --- | --- | --- |
| `semantics.rs` | 16 | 驱动 `Fs` 直写 | 路径规则 R1–R12 由 `fs-overlay/tests/contract.rs`（K1–K6，`Rules` 驱动）覆盖；`workspace_api.rs` 覆盖 `mkdir/rename/rm/stat` 的正常路径 |
| `large_file.rs` | 7 | `Fs` 的 extent 直写与分块读 | overlay 的内容表示是页 + `base_limit`，由 `fs-overlay/tests/contract.rs` 的 K3/K6 覆盖 |
| `shape_transitions.rs` | 6 | 表后端 `shape_of` 的 Empty/Blob/Extents 切换 | 快照侧形态归一由 `fs-snapshot/tests/encode.rs` 覆盖（`fs-workspace` 的物化器产出同一归一） |
| `holes.rs` | 6 | 稀疏 extent 语义 | overlay 无 extent 行；零块省略由 `fs-workspace::materialize` 的编码规则与 `fs-snapshot` 编码测试覆盖 |
| `acceptance.rs` | 4 | `Fs` 的验收脚本 | 等价语义在 `fs-cli/tests/cli.rs` 的**全命令往返**里以命令形式重现 |
| `coverage_gaps.rs` | 10 | 补 `Fs` 分支覆盖 | 随 `Fs` 消失；新面的分支由 `workspace_api.rs`/`verify.rs`/`cli.rs` 覆盖 |
| `errors.rs` | 13 | `FsError` 的 13 类映射 | 新的 `ApiError` 面由 `workspace_api.rs` 的 `is_not_found`/`InvalidPath` 与 `cli.rs` 的失败路径覆盖 |
| `tree_store_contract.rs` | 10 | 用 `Fs` 驱动 K1–K6（不是契约本体） | 契约本体在 `fs-overlay/tests/contract.rs`，**保留未动** |
| `tree_store_contract_concurrency.rs` | 8 | 同上（并发分支） | 并发契约在 `fs-overlay/tests/contract.rs::k3_*`，保留 |
| `b1_races.rs` | 4 | B1 竞态全在表后端（extent 行、D+ root 链） | 实现消失则竞态不存在；KI-5 的记录保留在 `known-issues.md` |
| `referential.rs` | 5 | D+ root 链的引用完整性 | `root` 表退役后无对象可测 |
| `consistency.rs` | 7 | `verify` 报告**表**内部不一致（不可达 inode/无 inode 的条目） | 新面用 `verify` 的 `Dangling` 表示同类问题；`verify.rs::verify_names_an_object_that_is_gone` 在"对象丢失"形态上重现 |
| `verify.rs` | 5 | 旧 `verify` 的引用计数与报告形状 | `verify.rs` 的 4 条新测试覆盖浅/深两层、丢弃 workspace 后仍可验证、对象缺失被点名 |
| `gb.rs` | 1 | 1 GiB 形态切换 | 形态归一在 `fs-snapshot/tests/encode.rs`；容量级验收属 P2a-8 |
| `acceptance_scale.rs` | 2 | 10 万文件级 `Fs` 验收 | 属 P2a-8（性能与规模），不在 7b 范围 |

### fs-namespace（4 个文件，44 个测试）

| 文件 | 测试数 | 删除原因 | 语义是否在新测试重现 |
| --- | --- | --- | --- |
| `namespace.rs` | 29 | `Namespace<SqlTree>` 的行为 | 规则层由 `Rules` 的契约测试（`fs-overlay/tests/contract.rs`）覆盖；crate 内不再有后端可测 |
| `schema.rs` | 5 | 表后端 DDL 与版本治理 | schema 治理移到 `fs-workspace`（`workspace_policy` 读回校验 + 旧库拒绝测试） |
| `concurrency.rs` | 4 | 表后端的 TX2/TX3 并发 | 并发契约由 `fs-overlay/tests/contract.rs::k3_*` 与 `fs-workspace/tests/core.rs` 的并发发布测试覆盖 |
| `atomicity.rs` | 6 | 表后端 TX1 回滚 + `failure-injection` | TX1 在 overlay 上由该 crate 的 `ScopeGuard`/事务 drop 保证，`fs-workspace/tests/core.rs::a_rejected_rule_leaves_the_view_unchanged` 覆盖"拒绝即无变化"；故障注入点随 `inject.rs` 一起删除 |

### fs-cli（1 个文件，17 个测试）

| 文件 | 测试数 | 删除原因 | 语义是否在新测试重现 |
| --- | --- | --- | --- |
| `cli.rs`（旧） | 17 | 旧命令集（`--namespace`、10 条命令）与 `Fs` 绑定 | 新的 `cli.rs` 4 条测试覆盖全命令往返、失败路径、dirty/discard、`branch --from commit` |

**合计**：删除 165 个测试；新增 12 个（`fs-api` 8 + `fs-cli` 4）。全仓从 372 变为 220。

---

## 三、CLI 与卡面约定的差异（需知会）

1. **二进制名**：卡面写作 `layerfs <cmd>`，实际二进制仍是 `fs-cli`。原因：`docs/architecture.md`、
   ADR-004 等 10 处引用 `fs-cli`，改名属于打包决定，会同时影响文档与验收脚本。用法串也相应写
   `fs-cli`。若你希望改名为 `layerfs`，是一次 `[[bin]]` + 文档 + 测试环境变量的机械改动。
2. **`--store` 全局选项**：卡面的命令都得先知道"哪个库"。实现为 `fs-cli [--store <dir>] <cmd>`，
   缺省 `.`。`init <path>` 直接收路径。
3. **当前 workspace**：`<store>/cli/current`（一行 workspace 名），由 `checkout` 写入——按你的
   裁决放在 CLI 侧。`checkout <branch>` 的 workspace 名 = branch 名，重复 `checkout` 是"恢复"
   而非"新建"。
4. **`ls` 的两种形态**：`ls [path]` 读当前 workspace；`ls <commit>:[path]` 读历史（含 `:` 即历史）。
5. **提交 id**：`branch`/`commit`/`log`/`reset`/`revert`/`restore` 一律打印**完整** 64 位 id，
   便于 `cat <commit>:<path>` 直接复用（未实现前缀解析）。

---

## 四、验证

- 四件套：build 0 warning、clippy 0、fmt clean、doc 0
- 测试：**220 passed / 0 failed / 2 ignored**
- 红线：仍是 `crates/cas-store/{durability.rs, object.rs}`；本卡未改 cas-store 的 fsync 路径
- CLI 全命令集成测试：`init → branch → checkout → mkdir/put/ls/stat/get → commit → log →
  mv/rm → commit → cat/ls <commit>: → restore → revert → reset → verify/--deep → stats`
