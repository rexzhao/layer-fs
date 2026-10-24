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
