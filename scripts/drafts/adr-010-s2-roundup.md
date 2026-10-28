# ADR-010 S2 收口材料（worker 草案，供 manager 改写进 docs/）

## 起点与终点

```
S2 起点：981d7b5 之前（S1 收尾）
S2 终点：b806270（adr010-s2）
origin/main：7dd79d4
全量：710/711（唯一红 mounted_write_chain，已知"同一会话多次挂载后半活"族）
clippy --workspace --all-targets：0
fs-cli：44/44 ；fs-workspace：128/128
```

## 一、发布锁（G1–G7）

| 项 | 提交 | 读数 |
|---|---|---|
| Gate(RwLock) + 读锁 9 方法 | `981d7b5` | 行为不变的安全落点 |
| 封绕过门（两扇，不是三扇） | `aa2fee1` | `overlay()` 降 `pub(crate)`；`set_times`/`set_attributes` 下沉到 `Workspace` |
| 写锁：commit / snapshot / restore / rebind | `d341e8d` | `Arc::clone(&self.gate)` 后 `&mut self`（不改签名迎合借用检查） |
| G4 嵌套读 = 0 | `79604b3` | 先测后断言；seam 是原子（crate 禁 unsafe） |
| G1 提交中写入被阻塞且**存活** | `0fa14d6` | 300 ms 后仍阻塞；提交返回后 `/late` 读回 `b"late"` |
| G2 读方法也有闸 | `cd33e5c` | **红先**：`read_at`/`content_size` 原本无闸 |
| G3 写入停在闸内 | `1eebcd4` | 读回 `b"early"` 而非种子 `b"old"`（读实时视图会"绿得不对"） |
| G5 无绕过（源码扫描） | `7bef142` | `crates/*/src/**` 无 `.overlay()`；`pub fn overlay(` 不得存在 |
| G6/G7 饥饿 | `b3d3048` | 最长读等待 **33 ms**；被阻塞写释放 **217 ms**（上界 30000 = IrpTimeout/10）；取**最大**不取平均 |

关键顺序：**先封门再加写锁**（557）。否则锁只挡住"大多数"写；并发正确性里的"大多数"等于零。

## 二、两源并存（562/563）

`BaseState{base_commit, base_root, branch}` 先"只读没人用"地落地（`b501dc9`，`allow(dead_code)` 是诚实标记），
四个双写点（adopt / restore / rebind）先写旧字段、再写新字段（`6ae47b4`），最后一步才删旧字段并把签名 `&mut self` → **`&self`**（`bf47a91`）；
clippy --fix 顺手删掉的 `let mut` 就是"签名真的变了"的证据（561）。

`&mut self` → `&self` 是 G1/G2/G3 的**前置条件**：留着 `&mut`，写者会先被 `Mutex` 挡住，根本到不了闸门——判据会因为错误的原因变绿。

`&str` → `String`（`585c0c1`）不是清理，是语义变更：跨发布的借用会持有"已经不再成立的值"。
`status()` 三个字段取自**同一次快照**（`5ef220b`），否则会打印"分支 A、基线 B"两个瞬间拼成的一个事实（566）。

## 三、管道（P1–P5）

```
b0350c3  Failed{kind, detail, fields}；commit/snapshot 两个 op（args 缺省不是错误）
473608a  挂载内 commit/snapshot 走管道，调用挂载自己的 workspace_handle（同一道闸，G5 的想法上一层）
```

真机：`cd /j && printf hello > probe.txt && layerfs commit -m "from inside the mount"` → `commit 63476a65a932…`；
`layerfs status` → `base: 63476a65a932 / dirty: no`。

## 四、A17 句柄身份（真机）

```
open(h) → write("a") → commit → 同一个句柄 read → "a"          （内容没换）
                                   FileIndex 前置 == 后置       （身份没换）
                     再 write("b") → dirty=true → 第二次 commit  （还能继续用）
```

`Metadata::file_index()` 仍 unstable ⇒ 用 `GetFileInformationByHandle`，**高低两半拼 u64**（只比低半，两块卷上的文件会看起来一样）。

## 五、目录挂载点的发现（A6e–A6g，A6d 由"未验证"转绿）

读数（`3525c2e` 摆出的三个值）：

```
cwd == canonical( cwd)          ⇒ 不是 8.3 短名（假设被自己的仪器否决）
cwd.starts_with(mount_point)    ⇒ 路径匹配没问题
volume 卷标 = "系统"             ⇒ 向上找到的是宿主卷 C:
```

⇒ WinFsp 的目录挂载点**不是**"卷挂载点"，`GetVolumeNameForVolumeMountPointW` 答外层宿主卷；
**目录挂载永远拿不到我们自己的卷标**——卷标那道闸对它前提不成立。

修法（`b806270`）：两种挂载点，两道**结构不同**的安全网

```
盘符挂载点   卷标 = layerfs                  （名字性事实）
目录挂载点   FILE_ATTRIBUTE_REPARSE_POINT    （结构性事实：这块目录确实是某种卷的挂载点）
```

`discover` 的判据：`live && cwd.starts_with(mount_point) && (在我们卷上 || 那个目录是挂载点)`。
A6c 的安全网仍在（`C:\` 不是 reparse point）。

测量方式两处修正（579 / 434）：

```
1 不用 current_dir：SetCurrentDirectory 对目录挂载点答 ERROR_DIRECTORY(267)，量的会是 Win32 边界
  改 bash -c "cd '<mp>' && <bin> status" —— 用户实际走的那条路
2 就绪判据从 metadata(mount_point) 改为"管道能答 hello"：
  目录挂载点在宿主卷上原本就存在 ⇒ metadata 会在卷接上之前成功
  实测 hello 与 record 在同一次 10 ms 轮询（65 ms）里就绪 ⇒ 探针不花钱
```

真机：`cd …/mnt-long-name-test && layerfs status` → `workspace: main / branch: main / base: 0d910344cd3e`。

## 六、留给下一阶段的事实

```
SetCurrentDirectory 对 WinFsp 目录挂载点返回 ERROR_DIRECTORY —— cmd/PowerShell 的 cd 可能同此（未量）
bash/MSYS 的 cd 走另一条路（已证工作）
⇒ 若以后要支持 cmd 内 cd 进目录挂载点，那是平台能力问题，不是发现逻辑问题
```
