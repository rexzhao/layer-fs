# 挂载类夹具的四件套与五个坑（写给下一个改测试的人）

这一页只讲一件事：**为什么挂载类判据的夹具长现在这样**。五个坑都是真机撞出来的，每个都曾让判据红过一次，
而每一次红的解释都不在代码里 —— 现在补上。判据号指向 `pending-docs`。

适用范围：任何需要「起一个挂载、通过它读写、再让它消失」的测试。当前就是 `fs-cli` 的
`restore_live` / `control_pipe` / `discovery_live` / `mount_cli` / `handle_identity` 和 `fs-winfsp` 的
`mounted_*` / `mount_*`。

---

## 四件套（按这个顺序用）

### 1. 记录目录：一个函数，谁都从它取（判据 638）

```rust
fn records_dir(store: &Path) -> PathBuf { store.join("mounts") }
```

- fixture 用它，**并且把它作为返回值**交给测试体（`mount_prepared(...) -> (store, child, root, records)`）；
- 每一个 spawn 出来的子进程都带 `.env("LAYERFS_MOUNTS_DIR", records_dir(store))`，包括 `cli` / `cli_code` 两个 helper；
- 目标：测试体里 `fs_cli::mounts_dir()` 的出现次数是 **0**。现状（据实）：`restore_live` 已达 0；
  `control_pipe`、`discovery_live`、`handle_identity` 仍有使用，它们的挂载仍写真实目录，迁移时照本节办理。

为什么：记录目录若有两处计算，就有两条路。第一版用 `sed` 替换调用点，漏掉一行**跨行写法**，于是
poller 盯着真实目录、mount 写到隔离目录 —— 两边各自都"对"，读出来却是"记录一直 gone"。

### 2. 就绪判据：问 downstream 判据要什么（判据 644）

同一个问题「卷好了吗」，在两个夹具里正确答案不同：

| 夹具 | 不能用的判据 | 用的判据 | 原因 |
|---|---|---|---|
| `discovery_live` | `metadata(mount_point)`、管道 `hello` | **通过卷写一次成功** | `hello` 由管道线程回答，线程在卷接上**之前**就启动了（判据 644 的读数：随后 `current_dir` 拿到 `267`） |
| `control_pipe` | 写探针 | **打开根目录拿到句柄**（`FILE_FLAG_BACKUP_SEMANTICS`） | 写探针会让工作区变脏，把 `status_reports_dirty…` 的"新鲜挂载是干净的"当场弄红（判据 644 第二次读数） |

规律：**仪器不能改变被测系统的状态**。写探针是"能证明卷活着"里最强的一种，但它会留下改动；
打开根目录弱一点，但**不改内容**。

### 3. 起挂载之前：等"这个点上没有卷"（判据 645）

```rust
fn wait_until_free(mount_point: &Path)  // 轮询 drive_present == Some(false)，上界 20 s
```

- 上一条挂载的 `umount` 返回 ≠ 设备已经消失；下一条紧接着挂，会挂在一个还在拆除的卷上；
- **只对盘符等**：目录挂载点是宿主卷上的一个目录，`drive_present` 对它答 `None`，
  等 `Some(false)` 会一直等到上界（实测 20 s 超时，把 A6d 弄红）。

### 4. 失败时留现场（判据 643）

```rust
child.evidence(&record, &mount_point)
// process_alive=… record=… content=… present=Some(true|false|None)
```

三种"没起来"必须分开：**记录没写**（`content` 空/无 `pipe=`）、**卷没起**（`present=false`）、
**进程死了**（`alive=false`）。配套的：mount 的 **stderr 要继承**（`Stdio::inherit()`），
piped 且没人读的流，恰好会在最需要它的时候丢掉最后一句。

---

## 五个坑（每个一句话 + 指向）

1. **`sed`/`perl` 会漏跨行写法** —— 替换调用点不可靠，改用单一函数 + 返回值（判据 638）。这就是 419 的家族。
2. **helper 忘带 env 就会静默走"非挂载分支"** —— 子进程打印 `on feat (workspace feat)` 且 **exit 0**，
   被测判据以为自己在测切换，其实在测创建（判据 637）。
3. **管道 `hello` 早于卷就绪** —— 管道线程先起，`hello` 会答；随后站进挂载点却 `267`（判据 644）。
4. **写探针会弄脏工作区** —— 一个"干净"判据因此变红，而原因在夹具（判据 644 第二次）。
5. **目录挂载点没有 "present" 可言** —— `drive_present` 答 `None`，"等到 false"是永远（判据 645）。

---

## 两条结构性规矩

- **`branch <b>` 不是 `workspace`**：`checkout <b>` 第一次只**创建** workspace 行（A3b），第二次起才是切换。
  凡测"切换"的判据，前置必须显式创建一次并断言它打印 `on ...`。
- **挂载运行期间建的 workspace 打不开**：overlay 是 `EXCLUSIVE`，创建它的进程退出后才释放。
  所以夹具的顺序是「先 prepare，再 mount」，且 prepare 之后要**有界等待"另一个进程能打开它"**（判据 638 的 `mount_prepared`）。

---

## 并行

`.config/nextest.toml` 里有一个 `mounts` 测试组（`max-threads = 1`），过滤器与理由都写在文件里。
其余测试保持并行 —— 串行化只覆盖"真的在挂载"的那一小类，整套串行会把墙钟时间翻几倍。
