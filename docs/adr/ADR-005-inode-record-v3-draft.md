# ADR-005 修订草案：inode record v3（P3 元数据）

- 状态：**Accepted（设计定稿，2026-10-25）**。P3-2 实现前生效；实现完成后另做一次落地核对（与 ADR-008 同做法），
  核对结论记入 `docs/reports/`，并在本行补"落地核对完成 <日期/提交>"。
- 日期：2026-10-25
- 依据：ADR-005 §3.2（inode 记录）/§5（profile）/§7（解码拒绝）/§10（未实现清单）、ADR-009 §10.1（owner 选 A：profile v3）、
  spec §2 P3（时间戳、ACL/安全描述符、Windows 属性）

---

## 1. 结论摘要

| # | 决策 |
| --- | --- |
| V1 | **snapshot profile v3**；根值格式不变；**inode 表页 `fmt = 2`**（目录/extent 页仍 `fmt = 1`）。旧 profile（v1/v2）根一律 `UnsupportedProfile`，不迁移（与 ADR-008 schema v3 拒绝同策）。 |
| V2 | inode record 增加：`accessed_ns`、`changed_ns`（Unix ns，i64）、`attributes`（u32，Windows 属性子集）、`security`（0–64 KiB 自相对 SD；**0 = 无显式 SD/默认**）。 |
| V3 | 属性只存**可写位**：`READONLY(0x1) / HIDDEN(0x2) / SYSTEM(0x4) / ARCHIVE(0x20)`；`DIRECTORY(0x10)` 由 kind 派生、`NORMAL(0x80)` 由"无其它位"派生，**都不落盘**；非法位拒绝。 |
| V4 | P3 **只写 0 长度的 security**（SetSecurity 仍 `STATUS_NOT_SUPPORTED`）；字段先占位，未来启用 ACL 写入**不再改格式**。 |
| V5 | **access time 不由读操作自动更新**（与现行 Windows 默认一致，避免每次读都产生新根）；只有显式 `SetFileTime` 才写。`changed_ns` 在任何 inode 元数据/内容变更时更新。 |
| V6 | `fs_namespace::Stat` **新增字段**（`accessed_ns`、`changed_ns`、`attributes`）；SD 不进 `Stat`，另开 `Workspace::security(FileId) -> Option<Vec<u8>>`（避免每次 stat 拷 SD）。 |

---

## 2. 编码（ADR-005 §3.2 的替换）

### 2.1 InodeTable 页

```text
叶：fmt=2 | node=0 | u32 count | count × inode_record_v3
分支：fmt=2 | node=1 | u8 height(≥1) | u32 count | count × (u64 first_serial | oid child | u64 subtree_count)
```

- 只有 inode 表页的 `fmt` 变 2；目录（5/6）、extent（3/4）、root（8）仍 `fmt = 1`（布局未变）。
- 解码：profile v3 要求 inode 表页 `fmt == 2`；其它 profile 要求 `fmt == 1`。不符 → `UnsupportedFormat`（不是 `Malformed`：是版本差异，不是损坏）。

### 2.2 `inode_record_v3`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `serial` | u64 | 键，页内严格递增（不变） |
| `kind` | u8 | 1 = Directory，2 = File（不变） |
| `size` | u64 | 逻辑大小；目录必须 0（不变） |
| `created_ns` | i64 | Unix epoch 纳秒（不变） |
| `modified_ns` | i64 | 最后写内容时间（不变） |
| `accessed_ns` | i64 | 最后访问时间；P3 只在显式设置时写 |
| `changed_ns` | i64 | 元数据变更时间（NTFS change time） |
| `attributes` | u32 | 见 §3；只存可写位 |
| `security_len` | u32 | 0..=65536；0 = 无显式 SD |
| `security` | bytes | `security_len` 字节的自相对 SD；P3 恒为 0 长度 |
| `content_tag` | u8 | 0=Empty,1=Blob,2=Extents,3=Dir（不变） |
| `content_oid` | oid | 仅 tag ∈ {1,2,3} 时存在（不变） |

- 全大端、定长；`security_len` 的字节紧接其后，再是 `content_tag`。
- 单条记录的最小增长：+8+8+4+4 = 24 字节（security 为空时）。
- 页容量：`max_page_bytes` 不变（16384）；记录变大 → 每页条目略减。历史无关性/局部性规则不变（ADR-005 §4 只看键）。

