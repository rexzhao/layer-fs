# 覆盖率基线：origin/main @ 6ecae76（S3 收口材料草案 = P3 全部 + ADR-010 四期 S0–S3 + CLI 重构）

- 日期：2026-10-29
- SHA：**6ecae76**（origin/main，fetch 后确认；注意：不含尚未合并的 README 收尾卡 43c63c7）
- 工具：cargo-llvm-cov 0.9.1
- 方法：独立 worktree（qa-cov6-wt）；`--exclude fs-winfsp`（环境敏感 flake 三连：W6 并行 flake、mounted_write_chain 1/3 失败、mounted_read_only 盘符残留自失效——详见"遗留环境事项"）；`--test-threads=1`（串行执行消除了 live-mount 并发冲突——W6/registry_lifetime/discovery_live 三类 flake 在并行下各复现一次，串行后全部消失）；`--ignore-run-fail` 启用但**本轮零失败**（99 个测试二进制全 ok）
- 跳过的测试（具名，均属上列 flake 集）：`a_failed_remount_rolls_back_and_says_so`、`windows_can_write_through_the_mount_and_read_the_bytes_back`
- 遗留环境事项：Y:/Z: 两个 stale WinFsp 盘符（失败挂载运行的 DOS Device 残留，进程已死；mountvol/net use/Remove-PSDrive 均无法清除，需重启）——影响 mounted_* 真实挂载测试的前置断言（"Y: incident" 前例）

## 总量

| 指标 | 值 |
| --- | --- |
| 行 | 12 414 / 2 140 missed = **82.76%** |
| 函数 | 82.90%（1082/1304） |
| 区域 | 82.13% |

## 逐文件明细

**本轮只捕获了新 crate 部分（fs-semantics 及之后）；cas-core/cas-store/fs-api/fs-cli/fs-namespace/fs-overlay 的逐文件明细未捕获——以 TOTAL 为准，三态原则：未捕获 ≠ 覆盖良好。**

