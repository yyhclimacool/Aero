# M2 反链面板验收

> 2026-09-26 现状补充：反链现已移到文档底部，标题计数入口 /「视图 → 反链」滚动到引用区；片段已有富文本渲染（见 [浏览与编辑一致验收](m2-seamless-edit.md)）。计数、块筛选与 200 条展示上限保留；未链接提及和完整分页尚未实现。以下保留早期布局及验收范围，末尾限制以当时为准。

更新日期：2026-09-21（计数 / 块筛选补充）。旧范围与测试记录的原日期为 2026-09-17，以下保留，不代表当日复跑结果。

## 原有范围（2026-09-17）

- 右侧「反链 / 大纲」切换；反链按来源笔记分组，显示标题、路径、正文摘要与链接文字高亮。
- 每次出现算一条引用，最多展示 200 条；不计同篇引用、未解析 / 歧义链接、代码与嵌入。
- 点击打开来源，并滚动、高亮引用所在编辑块；沿用保存失败阻止切换的保护。
- 初次索引完成、保存、重命名、删除、外部变化之后刷新；切换 / 关闭笔记丢弃过期查询。
- 查询时跳过已删除或指纹变化的来源；点击时再次检查来源版本和保存状态，避免使用过期位置。

## 自动化（2026-09-17 历史记录）

本机结果：格式检查通过；三个正式 crate 的测试报告共 **208 项通过**（app 89、core 76、往返 5、index 38），其中新增反链回归 4 项。完整日志位于 `target/m2-backlinks-tests.log`。首轮沙箱内的废纸篓 / FSEvents 集成用例受权限限制失败；在正常 macOS 权限下复跑全部通过。未设置 `AERO_CORPUS_DIR`，不代表真实个人语料已验收。

```sh
cargo test --locked --offline -p aero-index -p aero-app backlinks
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked --offline -p aero-core -p aero-index -p aero-app
```

新增回归：

- `backlinks_group_sources_preserve_occurrences_and_skip_non_links`：来源排序、多次出现、中文 / 别名、BOM / CRLF 归一化、条数限制及未同步的改动 / 删除。
- `backlinks_follow_source_edits_renames_and_target_resolution`：来源编辑 / 重命名 / 删除、目标晚创建 / 删除 / 重建。
- `backlinks_exclude_ambiguous_targets`：同名文件和标题歧义不算入链，精确路径仍有效。
- `backlinks_refresh_navigate_and_reject_stale_positions`：GPUI 面板实际鼠标事件 → 打开来源 → 高亮标题块、增量刷新、过期位置保护、连续切换与关闭。

## 2026-09-21 补充范围（已实现）

- 笔记标题栏新增反链计数入口，点击打开反链面板。标题入口与面板 total 来自独立 COUNT，不从展示列表长度推算，完整计数不受 200 条展示上限影响。
- 当前块有唯一显式 `^id` 时，可在「全部引用 / 此块的引用」之间切换；无 ID 或重复 ID 不提供该块筛选。fragment filter 在 LIMIT 前执行；total 始终是全篇总数，matched 是对应筛选的完整计数，不是在前 200 条中二次过滤。
- 仍按每次链接出现计数，而非按来源笔记去重；self、代码、嵌入、未解析和歧义链接不计。aliases 与 title 同级参与解析，别名冲突不计入已解析反链。
- 列表仍最多展示 200 条，按来源分组、纯文本摘要及链接文字高亮；未链接提及、富文本反链片段和完整分页列表未实现。
- 写任务 FIFO，查询等待调用前已排队的写入；保存 / 改名 / 删除 / aliases 变化后刷新反链，见 [重命名记录](m2-rename.md)。

### 新增回归

- `backlink_totals_and_block_filters_are_independent_of_the_page_limit`：209 条全篇引用、200 条展示上限、LIMIT 前筛选及更新 / 删除后的计数。
- `backlink_count_and_selected_block_filter_use_full_index`：207 条引用、标题入口、块筛选、重命名后的选区订阅与再次筛选。测试推进虚拟时间让重命名提示消失后点击，避免提示层遮挡按钮。
- `backlinks_connect_with_an_existing_selection` 与 `backlink_selection_notifies_when_the_caret_moves_between_nested_blocks`：连接时已有选区，以及同一引用容器中光标切换目标块。

```sh
cargo test --locked --offline -p aero-index --test backlink_counts
cargo test --locked --offline -p aero-app --bin aero backlink
cargo test --locked --offline -p aero-core -p aero-index -p aero-app
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
git diff --check
```

2026-09-21 全量 395 项通过，包含上述反链计数与交互回归；日志与分项计数见 [补全记录](m2-completion.md)。原生人工验收未完成：macOS 辅助功能与录屏权限均未授予；无头交互不能代替原生视觉结论。未改已安装应用。

## 尚未覆盖

当前片段是去除 Markdown 标记后的文本摘要，不含完整富文本渲染。标题栏反链计数与块 `^id` 引用筛选已通过自动化回归；未链接提及、富文本片段、分页或超过 200 条的完整列表仍未实现。

2026-09-17 的 GPUI 回归为无头交互测试；原生窗口视觉人工验收仍待进行。M2 尚未整体完成，也不关闭 M1 原生 IME、GUI 性能、远端 CI 和正式签名公证待验项目，详见 [M1 验收记录](m1.md)。
