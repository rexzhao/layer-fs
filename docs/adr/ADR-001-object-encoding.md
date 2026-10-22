# ADR-001：对象编码、ObjectId 与文件身份

- 状态：已接受（Accepted），适用于 P1 / 格式版本 1
- 日期：2026-10-22
- 决策者：manager（规格 `workspace_cas_fs_spec.md` v0.1 §4、§11；调研 `docs/research/2026-10-22-layerfs-predev-study.md` §4.1、§5）
- 实施者：T1（本 ADR 与 `crates/cas-core` 同步提交）
- 相关：ADR-002（Store 物理布局与事务，T2 起草）

本 ADR 记录**已经实现并有测试覆盖**的格式决策。凡标注"未实现"的特性一律不存在，
不得在提交说明、README 或对外文档中宣称其可用。

---

## 1. 背景与问题

P1 需要一个**规范对象身份**：同样的字节在任何位置、任何时刻、任何进程中都必须得到
同一个身份，从而让全局去重成立；同时不同种类的对象即使载荷字节相同也必须身份不同。

需要同时钉死：

1. 身份用什么哈希、吃什么字节；
2. 对象的规范编解码是什么样，哪些违规必须拒绝；
3. 身份与"逻辑文件身份"的边界（spec §0-4：不得把内容相同与文件身份相同混为一谈）；
4. 大小上界与超界语义；
5. 错误语义（spec 与调研 §2.8：一次操作、不重试、精确失败）；
6. 后续阶段可以如何扩展格式而不破坏已写入的 Store。

## 2. 决策摘要

| # | 决策 | 状态 |
| --- | --- | --- |
| D1 | `ObjectId = BLAKE3(domain ‖ canonical_envelope)`，32 字节，小写 hex 呈现 | 已实现 |
| D2 | 域分隔符 `wcas/object/v1\x00` 冻结、自带版本、以 NUL 结尾 | 已实现 |
| D3 | 信封 `magic "WCAS" \| version u8 \| role u8 \| payload_len BE32 \| value_len BE32 \| value` | 已实现 |
| D4 | role 十进制 1–8 的闭集，越界即解码错误，不得视为"未知对象" | 已实现 |
| D5 | 单对象上界 16 MiB（值上限；另有帧长上界） | 已实现 |
| D6 | 解码全程校验：头长 → magic → 版本 → role → 帧长上界 → 声明长度 vs 实际长度 → 尾字节 | 已实现 |
| D7 | 流式与整体哈希必须逐字节一致，不驻留整对象 | 已实现 |
| D8 | 核心命名空间**区分大小写**，名称是原始字节 | 决策（T3 实施） |
| D9 | `ObjectId`（内容身份）与 `FileId`（逻辑文件身份）严格分离 | 决策（T3 实施） |
| D10 | 一次操作、不重试；失败返回精确、可分类的错误，不静默兜底 | 已实现（本 crate 范围） |
| D11 | `payload_len` 在 v1 恒为 0，仅为未来内容布局预留；写出非零值需新版 ADR | 已实现（恒 0） |
| D12 | 读取默认整对象重算 BLAKE3 校验 | 决策（T2 实施） |

## 3. ObjectId

### 3.1 定义

```text
canonical_envelope := "WCAS"(4) ‖ version(1) ‖ role(1) ‖ payload_len(BE32) ‖ value_len(BE32) ‖ value(value_len)
ObjectId           := BLAKE3( "wcas/object/v1\x00" ‖ canonical_envelope )   // 32 字节
```

- 哈希覆盖**整个信封**，因此 `role`、`version`、`payload_len`、`value_len` 全部参与身份。
  这正是满足 spec §4-1"跨角色不能因字节巧合导致身份混淆"的机制：同样 10 字节的
  `value`，作为 `Bytes` 与作为 `DirectoryLeaf` 得到不同 `ObjectId`（测试
  `same_value_under_different_roles_has_different_identity`）。
- 哈希**不再额外**覆盖"逻辑文件名、路径、inode 号、时间戳、引用者"。身份只描述内容，
  不描述位置（见 §6 与 D9）。

### 3.2 域分隔符

`wcas/object/v1\x00`，冻结。三条理由，缺一不可：

1. **跨系统隔离**：本项目的哈希不与任何其它系统的哈希混淆（即使对方也恰好用 BLAKE3）。
2. **自带版本**：域字符串内含 `v1`，与信封 `version` 字节互为冗余校验；改域即改全部身份，
   因此必须是显式版本决策而非笔误可及的改动。
3. **结束标记**：末尾 NUL 保证长度可变的前缀不会与后续字节产生拼接歧义。

