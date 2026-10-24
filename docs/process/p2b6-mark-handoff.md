# P2b-6 mark 交接（worker 的格式事实，2026-10-24）

> 起因：worker 的上下文预算用尽，⑤（mark/sweep）尚未开工。本文件把"读代码确认的格式事实与落点"固化，供 compact 后
> 或交接给另一个会话直接使用。**不含半成品代码**（分支停在 `p2b/gc @ b73f413`，工作树干净）。

## 1. 角色（`cas-core::Role`）

`Bytes=1`、`Chunk=2`、`ExtentLeaf=3`、`ExtentBranch=4`、`DirectoryLeaf=5`、`DirectoryBranch=6`、`InodeTable=7`，
另有 `Root`。

## 2. 页格式（`read.rs` 已在用）

- 首字节 `node`：`0=leaf`、`1=branch`；
- branch = `u8 height | u32 count | { u64 first_key | oid child | u64 subtree_count }*`；
- leaf 载荷按域不同；
- root 值解出 `inode_table: ObjectId`（`RootValue`）；inode leaf = `u32 count | InodeRecord*`；
  `InodeRecord { serial, kind, size, created_ns, modified_ns, content }`；
- `Content::{Empty, Blob(oid), Extents(oid), Dir(oid)}` —— **引用只在这里**；目录项只带 child serial（不产生新 oid）。

## 3. 必须遵守的读取纪律

所有页读取都经过 `fetch(store, oid, Role, ..)` + `Reader::new` + `profile.fmt` + `check_page_size`；这三步是 ADR-005 的
一致性要求，遍历时**必须照做**，不要绕过。

## 4. 落点

`fs-snapshot` 新增 `SnapshotReader::references(&self, oid) -> SnapshotResult<Vec<ObjectId>>`（见 `p2b6-card.md` §11）。
GC 侧：`RootSet::by_root()` 入队 → 弹出 → 已标记则不展开 → 否则写标记集 + 把 `references` 的子节点入队；
`visited: BTreeSet<ObjectId>` 只在需要时保留（或用临时 SQLite，§5.4）。

## 5. 单测要点（与实现同提交）

- 两层树：一个目录 + 两个文件（一个 Blob、一个跨 chunk 的 Extents）→ 访问集合**恰好等于**期望集合；
- 同一父页里的重复引用 → 只出现一次（库内，同一父页去重）；
- 空树（root 指向空 inode 表）→ 只访问 root 与 inode 表；
- 每个 visited 对象的 `Role` 与其信封里的 role 一致（防"按错的 role 读对了字节"）；
- 重复调用 `references(oid)` 结果一致（纯读）；
- `Empty`/`Blob`/叶目录页返回空；损坏/未知 role **报错**而不是跳过。
