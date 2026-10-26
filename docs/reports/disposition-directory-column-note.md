# 不一致记录：disposition 表的“目录”列（ADR-009 §4.2 ↔ `fs-semantics::disposition`）

- 作者：architect，2026-10-26；基于 `origin/main` `c92a0da`
- 状态：**结论已定（按 owner 总原则"与真实文件系统行为一致"）**，排期由 manager 定：作 G1 修订卡的姊妹卡或紧随其后（同一函数，不并行）
- 判据影响：改变（实现侧 `decide()` 的 Directory 行与 Want 维度、ADR-009 §4.2 目录列；修订卡提交时填写具体测试名）

## 0. 结论（owner 总原则，2026-10-26）

**以 NT 真实行为为准**，ADR 与实现都要向它对齐：

1. **OPEN / OPEN_IF 目标是目录**：调用方**未**要求 `FILE_NON_DIRECTORY_FILE` ⇒ **打开目录成功**（Win32 侧需 `FILE_FLAG_BACKUP_SEMANTICS`，那是调用方约束，不是 FS 拒绝）；要求了 ⇒ `STATUS_FILE_IS_A_DIRECTORY`。现实现的 `Failed(NotADirectory)` **错**，ADR 的"打开"只在"未要求非目录"时对；
2. **请求目录（`FILE_DIRECTORY_FILE`）而目标是文件** ⇒ `STATUS_NOT_A_DIRECTORY`；
3. **覆盖类（OVERWRITE / OVERWRITE_IF / SUPERSEDE）目标是目录** ⇒ 返回码**以 L2 NTFS 实测为准**（`ACCESS_DENIED` 与 `FILE_IS_A_DIRECTORY` 二选一），实测前在 ADR 与表中标"未验证（P3-4 L2）"，现实现的 `NotADirectory` 在两种可能下都不对；
4. **CREATE 目标是目录** ⇒ `OBJECT_NAME_COLLISION`（两边已一致；WinFsp 在带 `FILE_NON_DIRECTORY_FILE` 时改写为 `FILE_IS_A_DIRECTORY`，`fsop.c:880-882`——adapter 无需自行处理）。

因此修法第 1、2、3 条（加 Want 维度、改正 `IsADirectory`/`NotADirectory`、ADR 拆表）是**必须**的，不再是选项；第 4 条（覆盖类返回码）实测前标"未验证"。

## 1. 两边各写了什么

**ADR-009 §4.2 表**，第三列表头为"目录（请求文件/目录）"：

| disposition | ADR-009 §4.2（目标是目录） |
| --- | --- |
| SUPERSEDE | `STATUS_FILE_IS_A_DIRECTORY` |
| OPEN | 打开 |
| CREATE | `OBJECT_NAME_COLLISION` |
| OPEN_IF | 打开 |
| OVERWRITE | `STATUS_FILE_IS_A_DIRECTORY` |
| OVERWRITE_IF | `STATUS_FILE_IS_A_DIRECTORY` |

表后还有一句："目录请求（`FILE_DIRECTORY_FILE`）与上表交叉：对'文件'目标一律 `STATUS_NOT_A_DIRECTORY`；对'目录'目标的覆盖类 disposition 一律 `STATUS_FILE_IS_A_DIRECTORY`。"

**`crates/fs-semantics/src/disposition.rs:125-153` `decide()`**（18 格，`Exists::Directory` 行）：

| disposition | `decide(d, Directory)` |
| --- | --- |
| SUPERSEDE | `Failed(AccessDenied)` |
| OPEN | `Failed(NotADirectory)` |
| CREATE | `Failed(NameCollision)` |
| OPEN_IF | `Failed(NotADirectory)` |
| OVERWRITE | `Failed(NotADirectory)` |
| OVERWRITE_IF | `Failed(NotADirectory)` |

一致的只有 CREATE 一格；其余五格两边结论不同（ADR 的 OPEN/OPEN_IF 是"打开"，实现是失败；ADR 的覆盖类是 `FILE_IS_A_DIRECTORY`，实现是 `NotADirectory`/`AccessDenied`）。

## 2. 根因（判断，待核）

两边的"目录"列其实回答的是**不同的问题**，表的维度少了一个：

- `decide(disposition, exists)` 只有 `Disposition × Exists` 两维（`Absent/File/Directory`），**不知道调用方请求的是文件还是目录**（`FILE_NON_DIRECTORY_FILE` / `FILE_DIRECTORY_FILE` / 两者都不带）。
- ADR 表头写"请求文件/目录"，把两种请求挤进一列，又在表后用一句话补"目录请求交叉"——同一列里混了两种请求。
- `Failure::NotADirectory` 的文档是"A path component that must be a directory is a file"，用在"目标是目录但请求文件"的格子上，**名字与语义相反**（该情形在 NT 上是 `STATUS_FILE_IS_A_DIRECTORY`）；而 `Failure::IsADirectory`（"The target is a directory; the operation needs a file"）存在却未被这几格使用。

## 3. 外部事实（供裁定用，非结论）

- NT 语义：`OPEN`/`OPEN_IF` 不带 `FILE_NON_DIRECTORY_FILE` 时**可以打开目录**（Win32 `CreateFile` 打开目录需 `FILE_FLAG_BACKUP_SEMANTICS`，属调用方约束）；带 `FILE_NON_DIRECTORY_FILE` 时目标是目录 ⇒ `STATUS_FILE_IS_A_DIRECTORY`。
- 覆盖类（OVERWRITE/OVERWRITE_IF/SUPERSEDE）命中目录：NTFS 返回值需 L2 实测（P3-4 卡 §4 第 1 条）；常见观察为 `STATUS_ACCESS_DENIED` 或 `STATUS_FILE_IS_A_DIRECTORY`，**本记录不下结论**。
- WinFsp 2.1 用户态分发：CREATE 与目录冲突且带 `FILE_NON_DIRECTORY_FILE` 时，把 `OBJECT_NAME_COLLISION` 改为 `STATUS_FILE_IS_A_DIRECTORY`（`src/dll/fsop.c:855-882` `FspFileSystemOpCreate_CollisionCheck`）——说明"请求的是文件还是目录"在传输层是独立维度。

## 4. 建议的修法（供裁定）

1. `decide` 增加第三维 `Want ∈ {File, Directory, Either}`（对应 `FILE_NON_DIRECTORY_FILE` / `FILE_DIRECTORY_FILE` / 都不带），表从 18 格变为 6×3×3=54 格（或以"Want 只影响 Directory/File 两列"压缩表达），**整表仍逐格写出、无通配**；
2. 修正 `Failure` 的使用：目标是目录而请求文件 ⇒ `IsADirectory`（映射 `STATUS_FILE_IS_A_DIRECTORY`）；目标是文件而请求目录 ⇒ `NotADirectory`（映射 `STATUS_NOT_A_DIRECTORY`）；
3. ADR-009 §4.2 拆成"请求文件"与"请求目录"两张表，或在表中显式加 Want 列；删掉表后那句"交叉"说明；
4. 覆盖类命中目录的返回码以 L2 NTFS 对照实测为准（`ACCESS_DENIED` 与 `FILE_IS_A_DIRECTORY` 二选一），实测前标"未验证"。

## 5. 与 SUPERSEDE 修订 (a) 的关系

无依赖。若 (a) 先合入，本修订只动 `Directory` 列与 Want 维度；若本修订先合入，(a) 只动 `File` 列的 SUPERSEDE 格。两者改同一函数与同一张表，**建议同一个 worker 顺序做、不要并行**，避免冲突。