### 2.3 profile v3 描述串（`profile_id = BLAKE3(串)`）

```text
wcas/snapshot/v3;fmt=1;inode_fmt=2;mask_bits=6;min_entries=4;max_page_bytes=16384;
cdc=fastcdc-nc2;cdc_min=8192;cdc_avg=16384;cdc_max=32768;gear=blake3:wcas/gear/v1;
content_role=chunk;max_object_value=16777216;name_max=255;hash=blake3;domains=inode,dir,extent
```

（根值的 `fmt` 仍为 1；`inode_fmt=2` 是本 profile 的新参数。）

---

## 3. 属性字

| 位 | 名称 | 存储 | 语义 |
| --- | --- | --- | --- |
| 0x0001 | READONLY | 是 | 前端拒写/拒删（Windows 层强制；核心 API 不解释属性） |
| 0x0002 | HIDDEN | 是 | 仅报告；不改变枚举（Windows 默认也不隐藏） |
| 0x0004 | SYSTEM | 是 | 仅报告 |
| 0x0010 | DIRECTORY | 否（由 kind 派生） | 置了 → 解码拒绝 |
| 0x0020 | ARCHIVE | 是 | 内容变更时置位、备份工具负责清除 |
| 0x0080 | NORMAL | 否（派生） | 置了 → 解码拒绝；报告时"无其它位"才返回 NORMAL |
| 其它 | — | 否 | 解码拒绝（`Malformed`） |

- 目录的 READONLY 无意义（Windows 忽略）；存储不禁止，前端不据此拒写目录项。
- 属性变更 → `changed_ns` 更新（由写入方保证）。

---

## 4. 时间语义

1. 四个时间都是 **Unix epoch 纳秒 i64**；可表示区间约为 **1677-09-21 – 2262-04-11**。前端把 FILETIME（1601 起 100ns，u64）转换：
   - **1601 本身不可表示**（1601→1970 的偏移约 1.164e19 ns，超过 i64::MAX 9.223e18），转换必须用 **i128 中间值**，先算 `(tick * 100 - OFFSET_1601_TO_1970_NS)` 再检查落在 i64 范围；
   - 落在 i64 范围外（含 FILETIME 0 / 1677 以前 / 2262 以后）→ `STATUS_INVALID_PARAMETER`，**不截断、不夹取、不静默回绕**；
   - 精度按 100ns 对齐（Windows 语义），小于 100ns 的余数向下取整并记录在测试中。
2. **写内容**（write/truncate/overwrite）→ `modified_ns = changed_ns = now`。
3. **元数据变更**（属性、时间设置、rename 之外的 inode 变更）→ `changed_ns = now`。
4. **读** → 不改 `accessed_ns`（策略见 V5）；显式 `SetFileTime` 可写任意一项。
5. **rename**：只改目录绑定，不改 inode；`changed_ns` 不变（与 NTFS 的差异记录在 ADR-009 兼容性表）。
6. 不要求时间有序（created ≤ modified 等不做校验；NTFS 也不保证）。

---

## 5. 安全描述符

- `security_len == 0`：无显式 SD。前端返回 `STATUS_SUCCESS` 且 `SecurityDescriptorSize = 0`，由 OS 使用默认安全（P3 的 W10）。
- `security_len > 0`：必须是**自相对** SD 的原始字节；解码只校验长度上限（65536）与非空，不解析内容（解析归前端/未来 ACL 卡）。
- P3 写路径恒写 0；读取路径把非 0 原样返回。将来启用 ACL 写入时**不改格式**，只改写入方与前端策略。

---

## 6. 解码拒绝（§7 扩展）

除现有 §7 的规则外，v3 新增：

| 情形 | 结果 |
| --- | --- |
| profile v3 但 inode 表页 `fmt != 2` | `UnsupportedFormat` |
| profile v1/v2 但 inode 表页 `fmt != 1` | `UnsupportedFormat` |
| `attributes & !WRITABLE_MASK != 0`，或含 DIRECTORY/NORMAL 位 | `Malformed` |
| `security_len > 65536` 或 `security_len > 剩余字节` | `Malformed` |
| 非 0 `security` 但内容为空/长度不符 | `Malformed`（长度已覆盖；不解析内容） |
| 时间字段任意 i64 值 | 接受（不校验范围；前端负责 FILETIME 转换） |

