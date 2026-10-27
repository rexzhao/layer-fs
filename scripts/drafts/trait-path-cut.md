# Next cut: `Path { key, display }` + the shape sweep (473, corrected by 477/478)

**This is a plan, not a work log.** The cut is one of those that needs a whole fresh budget (445): trait shape changes are mechanical but touch every
call site, and doing them in two sittings produces two half-shapes.

**Correction (477):** the first version of this plan left the **store** out of the dependency chain. `read_view::lookup` matches on the **raw bytes that
were stored**, so `Path` alone does not let `ApiBackend` "look up by key" — looking up `README.TXT` cannot find an entry stored as `ReadMe.txt`. The
real chain is: **the layer where lookup happens folds** → the backend can match by key while storing the display → then criterion ③ means something.

**Ruling (478):** `fs-snapshot`'s on-disk page order is **not** changed here (index key + display in separate fields would mean a format version bit and
re-baselining every fixture — that is project **B-18**, not a step in this cut). Fold at the **overlay view**, which is the layer where lookups actually
happen; the snapshot is the data being searched.

**Ruling (478):** the overlay uses a **scan** when the exact lookup misses, not a folded-key **cache**. This is not a performance trade: a scan is slow
**observably**, while a cache that misses an update on any of `bind`/`unbind`/`rename`/`remove_at` is wrong **silently** — which is the exact defect
class this whole campaign has been chasing. A folded-index cache is **B-19**, to be built later *against* the scan as its correctness baseline.

## Why these items are one cut

```
(trait) Path { key, display }        the matching key and the bytes to hand back, as one value
(②) overlay folding (scan)           the layer where lookup happens; the backend then stores the display spelling
(③) two-backend agreement test       only meaningful once ② exists
(shape) DirOutcome { id, binding }   the other parked trait shape change; same layer, same sweep
```