> 复用说明：本项目独立设计格式。域字符串、魔数、字段排布、role 编号均与本项目规格
> （spec §4、调研 §4.1）一致；与参考实现（LayerFS）的域字符串与编解码实现在实现层面
> 不共享代码，也不保证相容——两者互不读取对方的 Store。

### 3.3 呈现形式

- 内存/磁盘原始形式：`[u8; 32]`。
- 文本形式：**64 个小写 hex 字符**，用于对象文件路径、CLI 输出与日志。
- 解析：`FromStr` 接受大小写混合的合法 hex；长度不等于 64 或含非 hex 字符即
  `IdParseError::{Length, NotHex}`，附带失败偏移量。

### 3.4 流式身份

`ObjectIdHasher::start()` 先混入域分隔符；`update_envelope(role, value_len)` 混入信封
头（只可调用一次，且先于任何值字节）；`update_value(chunk)` 可任意次。契约与保证：

- 分片边界不影响结果，与 `ObjectId::hash(role, value)` 逐字节一致
  （测试 `streaming_identity_matches_whole_buffer_identity`，覆盖 1/3/7/64/4095/4096
  字节分片及空值）；
- 不驻留整对象，内存为哈希状态加调用方切片；
- `update_envelope` 会拒绝超过上界的声明长度；
- 未实现的检查：`finish()` 目前**不验证**"实际喂入字节数 == 声明 `value_len`"。调用方
  必须自行保证；完整帧的解码路径由 `Object::decode` 的长度校验兜底。T2 若需要更强的
  流式保证，应在此处补充计数校验（属实现改进，不改格式）。

## 4. 对象信封

### 4.1 字节布局（大端）

| 偏移 | 长度 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | 固定 `WCAS` |
| 4 | 1 | `version` | v1 = `1`；其它值拒绝 |
| 5 | 1 | `role` | 见 §4.2 |
| 6 | 4 | `payload_len` | v1 恒为 0（D11） |
| 10 | 4 | `value_len` | `value` 的字节数 |
| 14 | n | `value` | 规范值字节 |

固定头 14 字节（`ENVELOPE_HEADER_LEN`）。

### 4.2 role 枚举（v1 闭集）

| 值 | 名称 | 含义 | P1 是否使用 |
| ---: | --- | --- | --- |
| 1 | `bytes` | 任意规范字节；整文件的内容根 | 是 |
| 2 | `chunk` | 大文件的内容分块 | 否（P2） |
| 3 | `extent-leaf` | 文件 extent 索引的叶 | 否（P2） |
| 4 | `extent-branch` | 文件 extent 索引的内部分支 | 否（P2） |
| 5 | `directory-leaf` | 目录条目页 | 否（P1 走 SQLite 表，见 §5） |
| 6 | `directory-branch` | 目录索引内部分支 | 否（P2 以后） |
| 7 | `inode-table` | 命名空间 inode 表 | 否（同上） |
| 8 | `filesystem-root` | 不可变文件系统根 | 否（T3 决定编码方式） |

role 是**闭集**：编号 0、9–255 一律解码失败（`UnknownRole`）。这一点是安全属性而非
便利性——若把未知 role 当作"未知对象"放行，损坏字节可能被误认成合法对象。新增 role
属于格式变更，需新 ADR 并提升 `version`。

### 4.3 大小上界

| 常量 | 值 | 语义 |
| --- | --- | --- |
| `MAX_VALUE_LEN` | 16 MiB（16,777,216 B） | 单个对象的 `value` 上限 |
| `MAX_OBJECT_LEN` | 16 MiB + 14 B（16,777,230 B） | 完整帧（头 + 值 + 载荷）上限 |

- 超界在**构造**与**解码**两侧都拒绝（`TooLarge`，附带声明值与上限）。
- 恰好等于上限是合法的（测试 `construction_accepts_a_value_exactly_at_the_cap`）。
- 16 MiB 的选择：与调研 §4.1 一致的经验值；P1 整文件对象以 64 KiB 固定块切分
  （见 §5）后远小于上限；上限存在保证了单个对象解码的内存可预期。

### 4.4 解码校验顺序

`Object::decode` 按固定顺序返回**第一个**失败项，不合并、不忽略：

1. 输入短于 14 字节 → `Truncated { needed, actual }`
2. 前 4 字节 ≠ `WCAS` → `BadMagic { found }`
3. `version` ≠ 1 → `UnsupportedVersion { found }`
4. `role` 不在 1–8 → `UnknownRole { found }`
5. 声明帧长 > `MAX_OBJECT_LEN` → `TooLarge { declared, limit }`
6. 声明帧长 ≠ 输入长度（含**截断**与**尾随字节**两种情况）→ `LengthMismatch { declared, actual }`

