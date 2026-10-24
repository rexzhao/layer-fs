# P2b-2c 任务卡（KI-11：并发发布下"索引指向缺失 pack"的定性与修复）

> 状态：**待派**（优先级 **高于 P2b-6**：GC 的"删无行文件 / 重打包"依赖 `pack` 行 ⇒ 文件存在这一不变式）。
> 来源：P2b-2 收口时的已知不变量缺口（`docs/reports/known-issues.md` KI-11、`docs/reports/p2b2-qa-verdict.md`）。

## 0. 已知事实（不要重做这些）

- 现象：两个 workspace 并发发布后，新开 `Store` 做 `verify` → `pack` 行与 `object_location` 行都在、该 pack
  文件 ENOENT（`os error 2`）；每次恰好缺一个 pack（`packs_checked: 3 / checked: 9` 恒定）。
- 复现率：worker 3–4/12（`--test-threads=1` 也 4/12）；QA 1/140（不能据此断言 Defender 扫 `%TEMP%`）。
- **已排除**：① 通用 syscall 模式（12 000 次 write→fsync→rename→stat 0 失败，但**不能**排除定向删除）；
  ② 跨用例 static 串扰（`--test-threads=1` 仍红）；③ 写路径未落地（`record_pack` 提交前 `exists + size` 断言
  从未触发 ⇒ **提交那一刻文件在、大小正确**）。
- 因此范围是：**提交之后文件不再可见**（删除类 / 目录级清理 / delete-on-close / 命名空间可见性）。
- QA 的现场打印形似 Windows delete-pending（另两个 pack `metadata()` 正常、`read_dir(packs/)` 枚举为空）；
  但她的枚举用了 `.flatten()`，**entry error 被静默丢弃**，故"目录为空"未定性。

## 1. 目标

1. **先取证，再谈修法**：拿到一次失败的完整现场（含 `packs/` 每个 entry 及其 error、evidence 硬链接状态、
   `pack_id` 实样、store 根 `canonicalize`）。没有现场输出不得开始改产品代码。
2. 定性到三类之一：**产品缺陷** / **外部删除（环境）** / **测试夹具**，并给出可复现的最小形状。
3. 按定性处置：
   - 产品缺陷 → 修复 + 注入式复现 + 并发发布 20 轮 ×2 组稳定 + `verify` 不变量成立；
   - 外部删除 → 夹具根移出 `%TEMP%`（`WCAS_TEST_ROOT` 到受控卷）、`docs/process/` 记"并发/文件系统类测试的
     夹具根由 CI 指定、避开实时扫描目录"、KI-11 写清证据与"产品无改动"；
   - 夹具 → 修复夹具并把 KI-11 改成"夹具缺陷（已修）"。

## 2. 可用工具（P2b-2 已铺好，默认关）

- `staging-audit` feature：verify 侧 ENOENT 重试 + 完整路径 + 逐条 entry error；rename 后 `packs/<id>.pack.evidence`
  硬链接；`new_pack_id()` 的 `(pid, nanos, counter, id)` 实样打印。
- QA 的两套 harness：`F:/work/qa-scratch/qa_pack_files_harness.rs`、`zz_qa_concurrency_probe.rs`（复用其现场格式；
  **先修掉 `.flatten()`**）。
- 两条 `#[ignore]` 用例：`fs-workspace --test pack_concurrency -- --ignored`。

## 3. 验收

- 证据：至少一份失败现场（含 entry error 与 evidence 状态），或"新增 N 轮压测（如 200 轮）零复现"的统计。
- 若定性为产品缺陷：注入式复现（不依赖概率）+ 修后 20 轮 ×2 组零失败 + `verify` 干净 + 两条并发用例解除
  `#[ignore]` 并在全量里稳定通过（连跑 5 次）。
- 若定性为环境：能解释"提交时在、之后不可见"的机制，并给出"换卷/排除后 0 复现"的对照数据；KI-11 更新为
  "环境事实，产品无改动"。
- 无论哪种：`verify` 对"`pack` 行 ⇒ 文件存在"这一不变式的**行为**必须写进文档（现在 `verify` 报 `os error 2`，
  没有明确语义）。

## 4. 边界

- 不改 pack 格式 / schema；不引入 GC 内容（P2b-6）。
- 若必须改 GC 依赖的假设，先回 manager，由 manager 转 architect 裁决。
