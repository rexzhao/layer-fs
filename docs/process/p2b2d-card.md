# P2b-2d 任务卡（`MissingPack` typed 语义：让"索引指向缺失 pack"可诊断）

> 状态：**已派**（2026-10-24，`worker`）。来源：KI-11 定性（外部删除）后的产品侧处置。
> 不涉及 GC、不改 pack 格式/schema。

## 1. 范围

1. **typed 错误**：`StoreError` 增加"索引指向的 pack 文件缺失"这一情形。两种形制二选一并说明理由：
   - 新增 `MissingPack { pack_id: String }`；
   - `Corrupt { cause: CorruptCause::PackMissing(String) }`。
2. **`verify`**：`pack` 行存在而文件缺失 → 该响应使用新类型，**不得**退化成 `StoreError::Io(os error 2)`；
   汇总里能数出"缺失 pack 数"。
3. **`Store::get`**：命中缺失 pack → 返回同一个 typed 错误（带 `pack_id`），**不得**报成"对象不存在"。
4. **ADR-008 §7 措辞**：明确"索引指向的 pack 文件缺失属于**存储损坏（外部删除/丢失）**，与对象不存在是两类
   错误"；核对表加一行。
5. **夹具**：构造"删掉 pack 文件"的夹具（这是唯一合法的人工破坏方式，不是伪造错误），断言 `verify`/`get` 的
   错误**类型与文案**。

## 2. 验收

- 四件套 0；全量绿；上面 4 类断言在场（verify 报 typed、get 报 typed、两者都不与 NotFound 混淆、报告可计数）。
- 不改 `pack` 格式、不改 schema、不改发布顺序。
- 不引入"容忍缺失"的行为（例如把缺失当成空对象）——**可诊断 ≠ 可忽略**。

## 3. 边界

"发布期可见性确认 + 有界重 staging"是另一张卡（P2b-2e），等 architect 判；本卡不做。
