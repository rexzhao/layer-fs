# R3-1：`truncate` 缩容后边界页尾部残留，后续扩展"复活"已丢弃字节

- 类型：产品缺陷（实现与既有 ADR 不符，不是规格歧义）
- 发现：QA 覆盖率第 3 轮，main @ `bc87371`，soak `cargo test --release -p fs-api --test reference_model -- --ignored`
- 命中：`seed=1000 step=357 op=truncate /n2 → 181`（长度一致，内容在 `[~107,~153)` 出现旧字节 `0xE9`，参考模型为全零）
- 定性：architect；实现：worker；测试：qa
- 结论：**产品错，参考模型 `resize(size, 0)` 正确**。ADR-006 §7.3、ADR-004 §4.3 与 POSIX 一致要求
  "缩小丢弃、扩大补零"：任何存储页在文件 EOF 之后不得持有非零字节。

---

## 1. 机制

`crates/fs-overlay/src/content.rs::truncate_impl`（P2a-7b 后表后端退役，全仓唯一 truncate 实现）：

```rust
let boundary = new_size.div_ceil(schema::PAGE_BYTES);
DELETE FROM ov_page WHERE serial = ?1 AND page_index >= boundary;
let new_limit = base_limit.min(new_size);
touch_inode(..., new_limit);     // size = new_size
```

- 删除的是"**起点** ≥ 新末尾"的页；**跨界页（`boundary - 1`）被整页保留，尾部没有裁掉**；
- `base_limit` 的下调只掩蔽 **base** 字节，掩不住 overlay 页里的旧字节；
- 读路径只在区间上按 `size` 截断（`read_at_impl` 的 `want = size - offset`），因此缩容当下看不出来；
- 之后 `truncate` 扩展或 `write_at` 越过 EOF 时，`size` 变大，同一页里的旧字节重新落入可读区间 →
  **被"丢弃"的数据复活**。

现有注释（`truncate_impl` 上方）声称"只其尾部经 size 检查不可读……不会被后续扩展重新抬高"，
该论证只对 base 字节成立，对 overlay 页不成立——注释本身需要随修复改写。

## 2. 规范依据

| 来源 | 原文 |
| --- | --- |
| ADR-006 §7.3 | `truncate(new)`：删除 `page_index ≥ ⌈new/page⌉` 的页、**把边界页的尾部截掉**；`base_limit = min(base_limit, new)`；size = new。**之后再扩展读到的都是零** |
| ADR-004 §4.3（A4） | `size < 当前`：丢弃尾部；`size > 当前`：补零扩展 |
| POSIX | 截断后的字节不属于文件；再次扩展的新区域读为零 |

## 3. 最小复现（`bc87371` 上 2/2 失败）

```text
A（纯 truncate）  write(0, 100×0xAA) → truncate 10 → truncate 200
                 读回 [10,100) = 0xAA，应为 0
B（truncate + write）write(0,100×0xAA) → write(300,10×0xBB) → truncate 50 → write(400,0xCC)
                 读回 [50,310) = 旧字节，应为 0
```

QA 的独立复现（3/3 失败）：`write(0,200×0xEE) → truncate 66 → truncate 181`，`[66,181)` 实测 `0xEE`。

## 4. 为什么既有测试没抓到

- K6 系（`fs-overlay/tests/contract.rs`）只覆盖"截到**页边界**"，KI-6 之后补的是"非对齐截断保留
  **存活**字节"，二者都不涉及"缩容 → 再扩展"；
- 常规参考模型序列 40 种子 × 200 步未撞上该组合；soak 10 × 1000 撞上。

## 5. 修法（architect 原型已验证，正式实现由 worker 落）

删页之后，若 `new_size % PAGE_BYTES != 0`：取 `page = boundary - 1`，该页存在且 blob 长于
`new_size % PAGE_BYTES` 时把 blob 截短写回（页允许短于 `PAGE_BYTES`，读路径按 `data.len()` 容错）。

边界条件：

1. `new_size` 页内 → 需要裁剪；
2. `new_size` 恰好页边界 → 整页保留，不裁；
3. `new_size == 0` → `boundary == 0`，**全删页，不要走 `boundary - 1`**；
4. 页不存在（纯 base 文件）→ 不做事，靠 `base_limit`；不要为清零把 base 页拷进 overlay；
5. 页已短于待保留长度（重复缩容）→ 不回填、不报错。

