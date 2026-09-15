# ADR 0004 · 文本模型：ropey 2.0（与 gpui-base 同型）+ marker 表实现 Anchor

- 状态：**已采纳**（2026-09-15，spike 半天完成）
- 关联：`project_plan.md` 7.2 / 7.4 / 9.3；`spikes/rope/`（可重跑：`cargo run -p spike-rope --release`）

## 问题

方案 7.2 是"一份 Rope 作为唯一文本真相 + Block 树持有 `Range<Anchor>`"，7.4 是每次编辑整篇重解析。要回答：

1. 1 MB / 10 MB 笔记上的编辑延迟（输入延迟预算 p99 < 16 ms，这只是其中一小段）；
2. 快照是否 O(1)——整篇重解析要在后台对快照做，否则大文件每次按键都要等解析；
3. byte / char / line 换算的成本（光标、选区、"保持视觉列"都要用）；
4. 给 comrak 物化整篇 `String` 的成本（comrak 只吃 `&str`）；
5. anchor 怎么实现、正确性、每次编辑的成本。

候选：`ropey 1.6.1`、`ropey 2.0.0-beta.1`、`crop 0.4.3`，加 `String` 基线。原方案写的"Zed 的 `rope`/`sum_tree`（自带 anchor）"不成立：Zed 的 anchor 在 `text::Buffer`（CRDT，按 insertion id 定位）而不在 `rope` crate；`rope` 不在 crates.io，只能 git 依赖 Zed 单体仓库，还拖着 `util`（`gpui_util`、`rust-embed`、`async_zip`、`schemars`…）。刚在 ADR 0003 记录过 git 依赖的代价，不再试。

## 一个改变选型的事实

**gpui-base 0.6.1 的 `InputBaseState.text` 就是 `ropey =2.0.0-beta.1` 的 `Rope`**（features：`metric_chars`、`metric_lines_lf`、`metric_utf16`），`cursor()` / `selected_range()` / `set_selected_range()` 全是 **UTF-8 字节偏移**。ropey 2.0 把 API 改成了字节索引（`insert(byte_idx)` / `remove(byte_range)` / `slice(byte_range)`），char / line / utf16 是可选 metric。

这意味着 Document 若用同一个 ropey 2.0，聚焦块控件（9.3）可以直接拿 `RopeSlice` 而不做 `String` 往返，Document、comrak、gpui-base 三方全部说字节偏移。

## 语料与脚本

合成 Markdown（标题、段落、列表、代码块、callout，60% 中文），1 MB（15.8k 行、10k 块级节点）与 10 MB（157k 行、97k 块级节点）。同一份确定性编辑脚本（10k 次：65% 插入 1–2 个 char 含中文与换行，35% 删除 1–3 char，位置按比例落到 char 边界），四个候选回放后最终文本哈希一致。Apple M5 Pro，release。

## 结果（10 MB；1 MB 全部同量级或更小）

| 操作 | String | ropey 1.6 | **ropey 2.0-beta.1** | crop 0.4 |
| --- | --- | --- | --- | --- |
| 构建 | 0.25 ms | 2.8 ms | 2.3 ms | 1.3 ms |
| 堆开销（相对原文） | 0% | 14% | 11% | 4% |
| clone（快照） | 161 µs | 0.02 µs | 0.02 µs | 0.02 µs |
| 随机插入 mean / p99 | 88 / 334 µs | 0.41 / 2.3 µs | 0.38 / 1.8 µs | 0.36 / 1.8 µs |
| 连续键入 mean | 87 µs | 0.09 µs | 0.05 µs | 0.04 µs |
| byte→line / line→byte | 0.8 / 2.0 **ms** | 0.08 / 0.24 µs | 0.05 / 0.18 µs | 0.06 / 0.13 µs |
| byte→char | 0.18 ms | 0.05 µs | 0.05 µs | **5.1 ms**（无 char 索引） |
| 切 300 B 块文本 | 0.04 µs | 0.26 µs | 0.22 µs | 0.20 µs |
| 物化整篇 String | 0.20 ms | 0.51 ms | 0.67 ms | 1.13 ms |

其他：

- comrak 整篇解析：1 MB 4.9 ms、10 MB 43 ms（本语料比 ADR 0002 的真实笔记简单；真实笔记按 18 ms/MB 估，10 MB ≈ 180 ms）。
- **`String` 其实够快**：10 MB 上插入 p99 0.33 ms。rope 真正买到的是 O(1) 快照与 O(log n) 的行 / char 换算——String 的行换算是毫秒级，光标一动一次就超预算。
- 三个 rope 的编辑成本差在 0.1 µs 量级，对 Aero 没有意义。

### Anchor：排序 marker 表

实现：`Vec<(pos, Bias)>` 按位置排序，块区间 = `(start, Left)` + `(end, Right)`——在块首插入不动 start、在块尾插入 end 跟着走，两端都"吸纳"边界上的输入。每次编辑 `[start, end) → new_len`：二分找到第一个 `pos >= start`，其后线性平移；落在被删区间内的按 Bias 塌缩到替换区间的一端。

| | 1 MB（20k marker） | 10 MB（194k marker） |
| --- | --- | --- |
| 每次编辑平移 mean / p99 | 5.0 / 13 µs | 49 / 130 µs |

正确性：回放 10k 次编辑后，对未被触碰的块（10 MB 上 85,155 个）校验 `rope[drifted range] == 原文`：85,018 完全相同，137 个因边界插入而"长大"（原文仍是子串），**0 个错误**；marker 表与独立参考实现逐项相等。

不需要 sum tree / Fenwick：marker 表每次重解析后整表重建（O(m)），编辑间只做平移，线性扫描在 10 MB 上 49 µs。

## 决定

1. **Rope = `ropey =2.0.0-beta.1`**，features 与 gpui-base 完全一致（`metric_chars`、`metric_lines_lf`、`metric_utf16`），版本随 gpui-kit 升级，不单独升。beta 的风险由 gpui-base 一起扛：它已经用这个版本承载 IME、多光标、undo。
2. **Anchor = marker 表**：`Anchor` 是 marker 表的下标句柄，`Document` 持有表，重解析后重建并把旧 `BlockId` 按 7.3 规则映射；选区、光标、IME 标记区间同样以 marker 表达。放在 `aero-core`，不依赖任何 UI 类型。
3. **编辑管线分两段**（改 7.4）：
   - 同步段（按键路径）：rope 编辑 + marker 平移 + 聚焦块从 rope 切片重绘。10 MB 上 < 0.2 ms。
   - 异步段：`rope.clone()` 快照 → 后台线程物化（≤ 0.7 ms）+ comrak 解析 → 主线程 Block 树 diff、重建 marker 表。文档 < 256 KB 时可以直接同步做（≤ 5 ms），大于此阈值一律异步，期间视图靠漂移后的 anchor 保持一致。
4. `crop` 不选：无 char 索引，且与 gpui-base 异型；`ropey 1.6` 不选：char 索引 API 与其余各方的字节偏移不一致，且 gpui-base 已在 2.0。

## 对方案的修正

- 7.2：Rope 定为 ropey 2.0；删去 Zed `rope`/`sum_tree` 备选与"自带 anchor"的说法；Anchor 实现写明。
- 7.4：管线拆成同步 / 异步两段，阈值 256 KB。
- 9.3：聚焦块控件直接共享 Document 的 Rope 类型与字节偏移。
- 18 M0：Rope 选型完成；M0 四个 spike 全部关闭。
