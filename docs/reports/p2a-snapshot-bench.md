# P2a-1 快照编码器规模基准

- 日期：2026-10-23
- 卡：P2a-1（fs-snapshot crate）
- 用途：ADR-005 §9「数值是初值，P2a-1 卡用基准确认」——为 profile 参数提供实测依据
- 命令：`cargo run -p fs-snapshot --release --example scale`

## 0. 规则

- **数字仅记录，不设门槛。** ADR-005 §11 item 3 明确"不预先宣称"，本报告是测量结果不是验收标准。
- 每次运行输出全部记录，不挑选。
- 载荷特征随数字给出（收尾清单第 4 条）。

## 1. 环境

| 项 | 值 |
| --- | --- |
| OS | Windows 10 22H2 19045.6216 |
| CPU | i7-12700KF（20 线程） |
| Rust | rustc 1.92.0 |
| 构建 | `--release`（opt-level 3） |
| 工作盘 | F:（SATA SSD，见 `p1-baseline.md` §1） |

## 2. Profile 参数（本次测量所用）

```
mask_bits=6  min_entries=4  max_page_bytes=16384  block_bytes=65536
max_extent_len=65536  max_object_value=16777216  name_max=255
```

目标扇出 2^6 = 64。

## 3. 结果

### 3.1 inode 表

- **载荷**：200 000 个 inode；serial 0 是目录，其余 199 999 个是文件，每个 `size=5`、
  内容统一为同一个 blob（`ObjectId::from_bytes([1;32])`，不走 store）。created/modified 固定。
- **条目编码**：每条约 44 字节（键 8 + kind 1 + size 8 + 时间 16 + tag 1 + oid 32）。

| 指标 | 值 |
| --- | --- |
| 页数 | 3 004 |
| 扇出（条目/页） | 66.6 |
| 编码字节总量 | 13 362 181 |
| 编码耗时 | 113.3 ms |
| 页大小范围 | 103 – 16 110 字节 |

深度：3 004 页 / 66.6 ≈ 46 个叶页的父页 → 2 层（叶 + 根）。100 000 inode 时为 2 层。

### 3.2 单目录 100 000 条目

- **载荷**：一个目录，100 000 个条目，名字 `entry-000000` … `entry-099999`（12 字节定长），
  child_serial 0…99 999。

| 指标 | 值 |
| --- | --- |
| 页数 | 1 581 |
| 扇出 | 63.3 |
| 编码字节总量 | 2 191 680 |
| 编码耗时 | 41.0 ms |
| 页大小范围 | 89 – 10 253 字节 |

### 3.3 1 GiB 文件（16 384 个 extent）

- **载荷**：16 384 个 extent，每个 `length = 65536`（64 KiB），`logical_start` 连续，
  `object` 按 `index % 251` 循环取 251 个不同 oid（模拟内容不重复），`source_offset = 0`。
  合计 1 073 741 824 字节 = 1 GiB。

| 指标 | 值 |
| --- | --- |
| 页数 | 222 |
| 扇出 | 73.8 |
| 编码字节总量 | 929 227 |
| 编码耗时 | 8.1 ms |
| 页大小范围 | 198 – 16 077 字节 |

深度：222 页 → 叶 + 1 层分支 → 2 层。

## 4. 对 ADR-005 §5 初值的核查

| 参数 | 初值 | 实测支持？ |
| --- | --- | --- |
| `mask_bits=6`（扇出 64） | 是 | **支持**：三种树的实测扇出 63.3 / 66.6 / 73.8，均贴近 64 |
| `max_page_bytes=16384` | 是 | 支持：页大小上限实测 16 110，未触顶但邻近；16 KiB 远低于 16 MiB 对象上限 |
| `min_entries=4` | 是 | 无法由本基准单独判断（只在密钥哈希给出极小页时生效） |

**结论**：`mask_bits=6` 与 `max_page_bytes=16384` 有实测支持，不改。

## 5. 首轮测量发现的问题（已修）

首轮运行（修复前）报告：

```
inode table   entries=200000  pages=2993  fanout=66.8  bytes=13361580  encode=117.5ms  page=[103,16440)
```

**最大页 16 440 字节 > 16 384 上限。** 根因：字节护栏写成了"追加后判断 `> max_page_bytes`"，
允许页超出一格。ADR-005 §4 的实际规定是追加**之前**用
`max_page_bytes − max_entry_bytes` 预留判断。修复后最大页 16 110，`no_page_exceeds_the_profile_limit`
钉住该上限。

这处偏差是**基准跑出来才发现的**——单测与往返测试都不检查页大小，只有规模数据暴露了它。

## 6. 已知偏差：输入驻留

ADR-005 §8 要求编码器"内存 O(N/64)（只缓存各层未满页），1 GiB 文件的 extent 流与 10 万 inode
流都不需要整份驻留"。

**当前实现不满足**：为简化，各 `encode_*` 先把条目收集进 `Vec<Entry>`，因此输入驻留 O(N)。
这是格式之外的实现问题（不影响任何已发布的字节），记录为 KI-4，P2a 内视需要修。

影响评估：200 000 inode ≈ 13 MB 条目缓冲，1 GiB 文件 16 384 extent ≈ 0.9 MB——现实中不构成
压力，但与 ADR 的声明不符，不能默认为已实现。

## 7. 复现

```sh
cd /f/work/layer-fs
cargo run -p fs-snapshot --release --example scale
```

输出三行，格式：`label entries=N pages=P fanout=F bytes=B encode=Tms page=[min,max)`
