# P2b-2d + 2e 合入报告（`MissingPack` typed 语义 + 发布期 pin）

> 合入：main `6b2c201`（ff，2026-10-24）。QA 独立验收 **PASS，无未覆盖项**。
> 背景：KI-11（并发发布下 `verify` 可观察到"`pack` 行存在、文件缺失"），定性为**外部删除**。

## 1. 交付

| 提交 | 内容 |
| --- | --- |
| `27f9959` (rebase 后 `009f5b5`→…) | **2d**：`StoreError::MissingPack { pack_id }`；`verify` 的 `PackVerificationFailure.kind = {Missing, Corrupt}`；`VerificationReport.missing_packs`；`get` 同 typed 错误 |
| `7e87bf2`→`379239a` | **2e**：`durability::Pin`（Windows `share_mode(READ|WRITE)`，不含 `FILE_SHARE_DELETE`），生命期 = `rename` → `COMMIT`；`fail_next_pin()` 一次性钩子（`staging-audit`） |
| `6b2c201` | 测试侧：三处 `contains("MissingPack")` → **类型匹配**（抽 `is_missing_pack()`）；删除永假 sanity 断言 |

## 2. 四类错误互不退化（QA 独立夹具，破坏手段只有"删文件 / 改 1 字节 / 独占句柄"）

| 情形 | 期望 |
| --- | --- |
| 无索引行 | `NotFound` |
| 有行无文件 | `MissingPack { pack_id }`（`pack_id` 与删掉的那个逐字节相等） |
| 文件在、帧内改 1 字节 | `Corrupt` / `CorruptPack`（**不得**是 Missing/NotFound） |
| 文件在、另一句柄 `share_mode(0)` 独占 | `Io`（**不得**是 Missing） |

`verify`：`missing_packs` == `kind == Missing` 条数；每个受影响对象各一条失败；`packs_checked - missing_packs >= 1`；**全部缺失 ⇒ 判红**。

## 3. pin 证据

- **机制**（QA 独立复现）：pin 期间 `remove_file` → `raw_os_error() == 32`（`ERROR_SHARING_VIOLATION`）；同时 `get` 成功、字节一致；drop 后可删。
- **集成点**：`rename_replace` → `observer` → **pin** → `sync_directory` → `COMMIT`（与 ADR-008 §5.2 逐字一致）；`pack_concurrency -- --ignored` 10/10 绿（pin 前基线 3–4/12）。
- **无新增 fsync**：`sync-probe` 复算 `sync == 2k` 仍成立。
- **失败分支**（QA 用钩子独立复现）：`commit` 返回 `MissingPack`；`pack` 表 0 行、`object_location` 0 行；`packs/` 留一个无行 pack（归 GC）；随后 `verify` 干净；钩子一次性。

## 4. 证据边界（最容易被后人误读，务必保留）

1. **"10 连全绿"只覆盖"pin 有效 + 当轮未用到容忍"**，**不**背书容忍逻辑。10 次里 `removed after publication` 计数为 0。
2. **容忍逻辑的正确性证据是两条反证**：T2b（尾部改 1 字节 → `kind == Corrupt` 且 `missing_packs` 不增）与 T3（全缺失必红）。二者缺一不可。
3. `fail_next_pin()` 在 `crash-points`、`take_fail_next_pin()` 在 `staging-audit`：**必须同时开两个 feature** 才能测，否则"测不出来"会被误读为"分支不存在"。

## 5. 观察项（不阻塞，已记录）

- `assert_root_reads` 的 `inode.size` 断言只在成功路径生效（缺失时无从比较，合理）。
- pin 只覆盖 `rename → COMMIT`；**COMMIT 之后**的外部删除仍可能发生，因此 `pack_concurrency` 的容忍规则（只允许 `Missing`，`Corrupt` 照红）有 KI-11 与 ADR §5.2 的适用范围作依据。
