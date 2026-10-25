# P3 FFI 边界验收骨架（自写最小 FFI 之后的高风险面）

> 来源：QA 2026-10-24（`F:/work/qa-scratch/p3-ffi-boundary-skeleton.md` 的仓库版本）+ manager 的两处加强。
> 适用：**P3-3b**（真实 host 实现 + `winfsp-sys`）。owner 已定“自写最小 FFI、不用 GPL crate”，因此这一层没有第三方可“信任”。

## ① 结构体布局与对齐

- **L1**：所有跨边界类型 `#[repr(C)]` + `size / align / offset_of` 对表（改字段顺序即红）；
- **L3（权威）**：用 SDK 头文件做等价断言（C 侧 `static_assert` 或 bindgen 常量）；
- 判据要点：布局表必须**注明数值来源**。数值若来自“我们自己的文档”，只能防实现漂移，**防不了文档本来就错**——后者只有 L3 能证。

## ② 调用约定与“不可展开”越界

- `extern "system"` 全覆盖（写成 `extern "C"` 在 x86 上参数错位）；
- **panic 不得跨边界，二选一并写进 ADR**：`panic = abort`（断言**子进程退出码**）或每回调 `catch_unwind`（转明确 NTSTATUS）；
- **两者都留 = 阻塞**（既 `catch_unwind` 又假定 abort ⇒ 行为不可预测）；
- 核心断言是**退出码**，不是断言文本——与 P2 崩溃注入同形。

## ③ 指针与生命周期

- **不返回借用**：`trybuild` 写“返回内部切片”的回调，**期望编译不过**；并配一条**“正确写法能编译过”的对照**（否则“全都编译不过”也能通过）；
- **回调返回后的存活**：生命周期计数断言“释放发生在驱动不再引用之后”；
- **释放时机**：`Arc::into_raw/from_raw` 成对，计数断言驱动释放前 >0、释放后 ==0（miri/ASAN 抓 double-free）。

## ④ 字符串边界（UTF-16）

- `Length` = **字节数、不含 NUL**；`MaximumLength` = 容量（搞混 ⇒ 往返不等）；
- 往返语料：ASCII / BMP / **非 BMP（代理对）** / 空串 / 恰好填满 / **奇数 `Length`（必须拒绝且给出明确 NTSTATUS，不得 panic）**；
- `Buffer` 不得写超过 `MaximumLength`（恰好容量 + 超长输入，ASAN 抓越界）。

## 两条硬口径（manager 追加）

1. **布局权威必须留 L3**：L1 的对表只能防漂移；用自己抄的常量证明自己的布局，与 P2 的 `root_source_columns = 3` 同族；
2. **panic 二选一**：写进 ADR，并在验收里用**子进程退出码**或**typed NTSTATUS**之一证明；两者都留即阻塞。