因此"多一个尾字节"与"少一个值字节"都是硬错误，解码器不会默默忽略尾部内容——这一点
直接防止了拼接攻击与旧版本尾部填充。

### 4.5 身份校验（读路径）

`verify_id(expected, bytes)` 解码后重算身份并与 `expected` 比较，不一致返回
`IdentityMismatch { expected, actual }`。语义是：**坏对象报数据损坏错误，绝不静默填零**
（spec §4 末条）。T1 提供该函数；T2 在 `get` 路径上强制调用（D12）。

## 5. 命名空间与文件身份（决策，T3 实施）

本节是决策记录，**代码尚未实现**。T3 必须按此执行，若要偏离需先修订本 ADR。

### 5.1 路线 A：SQLite 常驻命名空间

采纳调研 §4.3 的建议 A：

- 目录条目、inode 记录存于 Store 的 SQLite 表；
- 每次写操作是**单事务**：改行 + 生成新 root 记录 + 推进 CAS head；
- 旧 root 记录保留、不可变（即快照雏形）；
- 不做"目录页 / inode 表也是 CAS 对象"的全对象化（路线 B），理由：P1 工作量翻倍，
  而 P1 的验收（spec §6 P1-1/4）只要求原子发布与旧 root 可读。

因此 role 5/6/7 在 P1 不会被写入；它们占据编号是为路线 B 或 P2 迁移预留，**不宣称可用**。

### 5.2 大小写语义（D8）

- 核心命名空间的名称是**原始 UTF-8/字节序列**，比较**逐字节、区分大小写**；
- 不在核心做大小写折叠、Unicode 规范化或 Windows 路径别名（`\\?\`、8.3 短名）处理；
- `Readme.md` 与 `README.md` 是两个不同的名字，可以同时存在于同一目录；
- Windows 前端（P3）负责把 Win32 的大小写不敏感语义映射到核心：同一目录下大小写
  不同的两个条目在 P3 必须先检测为冲突并按 ADR-003 定义的行为处理，不得让上层
  出现"两个都能打开"的不确定状态。

理由：核心保持单一、可证伪的规则；平台语义差异被隔离在适配层，避免核心在 Linux 与
Windows 上产生不同身份。

### 5.3 内容身份 vs 文件身份（D9）

| 概念 | 类型 | 含义 | 相同意味着 |
| --- | --- | --- | --- |
| 内容身份 | `ObjectId` | 某段规范字节的身份 | 字节完全相同，物理上一份 |
| 文件身份 | `FileId`（带 namespace/scope） | 某个目录项的逻辑身份 | 同一个文件，可跨 rename |

规则：

1. 两个不同 `FileId` **可以且应当**引用同一个 `content_root`（去重）；
2. 一个文件被改写后**保持** `FileId`，而 `content_root` 换成新 `ObjectId`；
3. 仅因 `content_root` 相同，不得推断两个路径是同一个文件（不能被当作硬链接）；
4. `FileId` 的分配必须带命名空间/作用域前缀，防止跨 Store 或跨 Workspace 合并时碰撞。

spec §6 P1-1（"100 个路径引用相同内容 → 只有一个内容对象，但文件身份与元数据各自独立"）
就是本条的直接验收。

### 5.4 大文件与块尺寸（决策，T3/T4 实施）

- P1 用 **64 KiB 固定块**（恰好是 `value` 上限的 1/256，且与 P2 的 CDC 目标 16 KiB
  **不同**，避免读者把固定块误认为内容定义分块）；
- 行程编码的 extent 列表存 SQLite；`read_at` 按区间查表合成，不加载整个文件；
- 明确**未实现**：CDC 内容定义分块、delta 编码、块级去重。P1 只在**相同规范块**
  级别去重，不保证"改一个小片段只存小片段"。

## 6. 错误语义（D10）

适用范围：`cas-core` 已实现；后续 crate 沿用同一原则。

1. **一次尝试**：每个操作只执行一次。不做自动重试、退避重试、busy handler、
   失败重放或错误驱动的算法/后端替换。
2. **精确失败**：错误是带字段的具名枚举值，不是字符串；调用方可程序化分类。
   本 crate 的两个错误类型是 `EnvelopeError`（7 个变体）与 `IdParseError`（2 个变体），
   全部 `PartialEq + Eq`，测试直接比对变体。
3. **不静默兜底**：解码失败不返回"部分对象"、不填零、不跳过坏字段。坏对象即损坏。
4. **失败不留半成品**：本 crate 无副作用，天然满足；T2 起由 Store 保证（写入暂存 →
   校验 → 原子发布，索引提交前不可见）。
5. **未定义行为一律拒绝**：未知版本、未知 role、超界、尾随字节都是硬错误；没有
   "宽松模式"。

## 7. 兼容性策略

- `version` 字节是解码的第一道闸门：见到非 1 的版本直接 `UnsupportedVersion`，
  **不做降级解析、不做猜测性转换**。
- 格式变更分两类：
  - **附加式**（例如未来把 `payload_len` 用起来）：提升 `version`，旧 Store 在打开时
    被明确拒绝并提示重新初始化；
  - **破坏式**：同时替换域分隔符的版本号，使新旧身份必然不同。
- 本项目目前**没有任何已发布的 Store**，所以 P1 阶段不提供迁移工具；若在 P1 期间调整
  格式，直接重建 Store 并在 ADR 中记录即可。
- 稳定的部分（如 role 编号、魔数、字段顺序）一经实现即视为冻结；改变需本 ADR 修订 + 测试。

## 8. 未实现特性（明确清单）

以下特性**均未实现**，不得在任何地方宣称存在或可用：

| 特性 | 计划阶段 | 说明 |
| --- | --- | --- |
| CDC / 内容定义分块 | P2 | role 2/3/4 已预留编号但无编解码实现 |
| Delta 编码 | P2 | 无 |
| Pack 打包 / 压缩 | P2 或以后 | P1 用独立对象文件 |
| 垃圾回收（引用计数） | P2 | P1 仅"启动清 tmp + 全量 verify" |
| Workspace / Overlay / 脏页 | P2 | 无 |
| Snapshot / Branch / Commit | P2 | P1 仅有 head + root 记录 |
| WinFsp 挂载 | P3 | 无 |
| Windows 属性 / ACL / 共享模式 | P3 | 无 |
| 文件锁 | P3 | 无 |
| 目录页 / inode 表对象化（路线 B） | 未定 | 需新 ADR |
| 加密 / 压缩后去重收益 | 非目标 | 不做收益保证 |
| 跨机 / 分布式 CAS | 非目标 | 不做 |
| 流式 `finish()` 的字节数校验 | T2 可选改进 | 见 §3.4 |

## 9. 公开 API 示例

以下示例直接取自实际编译运行的代码（`cargo test --doc` 覆盖
`ObjectIdHasher` 的示例）。

```rust
use cas_core::{verify_id, Object, ObjectId, ObjectIdHasher, Role};

