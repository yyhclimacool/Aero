# M2 链接悬停预览验收

> 2026-09-26 现状补充：`⌘,` 已提供设置对话框，可在「编辑器」中调整链接预览；反链片段也已渲染富文本。当前入口见 [UI 设计](../ui_design.md)，以下「还没做」与测试数字保留 2026-09-22 的历史含义。

更新日期：2026-09-22。对应 [UI 设计 8.1](../ui_design.md)（链接行为）与 [12](../ui_design.md)（编辑器设置里的「链接悬停预览」）。更早的纯文本浮层见 [行内链接验收](m2-inline-links.md)。

已交付：浮层按笔记预览渲染目标片段；可以改成悬停、按住 ⌘，或关闭。
还没做：反链列表面板的富文本片段、完整的 `⌘,` 设置页、原生窗口人工视觉验收。

## 范围

- 悬停已解析链接 500 ms 后，浮层用编辑器同一套预览画出目标：标题、强调、列表、表格、callout、行内颜色都在。只读，最高约 280 px，超出可滚动。
- 普通笔记大约取前 300 个字符（按去掉标记后的文本计，不切开一个 Unicode 字符）。切片从包含第一块的根块开始，所以 callout 标题和列表标记还在。`#标题` / `#^块` 取整个片段，不截断。front matter 不出现。
- 同篇用未保存文档，其他笔记后台读盘。外部 URL 只显示完整地址，不联网。未解析、歧义、读失败仍是纯文本说明。
- 离开链接后有 150 ms 宽限，用来把指针移进浮层。编辑、滚动、缩放、失焦、重载、删除会取消；过期的异步结果不会再把浮层打开。
- 视图菜单「链接预览」和命令面板：「悬停」「按住 ⌘」「关闭」。当前项打勾。选择记在应用数据目录的 `state.toml`（`link_preview = "hover" | "command" | "off"`），下次启动还在。缺这一项时当作悬停。
- 「按住 ⌘」：指针停在链接上时按下才出现，松开就关，不再等 500 ms。没按键时悬停不会弹出。「关闭」则完全不弹出。

## 自动化

新增 5 项回归（摘录 1、编辑器 2、状态往返 1、工作区 1），全量 **505 项通过**（app 276、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。

- `excerpt_keeps_inline_markdown_for_the_preview_renderer`：摘录保留 `**粗**` 与 `==亮==`。
- `hover_preview_renders_the_target_as_markdown`：跨篇浮层画出 callout 标题，而不是纯文本。
- `preview_off_stays_closed_and_command_waits_for_the_key`：关闭时不出现；按住 ⌘ 才出现，松开消失。
- `link_preview_mode_roundtrips_and_defaults_when_missing`：`state.toml` 往返，缺字段时是悬停。
- `link_preview_setting_is_saved_and_restored`：命令写盘并成为当前设置。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked --offline -p aero-app --bin aero link_preview
cargo test --locked --offline -p aero-core -p aero-index -p aero-app
```

## 限制

- 浮层里的图片和嵌入按当前打开的笔记解析路径。跨篇预览里的附件可能显示找不到。
- 反链面板仍是纯文本摘要，不是这段预览。
- 完整设置页（字体、主题、拼写检查等）还没有；这一项只在视图菜单和命令面板里。
- 无头测试不替代原生窗口的人工视觉验收。
