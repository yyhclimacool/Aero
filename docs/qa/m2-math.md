# M2 数学公式渲染验收

> 2026-09-26 现状补充：下文保留最初只交付公式块时的范围和测试数字。行内公式、源码实时预览与超宽公式横向滚动已接入，见 [UI 设计 5.6](../ui_design.md#56-富内容块)；内存缓存已有 [预算](performance-followup-2026-09-26.md)，磁盘缓存仍无容量淘汰。后面的「尚未覆盖」仅描述原验收日期。

更新日期：2026-09-22。对应 [方案 6.5](../project_plan.md)（数学）与 9.9（重内容渲染服务）、[UI 设计 5.6](../ui_design.md)。

## 范围

- `$$…$$` 块在预览态渲染成位图，源码态仍是源码。管线是 RaTeX 的四步：`ratex_parser::parse` → `ratex_layout::layout`（带主题色与行间 / 行内样式）→ `to_display_list` → `ratex_render::render_to_png`。
- 颜色在**排版阶段**给定（`LayoutOptions::with_color`），公式直接按主题前景色排出来，不走 9.9 里"单色 SVG + alpha 蒙版"那条路；主题色因此是缓存键的一部分，换主题会重排。
- 字体用 `ratex-render` 的 `embed-fonts` 特性内嵌 KaTeX TTF：运行时不依赖 `font_dir`，`.app` 被移动或挂在只读镜像上也能渲染。
- 复用 9.9 的渲染服务，所以占位、两级缓存、失败回退、取消都与图片共用一套：
  - 渲染服务的"源"从"vault 内文件"泛化为 `Source::File | Source::Formula`。图片按文件内容指纹入盘；公式没有磁盘文件，按 `源码 + 行间/行内 + 颜色` 的指纹入盘（`cache/<vault-id>/math/<指纹>-w<字号>@<倍率>.png`）。
  - 首帧按字号给占位条，渲染完成后替换；渲染在后台线程，UI 不阻塞。
  - 语法错误 / 排版失败显示「源码 + 一行错误」，不写缓存、不改笔记。空公式（`$$$$`）直接按源码显示，不去麻烦排版引擎。
- 公式按正文字号（`BODY_FONT_SIZE`）排版，按窗口 HiDPI 倍率渲染设备像素再按逻辑尺寸显示。块渲染时拿不到 `Window`，倍率由 `Editor::render` 每帧记下。

## 自动化

本机结果：格式检查通过；三个正式 crate 共 **472 项通过**（app 250、链接绘制集成 17、core 149、往返 5、index 50、反链计数集成 1），其中数学相关 4 项。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked -p aero-app render::tests::math
cargo test --locked -p aero-app math_
cargo test --locked -p aero-core -p aero-index -p aero-app
```

- `math_rasterizes_formulas_and_reports_syntax_errors`：真实排版出可见笔画、背景透明、笔画带上指定颜色；`\frac{` 这类语法错误返回中文错误而不是 panic；两个渲染器互不接受对方的源。
- `math_disk_cache_is_keyed_by_formula_style_and_color`：同一公式第二次命中磁盘缓存；换行间 / 行内、换颜色各自是新的缓存项（三个文件）。
- `formula_strips_delimiters_and_whitespace`：`$$` 定界符与空白的剥离，空公式返回空串。
- `math_blocks_render_as_bitmaps_and_fall_back_to_source`：GPUI 里真实绘制——首帧占位、渲染完成出现位图且有高度；写坏的公式显示源码 + 错误、不出位图。

## 尚未覆盖

- 行内 `$…$` 仍显示源码：只做了 `$$` 块。渲染器已经支持行内样式（`MathStyle::Text`），接行内需要在预览的行内层排版并按 baseline 对齐。
- 源码态没有"实时预览"（5.6 表格里写的"源码 + 实时预览"）：目前编辑时只有源码。
- 原生窗口人工视觉验收未做：真实字号下的清晰度、基线位置、深浅主题对比都没在真实窗口里确认。
- 公式位图的宽度没有按正文列宽做缩放 / 横向滚动，超宽公式会被容器裁掉（`max_w_full`）。
- 缓存没有容量上限；主题色每换一次就多一份缓存，只能整体删除。
- M4 计划里"直接消费 display list 用 GPUI 绘制"未做，现在是位图。