`verify --deep` 复用同一解码，因此对 v3 根自动获得上述检查。

---

## 7. 公开面兼容

| 面 | 变化 | 兼容策略 |
| --- | --- | --- |
| `fs_namespace::InodeRecord` | +4 字段（`accessed_ns`、`changed_ns`、`attributes`、`security: Vec<u8>`） | 内部类型；逐点更新 |
| `fs_namespace::Stat` | +`accessed_ns`、`changed_ns`、`attributes` | **破坏性**（字面量构造）；项目内无外部用户，随 P3-2 一起改；`Entry` 不变 |
| `fs_namespace::FileId` / `Entry` | 不变 | — |
| `fs_api::Workspace` | +`security(id) -> ApiResult<Option<Vec<u8>>>`；`set_times`/`set_attributes`（P3-2） | 纯新增 |
| snapshot profile | `Profile::CURRENT` 变 v3 | 旧根 `UnsupportedProfile`；`Profile::V2` 仅测试夹具保留 |
| workspace_policy | `snapshot_profile` 写 v3 | 旧 workspace（v2）打开即 `UnsupportedProfile`（owner 已接受零部署） |

---

## 8. 旧库拒绝判据（照 ADR-008 schema v3 的先例）

1. **真实 v2 夹具**：用当前 v2 构建生成一个含文件的 store（冻结为测试夹具/字节），用 v3 构建打开：
   - 返回 `UnsupportedProfile { found: <v2 id> }`；
   - 拒绝前后 `store.sqlite` + `-wal` 的字节哈希不变（`-shm` 不计，理由同 ADR-008 §6.1 度量口径）；
   - 连续两次打开都拒绝（无半迁移）。
2. **v1 夹具：不可构造**。P1 写路径已删除，当前仓库无法生成真实 v1 store；只能用**合成 `profile_id` 的根**验证"profile 不符即拒"的同一代码路径，并在测试名/注释里标注"合成，不是真实旧库"。**真实旧库的拒绝证明由 v2 夹具承担**；不得把合成夹具写成"v1 旧库已验证"。
3. **正向**：v3 根往返；新写入的根 `profile_id == BLAKE3(§2.3 串)`。
4. **格式内自识别**：把 v3 根的 inode 页 `fmt` 改成 1 → `UnsupportedFormat`（证明不是只靠 profile）。
5. **前向**：v2 读取器读 v3 根 → `UnsupportedProfile`（用旧构建/夹具验证一次）。

---

## 9. 测试清单

1. inode record v3 往返（四种 content；0/非 0 security；属性边界）；
2. 属性非法位、security 长度越界、fmt 不符各一例拒绝；
3. 时间字段极值（i64::MIN/MAX、0）往返；
4. 历史无关性：同一集合随机顺序 1000 次 → 同一根 id（现有测试扩展到 v3）；
5. 局部性：10 万 inode 改一条属性 → 新增页 ≤ 树深 + 2（现有测试扩展）；
6. 旧 v2/v1 夹具拒绝矩阵（§8）；
7. `verify --deep` 对 v3 干净根通过、对 §6 每类损坏指名失败；
8. 前端转换：**1601 不可表示 → 拒绝**；1970/2262 边界；i128 中间值防溢出；100ns 向下取整；越界不夹取。

---

## 修订记录

- 2026-10-25：Proposed → **Accepted（设计定稿）**；同时把 §8.2 的 v1 夹具从"若保留则同样拒绝"改为"不可构造，只能合成 profile 字段验证同一拒绝路径"。判据影响：无（P3-2 尚未开工；v1 夹具项从"未覆盖"改为"合成覆盖 + 明示局限"，真实旧库由 v2 夹具证明）。

## 10. 风险与遗留

1. **元数据进入树内容**：改 atime/属性会产生新根、新 commit（ADR-005 §11.2 已声明的既有性质）。P3 用"读不更新 atime"避免读操作放大；
2. **记录变大**：每 inode +24 B（无 SD），页容量略降；10 万 inode 的树高与写入放大需在 P3 基线重测；
3. **SD 语义未启用**：P3 只存 0，`SetSecurity` 拒绝；ACL 强制留 backlog（ADR-009 W10）；
4. **旧库零迁移**：owner 已接受；若未来有真实部署，需要另立迁移 ADR；
5. **目录 rename 的 change time**：不更新（与 NTFS 不同），ADR-009 兼容性表记录。
