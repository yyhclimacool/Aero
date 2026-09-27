# M2 渲染服务与图片验收

> 2026-09-26 现状补充：下文是 2026-09-22 首次交付的记录。数学、Mermaid、PDF、笔记嵌入、附件树与引用计数现已接入；拖放导入和图片宽度预设已实现。渲染任务与内存缓存已有预算，见 [性能续作验收](performance-followup-2026-09-26.md)；磁盘缓存仍无容量淘汰。当前使用说明见 [README](../../README.md)。原测试数字不改写为新结果。

更新日期：2026-09-22。对应 [方案 9.9](../project_plan.md)（重内容渲染服务）、[UI 设计 5.6](../ui_design.md)（富内容块）与 7.5（粘贴规则）。

## 范围

渲染服务骨架（`crates/aero-app/src/render.rs`）：

- 请求 `{ kind, source, width, scale }`：内存缓存按请求命中，磁盘缓存 `cache/<vault-id>/<kind>/<内容指纹>-w<宽>@<倍率>.png` 按**内容指纹**命中，改名 / 复制 / 重装应用都能复用，内容一变自然失效。
- 解码与缩放全在后台线程；首帧按"上次已知像素尺寸"占位，渲染完成替换，列表不抖动。第一次见到的内容用 16:9 占位。
- 只等比缩小、不放大：原图比目标窄时按自然宽度显示。缩放按 HiDPI 倍率算设备像素，倍率与宽度都是缓存键的一部分。
- 渲染失败统一表现为「源码 + 一行错误 + 重试」，不写缓存、不改笔记。磁盘缓存被截断或手工改坏时当作未命中重新渲染。
- 缓存写入是临时文件 + rename，并发渲染同一张图不会读到半个 PNG。
- 具体格式藏在 `Renderer` trait 后面，只有图片一个实现；数学（RaTeX）、图表、PDF 之后各加一个 kind，不必再写缓存 / 占位 / 失败回退。
- 附件解析复用 `![[` 补全那套规则（本篇 `.assets/` → 笔记同级 → 显式 vault 路径 → 全库唯一文件名），目录在后台枚举一次，之后按文件变化增量维护，不在 UI 线程走整库。

图片（`editor/preview.rs`、`editor/images_ui.rs`）：

- `![[x.png]]` / `![[x.png|400]]` 独占一段时渲染为位图，`|400` 是显示宽度，默认按正文列宽；与文字混排的嵌入仍显示源码。
- 单击图片选中所在块（不进编辑态，图片不会被源码顶掉，要改宽度按 ⏎ 进编辑），双击全屏看图，`Esc` / `Space` / 点击关闭。
- 找不到附件、同名附件有多处、图片解码失败都落到统一的失败表现；附件后来补上后点「重试」即可，不用重开笔记。
- 外部改动 / 删除 / 新导入的附件通过文件监听让缓存失效：`watch` 新增 `attachments` 通道（含 `*.assets/` 里的图片和 PDF），只影响渲染缓存，不触发文件树重扫。
- `⌘V`：剪贴板里是位图就写进 `<note>.assets/<ulid 后 8 位>.png` 并插入 `![[…]]`；是 Finder 复制的文件就按原名导入（重名追加 `-2`，绝不覆盖），图片 / PDF 插嵌入、其他文件插普通链接；剪贴板里有文字时一律交还给文本控件按文字粘贴。导入的源码插入是一个撤销组，撤销只回退源码、不删已落盘的文件。

## 自动化

本机结果：格式检查通过；三个正式 crate 共 **461 项通过**（app 241、链接绘制集成 17、core 147、往返 5、index 50、反链计数集成 1），其中渲染与图片相关回归 16 项。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked -p aero-app render::
cargo test --locked -p aero-app image_
cargo test --locked -p aero-app pasting_
cargo test --locked -p aero-app clicking_an_image
cargo test --locked -p aero-core attachment
cargo test --locked -p aero-core -p aero-index -p aero-app
```

`render.rs`（9 项）：缩放与不放大、极端比例不出 0 尺寸纹理、解码失败给中文提示、磁盘缓存填充 / 复用 / 坏缓存自愈 / 不同宽度与倍率互不覆盖、缓存按内容而非路径、文件缺失只报错不 panic、交给 GPUI 的帧是 BGRA、服务的占位 → 就绪 → 失效重渲染、失败与重试、附件解析与补全一致（含同名歧义与增量维护）。

`attachments.rs`（core，2 项）：附件目录的单条增删（导入 / 删除不重扫整库）、导入命名（沿用原名、清洗危险字符、重名不覆盖、无名用 ULID、非法扩展名拒绝）。

`editor/tests.rs`（5 项）：

- `image_embeds_render_as_bitmaps_and_honour_the_width_parameter`：首帧占位 → 位图替换，默认宽度不放大且保持比例，`|120` 按参数显示，混排嵌入与找不到的附件保持源码 + 错误 + 重试。
- `image_failures_recover_after_retry`：附件后补上，点「重试」出图且错误消失（点击不会误进编辑态）。
- `clicking_an_image_selects_its_block_and_double_click_opens_the_viewer`：单击选中块、图片不被源码顶掉；双击开看图，`Esc` 与 `Space` 各关一次。
- `pasting_a_bitmap_imports_it_and_leaves_text_paste_alone`：位图落盘字节一致、插入嵌入、单步撤销回退源码而保留文件；纯文字粘贴仍由文本控件处理。
- `pasting_external_files_imports_them_with_their_names`：原名导入、同名改名不覆盖、非附件类型插普通链接。

## 尚未覆盖

- 原生窗口人工视觉验收未做：真实图片的清晰度、HiDPI 倍率下的观感、看图浮层的实际手感都没在真实窗口里确认。
- `.svg` / `.heic` 等 `image` crate 不支持的格式落到失败表现（源码 + 错误），还没有 resvg 光栅化路径；动图只取第一帧。
- 拖放到编辑区已实现，见 [拖放导入验收](m2-drop.md)。块菜单的宽度预设（小 / 中 / 大 / 原始）未实现，改宽度要手写 `|400`。
- 磁盘缓存没有容量上限与淘汰，只能整体删除；渲染任务也还没按"滚动到视口附近才排队"做优先级与取消（当前是首帧即排队，服务被丢弃时取消）。
- 笔记 / 章节 / 块嵌入渲染、PDF 页面、`.assets/` 在文件树里的折叠展示、附件引用计数与孤儿检测仍未实现（M2 剩余项）。