原型验证：最小复现转绿；`reference_model -- --ignored`（10×1000）26 s 全过；
`cargo test --release --workspace` 全绿（含 fs-overlay contract/crash、fs-workspace core/crash、
fs-api verify/acceptance）。原型 diff 未提交，留档于 `F:\work\arch-r3-1-trim-prototype.patch`。

## 6. worker 回归清单（review 依据）

1. 不变式写入注释/文档："任何存储页在 EOF 之后不得持有非零字节"；
2. ① 纯 truncate 缩后扩；② 缩后 `write_at` 越 EOF；③ 缩到 0 再扩；
3. ④ 缩容 → `snapshot`/`commit` → 再扩展并读取，确认物化产物与被截历史都不含旧字节；
4. 缩容后 `flush` / `snapshot` / 崩溃重开（`base_limit` 与短页共存）；`verify --deep` 干净；
5. soak 10×1000 必过，常规 40×200 仍绿。

## 7. 待确认（未证实推断）

CDC 增量物化器（P2b-1）读内容时是否假设页 blob 恒为整页长度。原型下 workspace 全套（含 CDC/encode）
未见异常，但覆盖不足，不能作为证明；修复后应专门跑 `fs-snapshot` / `fs-workspace` 的 CDC 与物化用例。

---

## 附：落地核对与 review 记录（architect，对提交 `8d6631c`）

> 对象：`t6/r3-1-truncate` @ `8d6631c`（父 `3346993`）。隔离 worktree + 独立 `CARGO_TARGET_DIR`。
> 结论：**ADR 一致性无阻塞**，两条非阻塞建议、四条记录如下（manager 归档）。

### 阻塞项复核（三条，均通过）

1. **无短页产生者**：全仓只有两处 `store_page` 调用（`write_at_impl`、`truncate_impl`）。前者恒以
   `vec![0u8; PAGE_BYTES]` 为底再覆盖写入；后者 `data[keep..].fill(0)` 且有 `data.len() > keep` 守卫，
   长度只减不增。若有新的产生者，`k6_*` 的 `content_state.pages` 与 `regressions` 的 181 字节断言会先红。
   遗留短页经 `truncate` 在 `len <= keep` 时不被改写，读路径按 `data.len()` 补零，仍满足不变式（§5.3）。
2. **无 `base_limit`/`ov_page` 新耦合**：`truncate_impl` 对 base 只经 `geometry()`（读）与 `touch_inode()`；
   没有 `base.read_at`、没有页物化。`write_at_impl` 中的 base 视图拷入是 §7.3 原有行为，本次未触碰。
3. **§9.2 物化器无需变动**：`materialize.rs::encode_file → chunk_file → read_block` 全程只经
   `overlay.read_at`（按 `size` 裁剪），不读 `ov_page` 行内容、不看页数；清零改的是逻辑字节，正是物化器
   应看到的变化，块边界/复用判据的输入（`size`、`base_limit`、页存在性）未变。

### 非阻塞建议

1. **§7.3 措辞补条件**：原文"把跨界页自 `live_end` 起的部分清零"对纯 base 文件易被读成"要造一个跨界页"，
   已改为"跨界页**若存在**，则……"（manager 直提，不改语义）。
2. **可省掉一次页重写**：`truncate` 目前只要跨界页存在就写回，即使尾部已全零；"`write_at` + `truncate`
   到同长"的路径每文件会多一次 4 KiB 页 UPDATE（进 WAL）。建议加守卫
   `data[keep..].iter().any(|b| *b != 0)`。无正确性失败条件，判据是 **P2b-2 首次 snapshot 基线**
   （若 1000 文件比 P2a 的 8–9 s 慢，这是第一嫌疑）；P2b-2 之前不改也安全。

### 记录（供后续阶段）

1. **不变式是归纳的，不只靠 `truncate`**：`write_at` 存整页且扩展 `size` 只跨越自己物化过的区间，同样保持
   "EOF 之后无零外非零字节"。不写下这条，未来做稀疏页/部分页存储的人会以为只需改 `truncate`。
2. **清零不回收空间是显式取舍**（KI-9 已记）：整页 4 KiB 保留、`content_state.pages` 计数语义不变；
   P2b 的 pack 核算不受影响（物化按逻辑字节重切，全零扩展仍归一到 `Empty`）。
3. **A/B 可复现性**：本卡"3 红 / 2 覆盖"的分类在"独立 target + 已提交 SHA"下可逐例复现，可作为 A/B 纪律
   第三条的首个实例。
4. **短页容错仍应保留**（§5.3 读路径）：实现不产生 ≠ 永不遇见（旧库、未来优化）；将来的短页优化必须继续
   保证"EOF 之后非零字节不可读"。
