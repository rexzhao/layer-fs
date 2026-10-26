# P3-2 任务卡（核心补口：G1 原子 disposition / G2 按 FileId 读路径 / G3 profile v3 / G4 条件 rename）

> 状态：**已派（v4，2026-10-25）**。已吸收：QA 判据预审三处、worker 四条前置事实、QA 的 §8 实测数字。ADR-005 v3 已 Accepted（main `4988fd6`）。依据：ADR-009 §9（G1–G4）、ADR-005 inode record v3 草案（main `101a52e`）、
> ADR-003 R3–R7、ADR-008 schema v3 的先例、P3-0c 真实 v2 夹具（main `a846a46`）。
> **本卡不需要 WinFsp SDK**；ADR-005 需先转 Accepted（architect）。

## 0. 为什么四项一起做

adapter（P3-3a）需要它们才能不是 TOCTOU、才能保证 rename 后旧句柄有效：**G1 让"存在性判定 + 动作"原子；G2 让句柄按 FileId
而不是路径工作；G4 让 `ReplaceIfExists=false` 不再 stat-then-rename；G3 让时间/属性/SD 随 commit 走**（owner 已选 v3）。

## 1. G1：按 NT disposition 的原子 open/create

在**一个 overlay 事务**内完成"存在性判定 + 动作"（ADR-009 §4.2 的 6 disposition × 存在/不存在 × 文件/目录）。

- 入口：`create_or_open(path, disposition, ...) -> Result<Opened, Failure>`；
- 语义：`CREATE_NEW` 已存在 → `NameCollision`；`OPEN` 不存在 → `LeafMissing`；`OVERWRITE_IF` 存在则截断、否则创建；
  `SUPERSEDE` 存在则**替换条目**；`OPEN_IF` 两者皆可；`OVERWRITE` 不存在 → `LeafMissing`；
- **`OPEN` 命中目录**：返回 `Opened(Directory)`，**不产生文件句柄**（6 个目录格都依赖这条）。

**判据**

1. **18 格全表**，每格断言三列：`Action`、`Outcome`、**`FileId` 期望**（`SUPERSEDE` → **新 FileId**；`OVERWRITE_IF` → **保留原 FileId**）。
   理由：两者对外的唯一差别就是 FileId，缺这一列则两格从外部无法区分（QA 预审）。表长 == 6×3，rows/columns 分开断言、缺格即红；
2. **并发 `CREATE_NEW`**：`Barrier` 同时起跑，**20–50 轮**，每轮断言"**恰好一个成功、另一个 `NameCollision`**"（不是"至少一个"），
   事后 `verify` 干净；
3. **原子性的负对照**：测试侧自己写"stat 后 create"的两步版，在同一栅栏用例下必须**出现两个成功**——否则"原子"只是声明。

## 2. G2：按 FileId 的读路径

`stat/read_at/write_at/truncate/remove` 接受 **FileId**（inode serial），不是路径。

**同批补的两个入口（2026-10-26，architect 在 P3-3a 评审时提出的缺面）**

- **`read_dir(dir_id)`**：目录句柄在 rename 之后必须能继续枚举，所以入口只能是 FileId（`fs-api` 目前只有 `read_dir(path)`）。
  判据：① rename 后用**旧 dir_id** 仍能枚举出同样集合；② 枚举顺序**稳定**（同一目录两次枚举顺序相同；顺序由折叠键 + 二级键决定，
  见 P3-3a 卡 §3）；③ 目录被 `remove` 后再枚举得到 typed 错误，不是 panic。
- **`remove_at(parent, name, expected_id)`**：两段删除时最后一个 `close` 调它（W7）。
  **签名取 B 形状且 `expected_id` 必带**（manager 裁决，2026-10-26；architect 未回，按 QA 的不对称性分析定案）：

  > **两个签名不对称**：B 形状（`parent, name, expected_id`）**不封死** A —— 有了反向索引后，实现可以**忽略** `name`、
  > 直接用 `expected_id` 定位；反之 A 形状（只有 `id`）**封死** B，因为核心拿不到 `(parent, name)`。
  > ⇒ **先取 B 形状把取舍下推到实现内部**；ADR 复核后若改 A，**trait 签名不动**，只改实现。
  > 实现暂定 B：定位键 `(parent, name)` 只是**提示不是身份**，必须校验 `entry.child == expected_id`。

**守卫失败的三种结果（2026-10-26 定，《implementation 先行》）：**

| 情形 | 返回 | 理由 |
| --- | --- | --- |
| 名字现在属于**别的条目** | `InvalidRecord { field: "remove_at(expected_id)" }` | 这是**绑定过期**（内部不变量被违反），不是“找不到”；adapter 应记录并跳过 |
| 名字已消失，且 `expected_id` **已是墓碑** | `Ok(())` | **幂等**：崩溃后的重复清理不该变成错误 |
| 名字已消失，`expected_id` 仍活着 | `NotFound` | 真正的“找不到” |

