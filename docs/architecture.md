# 架构准则（Architecture Principles）

状态：manager 维护；负责人授权 manager 在 review 中强制执行（2026-10-22）。
任何提交不符合本准则的，manager review 退回修改。准则变化须经 manager 修订本文件并说明理由。

依据：workspace_cas_fs_spec.md §3（分层架构与原则）、docs/adr/ADR-001/002/003、docs/research/2026-10-22-layerfs-predev-study.md。

## 1. 分层与依赖方向（不可逆）

```
fs-cli ──► fs-api ──► fs-namespace ──► cas-store ──► cas-core
（P3）fs-winfsp ──► fs-api
（P2）fs-overlay / fs-workspace ──► fs-namespace（届时定）
```

- 依赖只能沿箭头方向；**禁止循环依赖、禁止跨层**（如 fs-cli 绕过 fs-api 直接触 fs-namespace 内部、fs-namespace 直接操作对象文件）。
- 外部入口（CLI、P3 的 WinFsp）只经 fs-api，不得依赖下层内部类型。
- 新增第三方依赖：先证明现有依赖不可满足（spec §4 精神）；锁定版本进 workspace.dependencies。

## 2. 模块所有权（"谁不知道什么"）

| 模块 | 拥有 | 禁止知道 |
|---|---|---|
| cas-core | 对象身份、信封、编解码（纯计算） | 存储、路径、命名空间、平台、SQLite |
| cas-store | 对象文件布局、durable 发布、恢复、校验、元数据 schema | 目录语义、文件身份、CLI、句柄 |
| fs-namespace | FileId、目录条目、root 发布事务、路径操作 | 对象物理布局、CLI、Windows 语义 |
| fs-api | 对外稳定语义（spec §3 trait 落地）、操作组合 | 算法实现（不得出现编码/存储细节） |
| fs-cli | 参数解析与输出（薄命令面） | 一切业务逻辑 |
| fs-winfsp（P3） | Windows 语义适配（share mode、ACL、句柄生命周期） | 去重算法、直接修改 CAS 对象、自行管理 Commit（spec §3 原则） |

算法归下层，语义归上层，适配层只做翻译。同一算法不得出现第二份实现（LayerFS 教训：禁止 error-driven 替代实现）。

## 3. 横切规则

1. **错误语义**：类型化错误、一次操作不重试、精确失败保管（失败保留原因与已产生效果）。下层错误可被上层包装，但语义不得丢失或被吞。
2. **格式与 schema**：对象格式冻结于 ADR-001；任何格式/schema 变化必须走 ADR + 版本策略声明，schema 自校验（DDL 逐字比对）不得绕过。
3. **平台红线**：平台 API 仅允许出现在 manager 批准的单点平台模块（先例：cas-store/src/durability.rs）；平台专属依赖必须 cfg 门控。cas-core 永远零平台依赖。
4. **工程上限**：生产文件 ≤999 行、mod.rs/lib.rs ≤200 行；测试钩子必须 feature 门控（先例：crash-points）。
5. **命名空间核心区分大小写**（原始字节名，ADR-001 D8）；大小写折叠归 P3 Windows 前端。
6. **ADR 先行**：影响持久化格式/事务语义/跨层契约的设计决策，先 ADR 后编码（spec §0-6）。

## 4. manager review 架构检查单（每卡强制）

1. 依赖方向与落位：新代码在正确的 crate 吗？有没有跨层引用/职责越界？
2. 有没有第二份实现（重复算法、平行路径）？
3. 错误语义符合 §3.1？
4. 格式/schema 变化是否走了 ADR 与版本策略？
5. 平台红线与工程上限合规？
6. 公共 API 面最小、文档化、测试钩子门控？
7. 与既有 ADR 决策冲突？（冲突即退回，除非先修订 ADR 并经 manager 批准）
