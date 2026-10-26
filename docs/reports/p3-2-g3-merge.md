# P3-2 G3 收口报告（profile v3 —— inode record 升级）

- 合并范围：`p3-2/g4` 22 个提交（G3 全部 + 三处待补 + open_any）+ architect 三分支（`732be8f`/`dbae733`/`b921bc3`）。
- 前置：`84bbd1f`（G4+P3-3a 合并报告与判据纪律之后的文档状态）。
- 全量：**544 passed / 0 failed / 9 ignored**（QA 独立复跑一致），`clippy --workspace --all-targets` **0**。
- 验收：QA 七条清单全过（本报告 §3）；判据 4 带一项未达标红值出档（§4）。

## 1. 交付了什么

| 层 | 内容 |
| --- | --- |
| **身份** | `Profile::V3`（描述串 = ADR 逐字、id = BLAKE3(串)、**v2 id 不变**）；`inode_fmt` 进 Profile（fmt 分配是 profile 的一部分） |
| **记录** | `inode_record_v3` 编解码：**字节级布局断言**、派生位不落盘（DIRECTORY/NORMAL ⇒ Malformed）、`security_len ≤ 65536` 含等号、**校验写读都跑** |
| **页** | v3 记录接进 page builder + inode 页 fmt=2 **同一次提交**（半 v3 在结构上写不出来）；fmt 矩阵 5 例（第 0 条自证当场抓夹具的错） |
| **反向拒绝** | v2 真夹具 ⇒ UnsupportedProfile（连拒两次 + BLAKE3 字节不变）；v1 合成字段；**读历史的 open_any 认全部可读格式** |
| **Stat/SD** | 派生属性（报告完整/存储克制同一设计两面）；`set_attributes` 拒绝派生位（InvalidParameter）；`security(id)` 独立入口；stat 不读 SD；读不更新 accessed（读两次根 id 相同）；注入时钟 |
| **overlay** | schema v2 四列（T1 全列清单当场拦下漏登记；security 内联 BLOB 非 oid）；materialise 对照三件；`failure_for`（InvalidRecord→Internal） |
| **切换** | **`CURRENT = V3`**；24 红里 **22 条是真缺陷**（references_of 按角色选页格式的第三处漏网）而非夹具——修一处 22 条同绿 |
| **P3-3a 三处待补** | N2 变异（容错会吃掉契约违规）；remove_count 恰一次（含零调用侧）；InvalidRecord 折叠收口（过期身份 vs 条目不存在，ADR-009 新段） |

## 2. 关键裁决（本卡期间）

| 裁决 | 内容 |
| --- | --- |
| **判据 1（读放大）** | 经三次读法定稿为 A(≤36 KiB) + B(ratio ≤1.2)：**1 条判据 2 条断言**。根因是格式级（Blob 整对象/32 KiB 块）——B-15 记为"已知格式代价 + 待决取舍"，owner 已定不动格式 |
| **判据 3（树高）** | 改用**根页 height 字节**（产物可验证、口径唯一）；原"height ≤2"是层数口径与页内字节差一；guardrail_hits 移出判据留作报告数据 |
| **升级语义（裁决-P）** | owner 裁 **(a) 拒绝**（未上线不需兼容）：v2 workspace ⇒ `SchemaMismatch`，与读根侧的 `UnsupportedProfile` 是**两个入口两个错误** |
| **SUPERSEDE（裁决-S）** | owner 裁 **(a) 同 FileId** + 总原则"与真实行为一致（含前后）"——G1 修订卡随后（本报告不含翻转） |
| **性能归因** | 456 ms 先查实现（10 万次分配，encode_into 修掉 231 ms）再谈格式——**先怀疑实现再怀疑格式** |

## 3. QA 七条终验（全过）

1. **24 红处置**：产品改动只有一行（references_of 的 fmt→inode_fmt）；测试仅三处翻转（两探针 + CURRENT==V3）；**没有任何断言被洗**；
2. **v2 夹具回归**：全绿（open_any 路径不受 policy 影响——预期形态）；
3. **两探针同翻**：同一 diff 齐（缺一不收）；
4. **四数**：QA 独立复测 1533/16116/3，与翻转基线精确一致；
5. **debug 耗时**：三批读数 225/180/198 ⇒ **区间 180–225 跨 224 红界**——按未达标带红值出档（见 §4）；
6. **release**：worker 62 ms / QA 96 ms 两台机器，只作记录不互借；
7. **拒绝规格**：typed SchemaMismatch + 字节不动。

## 4. 未达标出档（待 owner 二选一）

> **判据 4（编码耗时）：未达标。** 红界 = v2 基线 187 ms × 1.2 = **224 ms**（debug 同机口径）；v3 实测 **180–225 ms**（三批 5 次中位，波动约 25%，**跨线**）。
> 复现：`cargo test -p fs-snapshot --test g3_baseline -- --ignored --nocapture`。
> release 参考：62/96 ms（无判据，两机记录）。
> **选项**：(i) 调红界——新红界应从重测的**分布**取，不从单次读数取；(ii) 接受波动带并改用 release 口径重新定基。
> QA 附注：波动源可能是 v3 就地写后的机器状态敏感（7642322 前后读数不可比）。

## 5. 判据工程产出（本卡新增，已全部进 `criteria-and-acceptance.md` 第 156–238 条）

- **互斥失败面**（判据 2 的三条断言各有只有它抓的错实现）；
- **就地红法**（close_removes 抽成 fn + 错法对照 ⇒ 变异固化成断言）；
- **第 0 条自证的现场证据**（夹具裸页 vs 信封——拒绝矩阵差点一排假绿）；
- **半 v3 陷阱**（fmt 与记录必须同提交；反半 v3 判据用已知常数 + v2 布局负例）；
- **穷举代替感觉**（20 处 fmt/inode_fmt 逐一列页类型——"没有第四处"是证据不是判断；防错结构两个方向要对称）；
- **事故链三面**（旧观察 → QA 报告 → manager 转发 → worker 四条证据挡住删除——"提删方给当轮证据，执行方核证据"）。

## 6. 下一步

**G1 修订卡**（SUPERSEDE 裁决-S 全套翻转：8 处测试 + ADR 两节 + B-8，architect 草案 `732be8f` 已备）→ **P3-3b**（真后端接 WorkspaceOps；顺手项：读侧 `page_format(profile, role)` 单点化）→ **P3-4**（自写 FFI + 真实挂载；卡已备 `dbae733`，SDK 就位）。
