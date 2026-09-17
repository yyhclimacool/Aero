# M2 反链面板验收

日期：2026-09-17。

## 本轮范围

- 右侧「反链 / 大纲」切换；反链按来源笔记分组，显示标题、路径、正文摘要与链接文字高亮。
- 每次出现算一条引用，最多展示 200 条；不计同篇引用、未解析 / 歧义链接、代码与嵌入。
- 点击打开来源，并滚动、高亮引用所在编辑块；沿用保存失败阻止切换的保护。
- 初次索引完成、保存、重命名、删除、外部变化之后刷新；切换 / 关闭笔记丢弃过期查询。
- 查询时跳过已删除或指纹变化的来源；点击时再次检查来源版本和保存状态，避免使用过期位置。

## 自动化

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

## 尚未覆盖

本轮片段是去除 Markdown 标记后的文本摘要，不含完整富文本渲染。未实现标题栏反链计数、块 `^id` 引用筛选、未链接提及、分页或超过 200 条的完整列表。

GPUI 回归为无头交互测试；原生窗口视觉人工验收另做。本轮不关闭 M1 原生 IME、GUI 性能、远端 CI 和正式签名公证待验项目，详见 [M1 验收记录](m1.md)。
