# P3-4 收口报告（骨架，待写链批完成后填数）

> 状态：骨架。标 `[ ]` 的字段等写链批验收后填实测值；标 `[TBD]` 的等 QA 报告。
> 本文件是 layer-fs 的**项目最终报告**——P3-4 收口即项目收口。

## 0. 一句话

从 spec（workspace CAS 文件系统）到 Windows 侧可挂载、可读写、可与 NTFS 对照的文件系统——
P1/P2/P3 全链闭合。

## 1. P3-4 交付清单

### 1.1 挂载能力
- [ ] 盘符挂载（Z:）：create/mount/start 全零，probe 列出内容
- [ ] 目录挂载：挂载点由 WinFsp 自建（FILE_CREATE + DELETE_ON_CLOSE），卸载后目录消失
- [ ] 只读卷语义：写类操作返回 STATUS_MEDIA_WRITE_PROTECTED（不是"未实现"）
- [ ] 写链：Create(FILE_CREATE)/Write/SetFileSize/Cleanup/Close 真实现 `[TBD]`

### 1.2 回调与结构
- 接口表 64 槽、实测校准链（见 §3 方法论）
- 注册表：一挂载一 Adapter 一 HandleTable；stop→remove→delete + 查无此键断言；miss 计数期望恒零
- FileContext 生命周期：Box::from_raw 只在 Close 一次，按挂载计数配平断言

### 1.3 测试数字
- [ ] 收口时全量：____ passed / 0 failed / ____ ignored（ignored 逐条具名）
- [ ] clippy --workspace --all-targets: 0
- [ ] 不稳定红台账：____ 次，最终状态（留名/仍开放）`[TBD]`

## 2. L2 NTFS 对照（12 类 × 两形态）

- [ ] 三分档分布：一致 ____ / 有据差异 ____（各附证据 ref）/ 缺陷 ____（附复现与归因）
- [ ] 预置口径两条（枚举顺序、属性格式）生效情况
- 报告全文：`[TBD QA 路径]`

## 3. 七环错误链（P3-4 最有价值的过程资产）

| 环 | 码 | 根因 | 修复 |
|---|---|---|---|
| 1 | c0000010 | Overwrite@5 缺席 ⇒ fsop.c:907 类门整类拒绝、不调任何成员 | 接 Overwrite |
| 2 | c0000078 | 自相对 SD 缺 SE_SELF_RELATIVE | control=0x8000 |
| 3 | c0000005 | GSBN 在 size 查询时仍写缓冲 | 两段式（只写 size） |
| 4 | Win32 267 | 夹具把根按文件造 | root = directory kind |
| 5 | c0000010 | 缓冲太小答成"未实现" | STATUS_BUFFER_TOO_SMALL |
| 6 | c0000010 | GetSecurity@16 没接（与 GSBN 是两个操作） | 接 GetSecurity |
| 7 | 空目录 | ReadDirectory 签名不符 + 未写 *PBytesTransferred | 按 ddca7bd 签名改正 |

（另：目录挂载 0xC0000035 = 挂载点已存在，WinFsp 自建——第一天的误判链另述）

## 4. 方法论沉淀（判据 299–355，已并入 criteria-and-acceptance.md §十七）

四大主题：
1. **FFI 探针方法论**（326/330/331/333/336/339）：位用探针、形用裸值、头文件只当目录；
   门禁在成员之前；顺序依赖与 teardown 陷阱；夹具 shape。
2. **证据与观测**（301/304/309/328/335/337/347/352/354）：码没变来源变了；状态码 vs OS 读数；
   错误链剥洋葱；指纹带时间窗与归属；未注册返回 None 绝不编答案。
3. **注入与改动安全**（303/323/348/353/355）：结构靠写不靠 patch；注入三道防线
   （grep 特征→断言→红法）；死代码是诚实的中间态、回退是撒谎的中间态。
4. **挂载语义**（341/349）：挂载点必须不存在；先接类门让错误码说真话。

## 5. 遗留与去向

- B-17 不稳定红诊断：先窗口后专项（owner 已裁）——写链批合入后 nextest 首跑为窗口 `[TBD 结果]`
- 判据 4 耗时口径：改 release（owner 已裁 ii）——本报告数字以 release 为准
- B-1..B-16 backlog 状态表 `[TBD]`
- U1–U14 消项表 `[TBD]`

## 6. 引用

- 合并报告：p3-2-g1/g2/g3/g4-p33a/p3-3b/g1-supersede-a（docs/reports/）
- QA 验收：p34-readchain-acceptance.md + 写链批验收 `[TBD]`
- ADR-005/009；判据全文 criteria-and-acceptance.md（§十七 为 P3-4 期）
