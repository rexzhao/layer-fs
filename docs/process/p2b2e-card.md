# P2b-2e 任务卡（发布期 pin：把"索引提交时 pack 存在"下沉为 OS 保证）

> 状态：**已派**（2026-10-24，`worker`）。依据：architect 对 KI-11 的裁决（ADR-008 §5.2、§7.1）。
> 与 P2b-2d 的 typed `MissingPack` 配套：2d 让损坏**可诊断**，2e 让损坏**不发生**（在 OS 能保证的范围内）。

## 1. 范围

1. **RAII `Pin`**：平台模块 `crates/cas-store/src/durability.rs`（架构红线：平台 API 只允许出现在已批准的平台模块）。
   - Windows：以 `share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)`（**不含** `FILE_SHARE_DELETE`）打开 pack；
   - 非 Windows：退化为 `File::open`，**在文档里明确不承诺**（POSIX `unlink` 不受打开句柄影响），并 `cfg` 跳过相应用例。
2. **接进发布顺序**：`pack::write` 的 `rename` 返回后立即取得 pin，持有到 `record_pack` 的 `COMMIT` 返回。
3. **失败语义**：pin 打开失败（例如 rename 与 pin 之间那数微秒里已被删除）→ **`MissingPack { pack_id }`**，
   此时**一行都没提交**，发布失败、目录里留下无行 pack（由 GC 回收）。**不要**在这里加重试（architect 明确否决
   "可见性复查 + 重新 staging"）。
4. **ADR 已写**：§5.2（pin）、§7.1（错误分型）。实现只需与条文一致，不再改 ADR。

## 2. 验收

- Windows 用例：另一线程在 pin 期间 `remove_file(packs/<id>.pack)` 必须失败（`ERROR_SHARING_VIOLATION`），
  同时读者 `get` 仍成功（pin 的 share 允许 READ）。
- 非 Windows：该用例 `cfg` 跳过，注释写明原因。
- 失败语义用例：pin 打开失败 → `MissingPack { pack_id }` 且索引无行（断言 `pack` 表行数为 0）。
- `verify` / `get` 的 2d 契约不回归；四件套 0；全量绿。
- **不得**新增 fsync、不得改变发布顺序、不得把 pin 生命期延长到 `COMMIT` 之后。

## 3. 边界

不改 GC（P2b-6）。若 pin 引入与 `sync_directory`/`rename_replace` 的新耦合（例如句柄与 rename 的共享冲突），
先回 manager，由 manager 转 architect。
