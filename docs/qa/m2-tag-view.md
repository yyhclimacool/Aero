# M2 标签视图验收

更新日期：2026-09-22。对应 [UI 设计 4.3](../ui_design.md) 与搜索语法 [4.2](../ui_design.md)。标签补全见 [标签补全验收](m2-tags.md)。

已交付：左侧栏「标签」页的分级树、每项计数、点击后到搜索视图执行 `tag:#…`。
还没做：右键重命名标签（M4）、正文 / 属性条里点标签跳搜索。

## 范围

- 左侧栏分段控件现在是「文件 / 搜索 / 标签」。命令面板「显示标签」、视图菜单「标签」切到这一页。
- 树按 `/` 分级：`rust/cli` 显示为 `rust` 下的 `cli`。只出现在子标签里的祖先也会占一个节点，方便展开。默认全部展开，点左侧三角折叠。
- 每项右边是篇数。计数按「点这项会搜到多少篇」算：父标签含子标签，与搜索里的 `tag:#rust` 同一条规则。
- 点标签名（或整行）切到搜索视图，立刻执行 `tag:#rust/cli`（不等 150 ms 防抖），焦点落到搜索框。父标签 `rust` 会命中带 `rust/cli` 的笔记。
- 目录来自索引的 `note_tags`；索引还在同步或不可用时扫磁盘。保存 / 外部改动后索引更新，树跟着刷新。
- 同级按篇数降序、同频按名字。空库显示「还没有标签」。

## 自动化

新增 4 项回归（树 3、工作区 1），全量 **500 项通过**（app 271、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。

- `nested_paths_become_a_tree_with_inclusive_counts`：父子嵌套、父计数含子、顶层同名标签不混进子树、中文路径。
- `missing_ancestors_are_synthesized_and_empty_input_is_empty`：只有 `a/b/c` 时补出 `a`、`a/b`。
- `siblings_sort_by_count_then_name`：同级先按篇数再按名字。
- `tag_sidebar_nests_and_clicks_through_to_search`：三篇笔记（`#rust`、`#rust/cli`、`#中文`）进树；点 `rust/cli` 搜到 b.md；点 `rust` 搜到 a.md 与 b.md。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked --offline -p aero-app -- tag_tree tag_sidebar
cargo test --locked -p aero-core -p aero-index -p aero-app
```

## 尚未实现 / 已知限制

- 右键重命名标签（全库替换）在 M4。
- 正文里的 `#tag`、属性条 chips 点击仍是编辑，不会跳搜索。
- 一篇笔记同时写了 `#rust` 和 `#rust/cli` 时，父节点篇数会把这篇算两次（目录不自动生成祖先，这种情况少）。
- 未保存的新标签要等落盘进索引后才出现在树上。
- 原生窗口视觉、大库滚动与键盘在树里走位尚未人工验收。