// 1) 构造对象并取身份
let object = Object::new(Role::Bytes, b"hello, world").unwrap();
let id = object.id().unwrap();

// 2) 规范帧 = 该对象的全部字节；写盘/哈希都用它
let frame: Vec<u8> = object.to_vec();
assert_eq!(&frame[0..4], b"WCAS");

// 3) 解码：任何畸形字节都会被精确拒绝
let decoded = Object::decode(&frame).unwrap();
assert_eq!(decoded.value(), b"hello, world");
assert_eq!(decoded.role(), Role::Bytes);

// 4) 读路径校验：身份不符即报损坏
verify_id(id, &frame).unwrap();

// 5) 流式身份：分片喂入，不必驻留整对象
let mut stream = ObjectIdHasher::start();
stream.update_envelope(Role::Bytes, 12).unwrap();
for chunk in b"hello, world".chunks(5) {
    stream.update_value(chunk);
}
assert_eq!(stream.finish(), id);

// 6) 十六进制往返（64 个小写字符）
let text = id.to_string();
assert_eq!(text.len(), 64);
assert_eq!(text.parse::<ObjectId>().unwrap(), id);
```

运行：

```sh
cargo build --workspace
cargo test --workspace            # 28 个测试
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 10. 正确性风险与已知限制

1. **BLAKE3 依赖**：`blake3` 锁定为 `1.8.7`（workspace 依赖表）。它是纯 Rust 实现、
   有官方 SIMD 加速；升级需单独评估并记录。版本被锁在 workspace 级别，避免各 crate
   漂移。
2. **32 字节身份无版本位**：版本信息在域字符串与信封头里，不在 `ObjectId` 本身。
   若将来需要"同一字节、两个格式版本"共存，靠 `version` 字节区分即可，`ObjectId` 不必
   承载版本。
3. **只校验长度不校验语义**：`cas-core` 不知道 role 对应的值应当是什么结构（例如
   `filesystem-root` 的字段布局）。语义校验归 T3 的命名空间层，本 crate 明确不做。