（原卡写“不符 ⇒ `NotFound`”，已被上面细分取代；worker 在 `efc618b` 先实现，我按“实现给出了更好的区分”采纳并回写卡与 ADR。）

  **A 的可行性未评估（记一行，别留白）**：已知其代价至少 **O(#目录)**——`base` 树里的绑定不在 `ov_entry` 索引里，
  反查要么全树扫描、要么对每个目录问一遍。若 P3-3b 之前评估为“可实现”，则按 A 实现，**接口不变**（这正是取 B 形状的理由）。

  **依赖前提**：`(parent, name)` 由 adapter 维护，成立的前提是 **W2（workspace 排他打开）**——见 backlog **B-14**（W2 目前只有叙述，没有判据）。
  失效模式是**安全但功能坏**：校验不过 ⇒ `NotFound`，删不掉而不是误删。

  判据：① 幂等（再调一次不报错、无副作用）；② **恰好一次**真正删除（计数 source）；
  ③ **rename 后仍删对了对象**，且**路径复用时不得误删新对象**（§3.9 的招法：`/a→/b` 后重建 `/a` 指向 id2，`remove_at(…, id1)` 不得删 id2）。

**判据**

1. **范围读只触及覆盖帧**（2026-10-26 按实测收窄）：观测点在 pack 读取器 `read_frame`（不是 `Store::get` 返回处）；
   实测可判的形式是
   - **下界**：`bytes_read >= object_len`（内容帧必读）；
   - **上界**：`bytes_read <= 2 × object_len`（**索引帧 + 内容帧**；pack 里同一帧可能被读两次，实测 1851 B / 1607 B 的 pack 即此情形）；
   - **正对照**：再读一次，计数器必须继续增长（证明观测点接上了）。

   **fixture 必须多 chunk，否则这条判据没有判别力**（2026-10-26，QA 实测）：单 chunk 文件（1 KiB）时“读整个文件”与“只读覆盖帧”是同一件事，
   上界抓不到放大。**多 chunk 实测（`read_at_id(id, 0, 512)`，计数器）**：8 KiB 文件 **9,019 B（17.6×）**、16 KiB **17,211 B（33.6×）**、
   64 KiB **33,976 B（66.4×）**；8 KiB 那条**比整个文件还多 827 B**（索引帧），且**不随请求收缩** ⇒ 这是**真读放大**，
   不是“文档化代价”。判据形态：**64 KiB 文件 + 请求 512 B ⇒ `pack_read_bytes() ≤ 24 KiB`**（覆盖帧 + 索引余量）。
   报告若要用更大的界，必须给**机制推导**的数，不得用“2 × 请求长度”这种与机制无关的数。

   **两段阈值 + 差分判据（2026-10-26，QA 指出 24 KiB 容得下“改一半”）**

   由实测反推的**理想形态** = 1 个覆盖 chunk（≈4,110 B）+ 1 个 extent 索引叶（≈1,100 B）≈ **5.2 KiB**。

   | 段 | 判据 | 现状 |
   | --- | --- | --- |
   | 第一段（本次） | 64 KiB 文件、512 B 请求 ⇒ `≤ 24 KiB` | 现测 33,976 B ⇒ **红**（先能红） |
   | 第二段（同卡内，实现改完后收紧） | 同一请求 ⇒ `≤ 8 KiB` | 目标值（1.5× 理想） |

   **差分判据（真正对准缺陷本体）**：缺陷叫“**不随请求收缩**”，所以要比两次不同请求：

   ```
   b512  = pack_read_bytes() after read_at_id(id, 0, 512)
   b16k  = pack_read_bytes() after read_at_id(id, 0, 16*1024)
   assert!(b16k - b512 >= 3 * 4096);   // 请求多 15 KiB ⇒ 必须多读 ≥ 3 个 chunk
   assert!(b512 <= b16k);
   ```

   当前实现两次都按整对象取 ⇒ 差 ≈ 0 ⇒ **红**（准确命中原缺陷）；改成“只读覆盖帧”后差 ≈ 12 KiB ⇒ 绿。
   **只给绝对上限证明不了“随请求收缩”**——24 KiB 容得下“读 4 个 chunk（≈17.5 KiB）”这种“改一半”。

   **状态（2026-10-26 收口）：三条全部 `未达标`，不改成松口径。** 实测 = 64 KiB 文件、512 B 请求 ⇒ **33,976 B**；
   `b16k − b512 ≈ 0`（两次都按整对象取）。

   **根因是格式级，不是实现疏忽（worker 查证，`e3328f4`）：**
   - **overlay 侧**已经是页级裁剪的：`content.rs` 的 `read_at_impl` 只遍历 `first_page..=last_page`（512 B 请求不放大）；
   - **base（已提交内容）侧是 chunk 级**：`base.rs` 的 `read_at` 对 `Content::Extents` 走 `extents_in_range(...)`，
     每个 extent 调 `store.get(extent.object)` —— **拷贝的是区间那几字节，但读取单位是一整个 chunk 对象**。
   - 因此成本 = 覆盖到的 chunk 数，**与请求长度无关**（512 B 与 8 KiB 落在同一 chunk 时读数相同）——这正是实测形状。

   **为什么不在本卡修**：切小 chunk = 改切块参数 ⇒ 所有 object id 变化 ⇒ profile/format 变更（ADR + 迁移 + 夹具重做）；
   子对象读取（帧内切片）会**拆掉“对象 = 读取单位”这条不变量**（身份校验只能对整帧做）。两者都超出 G2 的范围（G2 是按 FileId 的入口，不是内容布局）。
   **根因（2026-10-26 定案，推翻中间那版“k = 8 个 4 KiB 块”的误读）**：**格式级**。
   - **≤32 KiB 的文件存为单个 `Content::Blob`**（ADR-005 §3.2：blob 的 value 长度 == 文件大小），
     `base.rs:122` 的 `store.get(oid)` **整对象读**，之后才切片 ⇒ 读 512 B 必然解出整对象；
   - **≥64 KiB 的文件按 32 KiB 块取**（extent 粒度）⇒ 读 512 B 付 **1 个 32 KiB 块 + 索引**。
   - 决定性实测（读 512 B）：8 KiB→9,019（≈文件本身）/ 16 KiB→17,211（≈文件本身）/ 32 KiB→33,595（≈1 块）/
     64 KiB→33,976 / 128 KiB→34,200 / 256 KiB→34,648 ⇒ **三组数全部对上“整对象 / 一块 + 索引”**。

   （中间曾据 33,976 ≈ 8 × 4,110 推断“k = min(8, chunk 数)”并怀疑读路径有 bug——**那是数值巧合，机制判断错了**，已作废。）

   **由此判据要改（原 ≤24 KiB 不可达成：下限本身就是 32 KiB）**：
   - **A（单文件）**：64 KiB 文件读 512 B ⇒ `≤ 36 KiB`（现 33,976 ⇒ **绿**；验证“只取一个块”，这是格式允许的最好结果）；
   - **B（跨文件大小，与格式无关的那部分）**：**256 KiB 文件读 512 B 的读数 ≤ 32 KiB 文件读 512 B 的读数 × 1.2**
     （现 34,648 / 33,595 = **1.03** ⇒ 绿）。它**不假设块大小**，只断言“读 512 B 的成本不随文件变大而变大”；
     将来若有人把块取成整文件，它会红。

   ⇒ 记 **B-15**，由 owner 决定是否值得改格式；**不作假修**（把 `store.get` 换成 `read_frame` + 切片不会减少读取字节，只让代码看起来更精细）。



   **原卡写的“单对象读 == 恰好一帧”与“不触及 48 B 尾部”在本 API 下不可判**（`read_at_id` 是**文件区间**读，不暴露 chunk 级读；
   接触尾部的检查需要 pack 尺寸比对，而帧可被读两次使其失效）。⇒ 这两条**记为未验证**，理由与实测数字写在
   `g2_read_side.rs` 的注释里；报告必须原样注明“尾部未触及”**未被验证**，不得写成已验。
2. **计数 source + 正反两侧读数**（ADR-009 §9 的写法，不用拓扑形状）：同一 FileId 连读两次 → 每次都真正取对象（计数 +1）；
   共享子树在遍历路径上只取一次；
3. **rename 后 FileId 解析仍指向同一对象（L1 代理）**：本卡只证明"解析不受路径变化影响"；
   **真正持句柄跨 rename 属 adapter 的 L2**（P3-3a），这里写明，避免被读成"G2 证明了 Windows 句柄语义"；
4. 跨重启（同一 commit）同一 FileId 仍能定位同一对象。

**①（读放大）与 ④（跨重启）的夹具前提（worker 交 `6618716` 时说明；记为缺口，不是“忘了”）**

| 判据 | 缺在第几步 | 缺什么 |
| --- | --- | --- |
| ① 读放大 | **能力（夹具）** | 需要“同一时刻只持有一个 `Workspace` 句柄”的夹具（第二个句柄 `Busy`），**且内容必须真的在 pack 里**——未提交的字节在 overlay 页时，计数会因**错误的原因**保持 0（假绿） |
| ④ 跨重启 | **能力（夹具）** | 需要一个已沉淀 commit 的 store，才能“重开 + 同一 FileId 解析到同一对象” |

**纪律**：**不得为了迁就现有夹具而放宽断言**——先落夹具、再落判据；缺口按四步表写进这张卡，不写在邮件里。

**worker 交 `a743147` 时的原文（已按四步表落档）**

> 状态：**未实现**（不是“已覆盖”）。阻塞点在第 2 步（夹具），不是在第 4 步（判据）；判据本身已明确，无需再裁定。
>
> - **判据 1（读放大）缺在第 2 步：夹具形态。** 需要“同一时刻只持有一个 `Workspace` 句柄”的夹具（第二个句柄会 `Busy`），且内容必须**已提交**（未提交字节在 overlay 页，`pack_read_bytes` 会因错误原因保持 0）。现有 `g2_read_side` 夹具同时持有 `Workspace` 与 `Repo`，两条都不满足；**不放宽断言去迁就夹具**，故未交付。
> - **判据 4（跨重启）缺在第 2 步：同一原因**（重开 `Repo` 前必须释放前一个句柄，现有夹具在测试体内无法释放）。
> - 证据位置：`crates/fs-workspace/tests/g2_read_side.rs` 末尾的 `read amplification / restart: fixture restructure pending` 注释块。
> - 计划：先落删除侧（`remove_at` tombstone），再用“一次只开一个句柄”的夹具补这两条，同批合入。


**归属说明**：`delete-pending` 期间"用 FileId 继续读"属 **adapter/模型层**（P3-1 的 `delete` 模块已建模），**不在 G2**；
存储层不持久 delete-pending（与 Windows 语义一致：重启后 pending 消失）。这条写进卡以免 G2/G3 互相越界。

## 3. G3：profile v3（inode record 升级）

按 ADR-005 v3 草案：inode 表页 `fmt=2`，新增 `accessed_ns`、`changed_ns`、`attributes`、`security_len` + security bytes。

- **字段顺序与 `security_len` 上限 65536 按草案，不得自行调整**；属性只存 `READONLY/HIDDEN/SYSTEM/ARCHIVE`，
  `DIRECTORY/NORMAL` 派生、落盘即解码拒绝，非法位拒绝；
- **`Stat` 破坏性变更一次性改完**（加三个字段，不保留旧构造）；
- **v2 读取路径不得因 v3 被删**（拒绝判据与兼容读取不得打架）。

### 3.1 接受侧：fmt / 字段矩阵（必须逐行有测试）

| 情形 | 期望 |
| --- | --- |
| v3 根 → inode 页 `fmt=2`；目录/extent/root 页仍 `fmt=1` | 往返成功 |
| profile v3 + inode 页 `fmt=1` | `UnsupportedFormat`（**不是** `Malformed`） |
| profile v2 + inode 页 `fmt=2` | `UnsupportedFormat` |
| v3 根 + 目录/extent 页 `fmt=2` | `UnsupportedFormat` |
| `attributes` 含 `DIRECTORY(0x10)`/`NORMAL(0x80)` 或未知位 | `Malformed` |
| `security_len = 0 / 1 / 65535 / 65536 / 65537` | 前四接受（0 = 默认 SD），65537 拒绝；非零 SD 字节原样往返 |
| 四个时间 `i64::MIN/MAX/0` | 往返成功；不校验时间顺序 |
| `profile_id != BLAKE3(§2.3 描述串)` | `UnsupportedProfile` |
| `Profile::CURRENT.id()` | == BLAKE3(描述串)；v2 id 保持不变（夹具仍可读） |

**`Stat` 兼容判据**
- 编译期：旧 `Stat { .. }` 字面量构造在所有 crate 里编译失败（CI 即可，不需要运行测试）；
- 运行期：`Stat` 含 `accessed_ns/changed_ns/attributes`，**不含 security**；`Workspace::security(id)` 单独返回 `Option<Vec<u8>>`；
- `stat` 的读放大：计数 source 断言它只读 inode 记录，不读 SD 字节（不把 SD 复制进每次 stat）。

**时间/属性更新语义**
- `write` / `truncate` → `modified_ns` 与 `changed_ns` 都更新；`set_times` / `set_attributes` → `changed_ns` 更新；
- **读不更新 `accessed_ns`**：读前后 root id 不变（否则每次读都产生新根）；
- 用**注入时钟**测，不用墙钟。

### 3.2 拒绝侧：旧根/旧 workspace

1. **v2：真实夹具**（`crates/fs-snapshot/tests/fixtures/snapshot-v2-store/`）→ `UnsupportedProfile`；README 的字节清单作外部事实；
2. **v1：只对合成的 profile 字段断言** `UnsupportedProfile`，并在卡里标注"**合成根只测字段识别，不证明历史兼容**"
   （本 build 造不出真 v1，P3-0c 已证明）；
3. **拒绝前后 `store.sqlite` + `-wal` 字节哈希不变**（`-shm` 不计）；**同一夹具连续两次都被拒**；
4. **fmt 自识别**：把 v3 根的 inode fmt 改回 1 → `UnsupportedFormat`（证明不是只靠 profile 串）；
5. **反向（v3 → 旧读取器）**：v3 根 + 强制 `Profile::V2` 读取 → `UnsupportedProfile`（同一 build 的 L1 代理，**标注"不是真实旧二进制"**）；
   另立**发布前手工项**：用 v2 构建产物打开 v3 夹具，必须拒绝且字节不变（结果贴报告，不在常规 CI）；
6. **workspace 侧**：`workspace_policy.snapshot_profile` 仍为 v2 的 workspace → `workspace_open` 必须 typed 拒绝，且无任何写入；
7. **v2 正读**：v2 夹具 + `Profile::V2` 能读出目录与文件内容（不能只有"被拒"用例）。

### 3.3 基线与阈值（**修正：原 `24/4096` 式作废**）

**原式作废的理由**：
- 分母用错：`24/4096` 用了 overlay 的 `PAGE_BYTES=4096`，而 snapshot 页是 `max_page_bytes=16384`；
- 机制不对：ADR-005 §4 的分页由**键哈希 + `mask_bits`** 驱动（平均约 64 条/页），`floor(PAGE_BYTES/inode)` 只是
  `max_page_bytes` 的**字节护栏**，不是"按容量打包"的目标。记录 +24 B（无 SD，约 66→90 B）时平均页仍远小于 16384，
  **页数通常不变**；只有某页撞到字节护栏才会提前切。原式既无机制依据，也可能把正确实现判红或放过真实退化。

**基线要求**：**改动前**、同机、同参数（10 万 inode）测，报告带 commit SHA 与机器；记录
`record_old`、`record_new`、`pages_old`、`height_old`、`time_old`、`avg_entries_per_page`、`max_page_bytes` 命中次数。

**判据（2026-10-26 定稿；数值来自 QA 在 `41694d0` 的同机实测基线）**：
1. `pages_new ≤ pages_old × 1.05`（基线 1472 ⇒ 红界 **1546**）。**依据**：容量 244→179（−26.6%），但**只有 2.3% 的页受容量约束**（34/1448），期望 ≈ **+0.9%**；+5% 留 5 倍余量，仍能抓“按字节打包”的真回归（+26%）；
2. **任一 v3 页的字节数 > `max_page_bytes` ⇒ 红**（这是记录膨胀唯一会改变页数的机制；报告触发页数/最满页）；
3. `height_new ≤ height_old + 0`（基线 2 ⇒ 红界 **>2**）。**依据**：24 个 branch 对扇出约 402，远未饱和；
4. `time_new ≤ time_old × 1.2`（基线 **187 ms** 中位（163/164/187/193/195）⇒ 红界 **224 ms**）。**必须同 build 同机**：debug/release 差 5–10 倍；worker 若用 release 测，不得套用此数。

**基线实测（`41694d0`，Windows 10 本地 SSD，debug，99,999 条带 oid 记录）**

| 项 | 值 |
| --- | --- |
| `record_old` | **66 B**（带 oid）／ **34 B**（Empty） |
| `pages_old` | **1472**（leaves 1448 + branches 24） |
| `height_old` | **2** |
| `time_old` | **187 ms** 中位（163/164/187/193/195） |
| 最大页实测 | **16110**（距 `max_page_bytes` 16384 仅 274 B） |
| `max_entries_per_leaf` | **244** |
| `avg_entries_per_page` | **69.06**（v3 预期 ≈ **68.5**） |
| **护栏命中** | **34 / 1448 = 2.3%** ← **“原 24/4096 模型作废”的直接证据** |

**三条执行纪律**：① 基线绑定 `41694d0`；G3 之前若 main 有改动触及**编码器或 profile** ⇒ 必须重测（G2 只改读路径，可复用）；② 判据 4 必须同 build 同机；③ **G3 派发时须再确认 `fs-snapshot` 的 `Profile`/编码器与 `41694d0` 相同**——顺序上 G3 排在 G2→G4→P3-3a 之后，其中 **G4 可能改 inode 记录的构造路径或加字段**（`CreateOutcome`/rename 的返回变化），一旦编码变了就必须重测基线，否则就是“**用旧基线判新编码**”的静默错配。

超出任一阈值 ⇒ 红，并作为**代价问题**上报给 owner（manager 转）。

## 4. G4：带 `replace` 的 rename

`rename(from, to, replace: bool)`，判定与动作在同一 overlay 事务内。

- `replace=true`：目标存在则替换（R3）；跨类型（R4/R5）与非空目录（R6）规则不变；
- `replace=false`：目标存在即 `NameCollision`，**且不得有任何改动**。

**前提（未验证，P3-4 L2 实测）**：上游 Rename 契约写"目标名已有打开句柄时 FSD 不下发 Rename"，但**尚无行为实测**。若实测成立：`replace=true` 的目标无句柄，核心可立即 retire；若实测不成立：核心必须改用 SUPERSEDE 的**保留**语义（目标 live、旧句柄可读），`replaced` 仍返回供 adapter unbound——**unbound 只防误删，不能代替保留**。核心不建模句柄，G4 按当前前提测，并在报告里标明该前提未验证。

**判据**

1. true：替换成功，目标内容为源内容、源消失；
2. false 冲突：返回 `NameCollision` **且指纹不变**——`store/` 的 `(路径, 长度, 哈希)` 清单 + `object_location` 行数 +
   目标父目录 `read_dir` 三者前后相同（"返回冲突"与"没改动"是两件事）；
3. 并发 `replace=false`：`Barrier` + 20–50 轮，每轮"一成一败"；
4. **返回的 `binding` 正确**：`rename(from,to,r)` 成功时 `binding.parent == FileId(parent(to))`、`binding.name == to 的最后一段原始字节`（保留大小写）；用该 `binding` + 源 FileId 调 `remove_at` 必须删到 `to`。反例（任一即红）：返回 `from` 的旧 binding、返回 parent 的路径字符串、返回折叠后的名字。
5. **`replaced` 语义**：`replace=false` 目标不存在 / `rename(x,x)` / 大小写-only 且目标不存在 ⇒ `None`；`replace=true` 且目标存在且是**不同 FileId** ⇒ `Some(old)`，且返回后 `stat_id(old)`/`read_id(old)` 都是 `NotFound`（tombstone），目标名此时解析到源 FileId；`replace=true` 且目标就是源自身（同 FileId）⇒ `None`，**不得**把自己 retire。
6. **目录 rename-replace**：`replace=true` 目标是空目录 ⇒ `replaced=Some(old_dir)`，其子树内各 FileId 不变；目标非空 ⇒ `NotEmpty`、无 `binding/replaced` 返回、§4.1 指纹不变。
7. **大小写-only rename**：`/A → /a` 且 `/a` 不存在 ⇒ `binding.name == b"a"`、`replaced=None`、FileId 不变；`/a` 存在且是另一个 FileId ⇒ `replace=false` 返回 `NameCollision`；`replace=true` 按 R3 替换（这是**核心语义**；adapter 对"折叠名冲突"另有拒绝策略，见 ADR-009 §4.1，G4 不替 adapter 表态）。
8. **失败不产生 outcome**：跨类型（R4/R5）、非空目录（R6）、移入自身子树（R9）、`replace=false` 冲突，一律返回 typed 错误，**不返回** `binding/replaced`，且判据 2 的三项指纹/计数不变。
9. **竞争与 outcome 配对**（扩展判据 3）：20–50 轮中，成功一方的 `binding`/`replaced` 必须与最终目录视图一致；失败一方无 outcome 且无改动；不得出现"返回成功但 binding 指向失败者的名字"。

## 5. 顺带清理（`PACK_TAIL_BYTES`）

- `cas-store` 导出 `pub const PACK_TAIL_BYTES: u64`，文档注明"由 ADR-008 §3 定义"；**不要顺手导出 `PACK_TAIL_MAGIC`**——它当前无测试消费者，导出会变成第二个"无人引用的常量"（假绿风险）；
- 删掉 `cas-store/tests/pack.rs` 与 `tests/sync_probe.rs` 的本地 `const TAIL_BYTES: usize = 48`；
- 只留一处断言 `PACK_TAIL_BYTES == 48`。

### 5.1 落地版判据（2026-10-26，与 `crates/cas-store/tests/tail_constant.rs` 一一对应）

五条检查已从“清单里的 grep”变成**每次 `cargo test` 都跑的检查**（判据不是清单：清单会被改文案绕过）。

| # | 判据 | 说明 |
| --- | --- | --- |
| R1 | 字面量绑定 = 0 | `let/const … = 48` 之类 |
| M2 | 收窄名字版 = 0 | 名字以 `tail`/`_bytes`/`_len`/`_size` 结尾且值含数字（`0x30`、`4+4+8+32` 都算） |
| M1 | 名字白名单 | 名字含 `tail` 且值含数字 ⇒ 必须叫 `{tail, tail_bytes, tail_len, tail_size}` 之一（抓 `tail_bytes2 = 0x30`） |
| F1–F5 | 白名单 | 就地 + 带理由 + **条目数 == 命中数（双向）** + **上限即信号**（第 2 条出现时该消灭知识而不是登记） |
| — | 三级分辨率 | 裸名字 `pub const TABLE` 7 处 / 类型化形状 3 处 / 语意判据 1 处——**每个数都带自己的命令** |

三级分辨率那句原话（QA）：**每提高一级，被误计为“定义”的“提及”就少一类。**

另外两条落地约束：`// tail-allow:` 行内豁免（当前**只许 1 条**：`src/pack.rs` 的 `at + 16..at + 48` 是摘要结束位置，
不是第二份尾长）；`let tail = 4 + 4 + 8 + 32` 这类“算出 48”由 M1 抓。

**验收用语义检查，不用 `grep 48`**（`48` 可能是别的数，会假红/假绿）：

1. `grep -n 'pub use.*PACK_TAIL_BYTES' crates/cas-store/src/lib.rs` = **1 行**；
2. `grep -rn 'const TAIL_BYTES' crates/*/tests/` = **0 行**；
3. `grep -rn 'PACK_TAIL_BYTES == 48' crates/` = **1 行**，且**不是** `assert!(PACK_TAIL_BYTES > 0)` 这类弱断言；
4. `grep -rn 'PACK_TAIL_BYTES' crates/ | wc -l` = **12**（2026-10-26 由 11 改为 12）：
   逐文件 `src/pack.rs` 5、`src/lib.rs` 1、`tests/pack.rs` 4、`tests/sync_probe.rs` 1、**`fs-workspace/tests/g2_read_side.rs` 1**（G2 的读放大上界要用**常量**而不是字面量 48 来界定）。
   这一处新增正是该判据的设计意图：`tail_constant` 的逐文件 pin 表**先红了一次**，逼着新消费点**显式登记**——这正是“绝对数比 ≥ 使用点数硬”的现场演示。
   **绝对数比“≥ 使用点数”硬**：少一处 = 漏改（别名常量还在，仍能编译）；多一处 = 多留副本。

## 6. 判据预审四问（QA，开工前逐条答完）

1. **可构造吗**；2. **可达吗**；3. **它会怎么红**；4. **期望独立吗**。
本卡 v2 已按这四问改过一轮；开工前若 worker 对某条有异议，回卡不自行改口径。

## 7. 交付与验收

- 分支基于当时 main；四步检查；提交说明**首行判据影响行**；
- 四件套 0；全量不回归；P3-0c 夹具用例仍绿；
- 报告：G1 的 18 格（三列）+ 并发与负对照；G2 的计数读数与"L1 代理"声明；G3 的旧根拒绝四项 + 基线原始数据与阈值对照；
  G4 的三条（含指纹）；§5 的四条语义检查；
- QA 按 §6 与逐条"怎么红"验收，`L1/L2/L3` 与"待环境/未验证"分档照 P3 骨架。

## 8. §8 已并入 §11（本节仅留指路）

原“待填（每页 inode 数 `floor(4096 / rec)`）”已删除：inode 表用的是 `max_page_bytes = 16384`（**不是** `PAGE_BYTES = 4096`），
容量为 `ceil((16384 − 311 − 6) / rec)`。**一切以 §11 为准**——若按 `floor(4096/66) = 62` 算，那是把页大小搞错了。

## 9. 前置事实与实现顺序（worker 核对 + QA 复核，2026-10-24）

> **前置**：P3-1 已在 main（`737c646`），`crates/fs-windows` 存在 ⇒ §12 的“表上移”可以开工（`git ls-tree origin/main crates/` 可验）。

1. **`PACK_TAIL_BYTES` 现在是 `pub(crate)`**（`cas-store/src/pack.rs:85` 的 `TAIL_BYTES`）。G2 判据要断言“不触及 48 B 尾部”，
   测试需要它 ⇒ **P3-2 的第一件事**就是把 `cas-store` 的公开面收敛到这里：
   - 导出 `pub const PACK_TAIL_BYTES: u64`，文档注明“由 ADR-008 §3 定义”；**不要顺手导出 `PACK_TAIL_MAGIC`**（当前无测试消费者 = 假绿风险）；
   - 断言 `PACK_TAIL_BYTES == 48` **只在** `cas-store` 一处；
   - 删掉 `tests/pack.rs` / `tests/sync_probe.rs` 的本地 `const TAIL_BYTES`；使用点必须引用常量（§5 的四条语义检查）。
2. **G2 不是从零造类型**：`fs_snapshot::SnapshotReader::inode(serial)`、`root_serial()` 已有（`read.rs:151/160`），
   `fs_namespace::FileId` 已贯穿 `materialize.rs:46`。G2 的工作是**把路径入口换成 serial 入口**，并补齐判据。
3. **G1 复用同一张语义表，不写第二份矩阵**：`fs-windows::disposition::{Disposition, Exists, Action, Outcome, TABLE, decide}`
   已存在（P3-1）。**裁决：把这张表与 `decide` 上移到核心 crate（`fs-namespace`，`fs-api` 再导出），`fs-windows` 改为 re-export**。
   理由：分层方向只能是 adapter 依赖核心（`fs-windows → core`），不能反过来；两份矩阵必然漂移。
   验收：`fs-windows` 里不再有独立的 `TABLE` 定义，只有 `pub use`；P3-1 的 18 格用例仍绿（证明搬运无行为变化）。
4. **FileId 期望列排在 G3 之前**：`SUPERSEDE → 新 FileId`、`OVERWRITE_IF → 保留` 这一列与 `Stat` 字段集合、overlay 的 serial
   分配语义互相牵连 ⇒ **先定 FileId 语义与列，再动 G3**，否则实现顺序会来回改表。

**因此实现顺序**：§9.1（常量收敛）→ G1（含表上移）→ G2（serial 入口 + 读放大）→ G4（条件 rename）→ G3（profile v3 + 基线）。

## 10. 两条字面约束（worker 建议，manager 采纳）

1. **L1 测试不得使用 adapter 的 handle 表**：G2 只证明“FileId 解析在 rename 后稳定”（L1 代理）；若 L1 借用 adapter 的 handle 表，
   等于提前“证明”了 P3-3a 的 L2 判据，变成自证。
2. **旧根拒绝的度量口径**：`-wal` 在**只读打开**时也可能被创建/截断（P3-0c 实测到 `-shm`/`-wal` 会被 SQLite 造出来）。
   度量写成：**`store.sqlite` 与“已存在的 `-wal`”的内容哈希**，并对 `-shm`/`-wal` 的**出现/消失**明确豁免——与 ADR-008 §6.1 一致。

## 11. §8 的数字（QA 实测，非估算；编码器临时插 24 B 跑完已还原）

### 每页 inode 容量

```
guard = max_page_bytes(16384) − max_entry_bytes
max_entry = name_max(255) + 8 + 32 + 8 + 8 = 311  ⇒ guard = 16073
leaf_overhead = 1 + node(1 B) + 4 = **6**（首字节 node 必须计入）
capacity(rec) = ceil((16073 − 5) / rec)
```

| 记录类型 | v2 长 | v2 每页 | v3 长 | v3 每页 | 降幅 |
| --- | --- | --- | --- | --- | --- |
| 有内容（带 32 B oid） | 66 B | **244** | 90 B | **179** | −26.6% |
| 无内容（空文件/空目录） | 34 B | **473** | 58 B | **278** | −41.2% |

**但页数只涨 +4.1%（不是容量降幅）**：`mask_bits = 6` 的哈希切分在**平均 69 条/页**处就切开，远早于字节容量 244 ⇒
**v2 的 inode 表页数由哈希切分主导，不是字节预算**。实测 99,999 inode：leaf 1448 → 1511、总页 1472 → 1533。
因此 **§3 的基线报告必须同时给 `mask_bits` 与"平均每页条数"**，否则 "+4%" 不可解释。

### 基线阈值（10 万 inode、带内容、最坏情况）

| 指标 | 基线（v2 实测） | 红 |
| --- | --- | --- |
| 树高 | 2 | **> 2** |
| leaf 页数 | 1448 | **> 1521**（+5%） |
| 总页数 | 1472 | **> 1546**（+5%） |
| inode 表字节 | ≈ 6.60 MB | **> 6.94 MB**（+5%） |
| **单页最大字节** | **16110**（= 6 + 244×66；≤16384） | **> 16384**（越界即红） |
| 编码耗时（本机 debug） | ≈0.34 s / 99,999 条 | **> 基线 × 1.2** |

**更正**：早前写的 "1 + 24/4096" 是"按 4096 页"的错觉（实际 `max_page_bytes = 16384`、且哈希切分主导），**以本节为准**。

**为什么必须有"单页最大字节"这条**：`encode.rs:106-109` 注释记载第一次 scale run 曾出现 **16440 > 16384**——装填规则真的溢出过。
所以它不是形式条款，而是回归防线：记录变大时同类 bug 会以"+24 B"的形式再现。

### 报告必须给的两个原始数

1. **v3 每页容量实测值**（应为 **179 / 278**）——不符即说明字段布局没按 ADR-005 §2.2；
2. **`mask_bits` + 分析形状**（有内容/无内容各多少条）——否则页数无法解读。

## 12. G1 的表放哪：**`fs-semantics`**（manager 裁决改判，2026-10-26；原方案 `fs-namespace` 作废）

> **改判理由（QA 用一条 `cargo tree` 抓到的）**：`fs-namespace → cas-store → rusqlite(bundled) → cc/pkg-config/vcpkg`
> 共 23 个 crate。若把表搬进 `fs-namespace`，`cargo test -p fs-windows` 就需要 **C 工具链**，且 `fs-windows` README 的
> “no FFI / no SDK / any platform” 三条声明**全部失守**（`libsqlite3-sys` 就是对 C 的 FFI）。
>
> 现方案：新建**零依赖** crate **`fs-semantics`**，只装决策词表；依赖方向 `fs-windows → fs-semantics`、
> `fs-namespace → fs-semantics`、`fs-api` 再导出。实测 `cargo tree -p fs-windows --depth 1` 从 **0 → 1**（只多这一条），
> 无 C 工具链。落地见 `8f71814`。
>
> 边界（同批裁决）：**类型跟表走**（`Disposition`/`Exists`/`Action`/`Outcome` 全进 `fs-semantics`，否则表引用它们会形成
> `fs-semantics → fs-windows` 反向依赖）；**`FileId` 不进**（存储概念，`CreateOutcome { file_id, kind, action }` 留在
> 定义 FileId 的那层）。

- **表与 `decide` 上移到 `fs-namespace`**（`fs-api` 再导出）；`fs-windows` 改为 `pub use`，**不得保留副本**；
- 依赖方向：`fs-windows → fs-namespace`（adapter 依赖核心，方向正确）；**P3-1 README 里 "no deps" 的表述同卡改成
  "依赖核心的语义表、无 FFI、无平台依赖"**（这是事实修正，不是退让）；
- `share`（NT 共享位矩阵）**留在 `fs-windows`**：它只有 adapter 用，核心不需要；
- 验收：`fs-windows` 无独立 `TABLE`（只有 re-export）；P3-1 的 18 格用例仍绿（搬运无行为变化）；`fs-namespace` 的表有
  "缺格即红"基数断言（6×3，rows/columns 分开）。
