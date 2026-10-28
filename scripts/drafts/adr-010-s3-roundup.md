# ADR-010 S3 收口材料（worker 草案，供 manager 改写进 docs/）

## 起点与终点

```
S3 起点：99d7bc6（arch/adr010-s3 卡）
S3 终点：adde6af
全量：732 例中 730 绿（两次跑出的红都是已知族：cas-store::tail_constant 子串误报一次、
      fs-workspace::crash / mounted_write_chain 抖动；各自单跑全绿）
clippy --workspace --all-targets：0
restore_live 12/12 ；admission_q1 8/8 ；admission_handles（Q2）1/1 ；fs-winfsp 149/150
```

## 判据总表（全部真机，读数均先打印后断言）

| # | 判据 | 读数 |
|---|---|---|
| G4a | 深度真的会减（`ReadGuard` drop 减一） | 连续三轮 take/drop 后仍为 0 |
| G4b | 严格模式在每个集成测试里打开 | fs-workspace 135 / fs-api 47 / fs-cli 43+ **零嵌套 panic** |
| G4c | 两个空转的环境变量删除 | 注释与代码一致 |
| Q1 | 准入状态机 7 例 | Closing 立拒；变异把 Closing 改成等待 ⇒ 红且挂 116 s |
| Q2 | 文件/目录句柄分别计数、各自回落 | 文件 1/0 → 目录 1/1 → 回落 0/0 |
| R1 | restore 后读到旧字节（V6） | `/f = "1"`，`status` 的 `root:` 是 c1 的根 |
| R2 | 文件句柄挡住 restore | 等 **10.06 s** 后 exit 4，视图不动，关掉后成功 |
| R3 | 目录句柄放行并报数 | `dirs_invalidated = 24`（修好泄漏后为真实值 1–2） |
| R4 | 准入窗口 | restore 停 3.07 s，打开等 **2.37 s** 后 `NotFound` |
| R5 | 错误 kind 透传 | dirty=5 / unknown=3 / discard=0（改前全是 1） |
| W1 | switch 成功、记录从未 Gone | 19 样本 gone=0 stale=0；`status` 变 feat |
| W2 | 文件句柄挡住 switch | exit 4 + `1 file`；关掉后换挂成功、`handles: 0` |
| W3 | 被占用 ⇒ busy，通道恢复 | exit 4；恢复 **179.6 µs**（上界 100 ms） |
| W4 | 换挂窗口 | **29.6 ms**（上界 2000 ms） |
| W5 | Closing 期间打开立拒 | **149 µs**，实测 `ERROR_NOT_READY (21)` |
| W6 | 6b 回退 | exit 1，回退后 `workspace: main`，内容仍在 |
| W8 | 三种 shell 的 cwd | bash/cmd/powershell 均 exit 0，两段 PHASE 都打印，`cd .` 后可用 |

## 三个结构性改动（比判据重要）

1. **准入（Q1）**：检查与阻止新打开在同一把锁下，覆盖"解析 → 后端 → 登记"全过程。
   Paused 等待、Closing 立拒 —— 两种不同的死锁避免。
2. **发布锁的修复（G4）**：S2 的"零嵌套"是**空转**（开关没人读）。现在 226 例每个都开着它跑过。
3. **switch 的成功位置（W6）**：`Switched` 由**新挂载自己**在卷起来的那一刻回答；
   我第一版放在"下一次 mount_serving 返回后" ⇒ W1 红（等 120 s 超时）——回答的位置必须与事实的发生对齐。

## 修掉的一个真缺陷：句柄泄漏

```
现象：从挂载内每跑一条 fs-cli 命令，句柄表 +8 且永不回落（22 → 78 → …）
定位链：
  1 上下文表其实是空的（live_refs=0）⇒ 泄漏不在上下文
  2 stale_releases 4→9 是**仪器**在说话（Option 同时表示"从未发放"与"还有引用"）
  3 拆成 Freed/StillShared/Unknown 后 stale 恒为 0
  4 真因：每次 open 注册一个新 handle，而上下文只记得第一个 ⇒ N 次打开只关 1 次
修法（裁决 甲，两件事必须一起做）：
  HandleTable 一个条目一份 handle + refs（打开 +1、关闭 −1，最后一个才移除）
  handle_kinds 按 refs 求和 ⇒ 计数仍然表示"打开次数"
验收：同样操作之后 table=(0,0)、status 报 handles: 0
```

## ADR 需要补的三段

1. **§10 管道**：switch 处理期间管道**串行不可应答**（已知行为）；结束后恢复 ≤100 ms（实测 179.6 µs）。
   理由：挂载确实在换，这时答"活着"才是谎；并发化要单独论证。
2. **§7.3 用户说明**：换挂后 shell 的 cwd 指向旧视图；三种 shell 都不会因此坏掉，`cd .` 后继续可用（W8 读数）。
3. **§10.x 句柄**：一个条目一份句柄 + refs；`status.handles` 与 `dirs_invalidated` 都按**打开次数**计。

## 留给下一阶段

```
cas-store::tail_constant 的自扫描按子串匹配：`let detail = …` 会撞上 "tail"（本轮改名 why 绕过）
⇒ 建议把匹配从子串收紧到标识符边界
「同一会话多次挂载后半活」（502/515 族）仍在，本轮多条判据靠"单跑绿"过关
```