4. **`payload_len` 恒 0**：任何写出非零值的代码路径都是错误；未来启用它需要新 ADR
   与版本提升。
5. **流式 `finish()` 不核对字节数**：见 §3.4，属已知缺口，非格式问题。
6. **未做模糊测试**：当前用 26 个定向用例覆盖畸形输入。随机模糊（如 `cargo-fuzz`）
   尚未接入，属 T5 可选增强；**在接入前不得宣称"已通过模糊测试"**。
7. **未做性能测试**：本 ADR 不含任何延迟或吞吐数字。

## 11. 测试清单（对照验收标准）

`crates/cas-core/tests/object_format.rs`（26 个集成测试）与
`crates/cas-core/src/error.rs`（1 个单元测试）、`cas-core` 文档测试（1 个）。

| 验收项 | 测试 |
| --- | --- |
| 编解码往返 | `roundtrip_preserves_role_and_value`、`roundtrip_covers_every_role_and_value_shape`、`frame_into_caller_buffer_reports_written_length`、`read_header_accepts_a_header_without_the_value` |
| 错 magic | `decode_rejects_wrong_magic` |
| 错版本 | `decode_rejects_wrong_version` |
| 错 role | `decode_rejects_unknown_role` |
| 长度不符（截断 / 尾随字节 / 声明载荷） | `decode_rejects_truncated_value`、`decode_rejects_a_trailing_byte`、`decode_rejects_a_declared_payload`、`decode_rejects_empty_and_short_input` |
| 超上界 | `decode_rejects_declared_value_beyond_the_cap`、`construction_rejects_a_value_beyond_the_cap`、`construction_accepts_a_value_exactly_at_the_cap`、`streaming_envelope_refuses_an_oversized_value` |
| 跨 role 相同值身份不同 | `same_value_under_different_roles_has_different_identity`、`verify_id_reports_a_role_substitution` |
| 流式与整体一致 | `streaming_identity_matches_whole_buffer_identity`、`streaming_identity_matches_an_empty_value`、`streaming_identity_matches_the_framed_object_identity` |
| 身份校验 | `verify_id_accepts_the_matching_object`、`verify_id_reports_a_content_substitution` |
| 幂等/稳定身份 | `same_role_and_value_has_identical_identity`、`decoded_object_hashes_to_the_same_identity` |
| hex 呈现与解析 | `identity_is_lowercase_hex_and_parses_back`、`id_parse_rejects_wrong_length_and_non_hex` |
| 错误信息可诊断 | `error::tests::displays_name_the_failing_rule` |

执行结果（Rust 1.92.0，Windows x86_64）：

```text
cargo build --workspace                                        -> Finished, 0 warnings
cargo test --workspace                                         -> 28 passed, 0 failed
   (cas-core lib 1, object_format 26, doc-tests 1)
cargo clippy --workspace --all-targets -- -D warnings           -> Finished, no warnings
cargo fmt --all -- --check                                     -> clean
```

**未运行**：任何性能测试、模糊测试、跨平台（Linux/macOS）CI 验证、100k 文件规模测试。
上述"全绿"仅指本机 Windows 上的编译/测试/lint 结果。

## 12. 跨平台红线

生产代码（`crates/*/src/`）**零平台专属依赖**。当前状态：

- 依赖仅 `blake3 1.8.7`（纯 Rust，无 `cfg(target_os)` 分支出现在本项目代码中）；
- 无 `nix`、无直接 `libc`、无 `std::os::unix`、无 `std::os::windows`、无 `winapi`、
  无 `windows-sys`；
- 核心做文件 I/O 的 crate（T2 起）如需 platforms 差异，必须集中在**单一受 cfg 门控
  的模块**内，并在 ADR 中列明（调研 §3-2 的教训：参考实现出现 115 处 cfg 站点与
  4 个未门控依赖）。

验收方式：`grep -rn 'std::os::\(unix\|windows\)\|use nix\|libc::\|winapi\|windows_sys' crates/*/src`
必须为空（当前为空）。

## 13. 下一步（最小任务）

- **T2**：`cas-store` 实现对象文件布局（`objects/ab/cd/<oid>`，2 级前缀）、`put/get/
  exists/verify`、原子发布顺序（写 tmp → fsync → rename → fsync 目录 → 索引事务）、
  重启恢复与 tmp 清理、崩溃注入测试。需起草 ADR-002（Store 物理布局与事务）。
- **T3**：按 §5 落地路线 A 命名空间；`filesystem-root`（role 8）的值布局需在 T3 的
  ADR 中定死（本 ADR 只预留编号）。
