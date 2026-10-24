# P2b-2 QA 独立验收判定（对象 `p2b/pack @ 6b5f404`，基 main `42410fc`）

> 验收：QA（独立实现判据、隔离 worktree、两臂各自独立 `CARGO_TARGET_DIR`、均跑在已提交且工作树干净的 SHA 上）。
> 归档：manager。原文要点保留，措辞未改。

## 结论

**PASS（附带已知不变量缺口 KI-11）**——并发发布下 `verify` 可观察到 `pack` 行存在而文件缺失；欠账见 KI-11。
**未完全覆盖项**：并发发布稳定性。

另有 **1/140** 复现的外部删除特征（`metadata()` 可取到另两个 pack 的大小、`read_dir(packs/)` 却枚举为空；
枚举使用 `.flatten()`，故"目录为空"未定性），未定性，**不得**表述为"Windows 偶发 IO"。

**判据曾因 instrumentation 污染而失真，已由 `lock-probe` 修正**（`m ≤ S/10` 曾测的是 trace 回调成本，
而非持锁成本）。

## 逐项

| 项 | 判定 | 关键证据 |
| --- | --- | --- |
| 四件套 / 红线 | PASS | build/clippy/fmt/doc 0；红线只到 `durability.rs` 的 `#[cfg(feature)]` 增量 |
| A 探针归属 | PASS | 验证方 additive 检查器零违反 + 阳性对照（`inject_sync_inside_next_transaction`）判红 |
| C 计数 | PASS | k=1/2/3：五计数 = k、`sync = 2k`、`SyncFile < objects`、`Σcount = 对象数`、`Σbytes = Σ帧长 + 48k` |
| B V1–V8 | PASS | V1/V2/V5 亲判；V8 复核 `new_unchecked(Immediate)` 与 trace 文本 |
| E 尾部/单读 | PASS | `frames_len + 48 = 文件大小`；索引 `length = 帧长`、`offset+length ≤ frames_len`；`get` 帧头 `payload_len = 0` |
| D 锁持有 | PASS（判据已修正） | `lock-probe`：`S = 1.04 ms`、`m = 6.18 µs/object ≤ S/10 = 104.4 µs`（16.9×）、`H(1000) max = 11.07 ms ≤ 104.4 ms` |
| §2.1 拒绝矩阵 | PASS | `{0,1,2,4}` 连续两次被拒；被拒前后 `cas.sqlite` blake3 相同；`tmp/` 哨兵逐字节不变 |
| §2.3 崩溃注入 | PASS | 真子进程 `Command::new(current_exe)` + `abort()`；`cas-store crash` 11 + `fs-workspace crash` 7 全绿 |
| §2.3 两边成功 | PASS | A（顺序 open + 并发 commit）10 轮、B（并发 open + 并发 commit）10 轮，40 次发布全部 `Ok`，`Busy = 0` |
| §2.4 性能 | PASS | 首次发布 1000 文件 49.7/50.0/52.1 ms vs P2a 7 945/8 152/9 305 ms（≈160–185×）；同树无变更再发布 ≈1.4× P2a（剩余成本是全树遍历，KI-8）；`materialize.rs`/`repo.rs` 零 `truncate` |
| §2.5 回归 | PASS | 全量 275 passed / 0 failed / 6 ignored；soak 10×1000 绿；M9 秒级 |
| §5 `tmp/` | PASS | 重试三条件 + attempt 作用域 + 并发回归（20 轮 × 4 线程，80 次发布中 6 次吸收重试） |
| `pack_concurrency` | 已知缺口 | 见 KI-11；两条用例 `#[ignore]`，理由写明"索引可指向缺失 pack" |

## 方法学（保留原文）

- **先验工具，再验被测物**：D 的判据在开 `sync-probe`（含 `rusqlite/trace`）时被 instrumentation 放大 2–3 倍，
  边缘失败是测量器的，不是实现的。
- **错误信息必须是事实，不是策略常量的转述**：`waited_ms` 曾填 `MAIN_BUSY_TIMEOUT_MS`，导致 5 ms 的失败被报成
  "stayed locked for 5000 ms"。修正后两处既有断言（`core.rs`）也随之从"钉常量"改为"钉事实"。
- **阳性对照先于阴性结论**：每个"应为 0/应相等"的断言，先证明检查器能抓到自己注入的违反，否则记"未验证"。