```text
fs-semantics/src/attributes.rs           16                 0   100.00%           2                 0   100.00%          16                 0   100.00%
fs-semantics/src/disposition.rs          22                 0   100.00%           1                 0   100.00%          21                 0   100.00%
fs-semantics/src/name.rs                 53                 1    98.11%           5                 0   100.00%          23                 1    95.65%
fs-snapshot/src/bytes.rs                257                17    93.39%          27                 0   100.00%         140                 5    96.43%
fs-snapshot/src/cdc.rs                  119                 1    99.16%           7                 0   100.00%          67                 1    98.51%
fs-snapshot/src/encode.rs               297                24    91.92%          15                 5    66.67%         219                21    90.41%
fs-snapshot/src/error.rs                 87                 2    97.70%           6                 0   100.00%          67                 1    98.51%
fs-snapshot/src/inode_v3.rs             277                15    94.58%           9                 2    77.78%         158                 4    97.47%
fs-snapshot/src/pager.rs                242                 8    96.69%           8                 0   100.00%         138                 2    98.55%
fs-snapshot/src/profile.rs               95                13    86.32%           8                 2    75.00%          69                12    82.61%
fs-snapshot/src/read.rs                1121               356    68.24%          31                 7    77.42%         587               158    73.08%
fs-snapshot/src/record.rs                49                 7    85.71%           4                 0   100.00%          46                 7    84.78%
fs-windows/src/attrs.rs                  35                 0   100.00%           4                 0   100.00%          23                 0   100.00%
fs-windows/src/case.rs                   41                 0   100.00%           6                 0   100.00%          29                 0   100.00%
fs-windows/src/delete.rs                 40                 0   100.00%           2                 0   100.00%          39                 0   100.00%
fs-windows/src/dirent.rs                 80                 8    90.00%           7                 1    85.71%          43                 5    88.37%
fs-windows/src/error.rs                  62                 0   100.00%           4                 0   100.00%          62                 0   100.00%
fs-windows/src/length.rs                 35                 4    88.57%           3                 1    66.67%          28                 3    89.29%
fs-windows/src/path.rs                  122                 4    96.72%           5                 1    80.00%          73                 3    95.89%
fs-windows/src/share.rs                  40                 0   100.00%           5                 0   100.00%          27                 0   100.00%
fs-windows/src/time.rs                   22                 1    95.45%           2                 0   100.00%          13                 1    92.31%
fs-windows/src/volume.rs                  3                 0   100.00%           1                 0   100.00%           8                 0   100.00%
fs-workspace/src/busy.rs                 35                 2    94.29%           6                 0   100.00%          23                 2    91.30%
fs-workspace/src/error.rs               201                12    94.03%          10                 0   100.00%         168                10    94.05%
fs-workspace/src/gate.rs                140                33    76.43%          17                 5    70.59%          89                19    78.65%
fs-workspace/src/gc/apply.rs            244                53    78.28%          12                 7    41.67%         153                41    73.20%
fs-workspace/src/gc/crash.rs             33                 6    81.82%           7                 2    71.43%          22                 4    81.82%
fs-workspace/src/gc/criteria.rs         107                 2    98.13%           8                 1    87.50%          62                 1    98.39%
fs-workspace/src/gc/mark.rs             194                33    82.99%          13                 2    84.62%         112                17    84.82%
fs-workspace/src/gc/mod.rs               80                16    80.00%           5                 1    80.00%          42                 7    83.33%
fs-workspace/src/gc/roots.rs             99                12    87.88%          13                 2    84.62%          72                12    83.33%
fs-workspace/src/gc/run.rs              145                12    91.72%           3                 0   100.00%          80                 2    97.50%
fs-workspace/src/gc/source.rs            12                 0   100.00%           2                 0   100.00%           9                 0   100.00%
fs-workspace/src/gc/sweep.rs            147                21    85.71%           8                 2    75.00%          88                10    88.64%
fs-workspace/src/ids.rs                 140                 7    95.00%          10                 1    90.00%          81                 6    92.59%
fs-workspace/src/main_db.rs             217                38    82.49%          17                 3    82.35%         151                30    80.13%
fs-workspace/src/materialize.rs         371                56    84.91%          26                 3    88.46%         234                43    81.62%
fs-workspace/src/publish.rs             275                33    88.00%          13                 1    92.31%         160                 9    94.38%
fs-workspace/src/repo.rs               1384               215    84.47%          90                 9    90.00%         818                82    89.98%
fs-workspace/src/schema.rs                5                 0   100.00%           1                 0   100.00%           3                 0   100.00%
fs-workspace/src/tree.rs                 49                10    79.59%           7                 2    71.43%          39                 9    76.92%
fs-workspace/src/workspace.rs           321                59    81.62%          40                 7    82.50%         235                32    86.38%
TOTAL                                  19319              3445    82.17%        1304               222    82.98%       12414              2134    82.76%
```

## 观察

- 最低覆盖：`fs-workspace/src/gc/apply.rs` 73.20%（GC 应用路径 41 行未覆盖）、`fs-snapshot/src/read.rs` 73.08%（CDC 读路径 158 行未覆盖，P2b 后续观察点）
- 错误枚举 Display 穷举（C6）：fs-overlay / fs-workspace 的 error.rs 仍缺 `every_variant_has_a_diagnosable_message` 式穷举（其余 crate 已有）；本轮实测 fs-snapshot/src/error.rs 已达 97.70%
- `fs-namespace/src/tree_store.rs` 在部分轮次显示 "0%"：纯 trait 定义文件（方法签名无可执行体），为覆盖率工具伪影，不计入缺口
- 与历史基线不可直接比较：6ecae76 相对 bc87371（P2b-0，85.63%）新增 fs-winfsp/fs-semantics/fs-windows 三个 crate + CLI 重构 + gc 全套，分母变化大

## 复现

```sh
git worktree add <dir> 6ecae76 --detach
cd <dir>
CARGO_TARGET_DIR=<tmp> cargo llvm-cov --workspace --exclude fs-winfsp --summary-only -- \
  --test-threads=1 \
  --skip a_failed_remount_rolls_back_and_says_so \
  --skip windows_can_write_through_the_mount_and_read_the_bytes_back
```