`②` cannot be done alone: folding a path at the backend entrance also folds the bytes that get **stored**, which loses the display spelling. That is the
same mistake the mock made once and still documents at `mock.rs:104` ("folding inside `split` alone would have made the display name folded too —
measured: an entry created as `A` listed as `a`"). The correct shape needs both values across the boundary, i.e. this cut.

## Order inside the cut (each step compiles before the next)

1. **Introduce `Path`** — **done** (`a22c9dd`): `new(raw)` folds with `fs_semantics::name::fold_bytes` and keeps the raw bytes; `display_name()` answers
   `Option` so "this path has no name" cannot become "an empty name in the store". No call site changed.
2. **Change `WorkspaceOps`'s path parameters** to `&Path` in one commit — the compiler lists all **18** implementations (`2` real + `16` test doubles),
   which is exactly why this is one step and not twelve.
3. **Overlay folding, scan version** (478): in `fs-overlay/src/view.rs::lookup_impl`, when the exact lookup misses, scan the base directory and compare
   `fold_bytes`; the overlay's own rows are written with the display spelling, so the same comparison applies to them. **This is where ② actually
   lands** — `ApiBackend` needs no folding of its own once the view matches by key.
   - Criterion: a base entry stored as `ReadMe.txt` is found by looking up `readme.txt`, and the name it reports back is still `ReadMe.txt`.
4. **`DirOutcome { id, binding }`** merge (the other parked shape).
5. **`FileInfoTimeout = 0`**: verify the semantics rather than describing them in a comment (the belief has been unverified since batch (i)).
6. **Criterion ③**: one shared scenario, both backends, same answers — including a case-only rename answered **directly** at the backend (this is also
   the U1 direct test; it bypasses the mount layer, where the FSD short-circuits case-only renames and the request never arrives — U16).

## What must not change while this is done

- `display` bytes are what `read_dir` returns, on every path, including after a rename that only changes case.
- The single fold definition stays `fs_semantics::name::fold_bytes`. No second copy, no local `to_ascii_lowercase`/`to_ascii_uppercase`.
- No criterion is widened, and no test count may drop (450).

## Entry state

```
branch p3-4/mount @ a22c9dd ；656/656 (workspace, lock) ；clippy 0 ；worktree clean
step 1 done ；steps 2–6 not started
```

## Parked as their own projects (477/478)

- **B-18 — snapshot format**: index key and display spelling as separate fields, a format version bit, every fixture re-made and every baseline
  re-timed. Triggered when the fold must be pushed down to disk (or a direct snapshot reader needs the same semantics).
- **B-19 — folded-index cache**: a `(parent, folded) -> display` index in the view layer. Built **against the scan as its baseline**: any divergence is a
  silent wrong lookup, which is why it is not the first version.

---

# 收口小结（486）—— 这是这一刀的判决书

**入口（刀前基线，475）**：`462a925`，175/175 定向、652/652 全量、clippy 0。**出口**：`782d558`，**661/661**、clippy 0。
七次提交，`+11` 判据，两例**反转**（444 口径），两笔挂账（B-18、B-19）。

| # | 提交 | 内容 | 判据 |
|---|---|---|---|
| 1 | `a22c9dd` | `ops::Path { key, display }`，两个字段一次决定 | +4 |
| 2 | `04af3b8` | overlay 按折叠键查找（**扫描版**）+ 两例反转 | +2 / 反 2 |
| 3 | `2d07cd2` | U1 直测：真后端上的 case-only rename | +1 |
| 4 | `a28749e` | `DirOutcome { id, binding }`，身份与绑定同行 | +1 |
| 5 | `2b7dcf7` | `FileInfoTimeout = 0` **量出来**（FSD 背后改成 11 → 挂载侧读到 11） | 0 |
| 6 | `a2b409f` | 6b：同一场景在 mock 与真后端各跑一遍 | +1 |
| 7 | `782d558` | 12 方法签名 / 18 impl 迁到 `&Path<'_>`，**行为不变** | 0 |

## 三件这次才成为事实的事

1. **一个缺陷一个修复点**：折叠进 `overlay` 的查找层之后，`lookup`/`rename`/`remove_at`/`read_dir` 同时获得它 —— 不是每个规则各写一遍（442）。
2. **两后端一致是两条读数**：`scenario_case_insensitive_names` 在 mock 与 `ApiBackend` 上各跑一次；此前 mock 折、真后端不折，同一个判据在两边测的是两件事（472/485）。
3. **U1 在它能被问的地方被证明**：case-only rename 在挂载层**到不了我们**（U16，双工具 + 源码 + flag 翻转都试过），但在后端直问的答案是事实（482）。

## 两条反转（不是放宽）

- `case_only_rename_onto_another_inode_…` → `two_spellings_of_one_name_cannot_coexist_and_rename_replaces_ordinals`：
  「`/A` 与 `/a` 是两个 inode」这个前提被折叠**有意**变成不可达，空出的半边换成正向判据（第二次 create 必须 `AlreadyExists`）。
- 「case-only 改名后旧拼写不再解析」→ **「旧拼写仍解析到同一 inode，且目录里只剩新拼写一条」**：原断言只证明旧壳死了，改写同时钉住折叠匹配与显示保持。

## 挂账

- **B-18 — snapshot 格式**：索引键与显示名分字段存，需要格式版本位、全套 fixture 重做、基线重定。触发条件：折叠要下推到磁盘，或出现直读 snapshot 的消费者。
- **B-19 — 折叠索引缓存**：`(parent, folded) -> display` 的缓存。**对着扫描版做差分**再上；缓存漏更新是静默错查，扫描慢是可见的。

## 两次事故（都要并进结构修复）

- 批量替换 `\'` → `'` **越界改了三个无关文件**（`cas-store`、`fs-snapshot`、`fs-windows` 的字符字面量）。编译报错抓到、`git checkout` 复原、提交前核对 `git status` 文件清单。这是 **419 的第四族**：CRLF 静默 no-op 是「没改到」，这次是「改多了」——同一根源，替换工具不尊重边界。
- 计划本身漏了 store 层（477）：`read_view::lookup` 比的是**落库的原始字节**，所以 `Path` 单独存在时后端仍然做不到「用 key 查」。施工图带着错的依赖链比没有施工图更危险。
