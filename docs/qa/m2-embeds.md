# M2 嵌入 / 附件补全验收

日期：2026-09-18。

## 功能范围

- 输入 `![[` 列出笔记、图片和 PDF，显示类型及完整路径，支持标题 / 文件名 / 路径模糊筛选，最多 8 条；最近笔记、本篇 `.assets/` 附件及同目录附件有排序加分。普通 `[[` 菜单仍只列笔记。
- `↑↓` 选择，`⏎` / `Tab` / 鼠标确认，`Esc` 关闭；一次插入可单步撤销 / 重做。有现成 `]]` 时复用，光标留在末尾，不重复插入 `!`。无匹配时只插入未解析嵌入源码，不创建文件。
- `![[笔记#` / `![[笔记#^` 复用标题和块选择器；`![[#` / `![[#^` 使用当前未保存笔记。块 ID 仅在确认时写入，同篇补 ID 与插入嵌入组成一个撤销组；跨篇撤销保留目标 ID，沿用已有指纹检查与字节保持逻辑。
- 嵌入的 `|` 后是尺寸等参数，`⇧⏎` 与 `⏎` 一样只插入嵌入，不生成显示别名。编辑带参数的现有嵌入、或在带片段的现有嵌入内编辑笔记名时不打开补全，保留原始源码。
- 支持中文、引用 / callout、表格源码；代码、数学、转义和 URL 内不触发。输入法预编辑不写文档、不确认候选；提交后再筛选。普通链接、嵌入、标题及块模式切换后，旧请求不能覆盖新菜单。
- 附件在后台枚举，进入 `.assets/`，遵守隐藏文件、`.gitignore` 和 `.aero/ignore` 规则；不跟随文件或目录符号链接，不读取 / 修改附件内容，不依赖全文索引。确认前检查附件仍是 vault 内的普通文件，加载 / 读取错误不当作空结果。
- 附件按本篇 `<note>.assets/` → 笔记相对路径 → 显式 vault 路径 → 唯一文件名解析，不区分大小写。限定路径缺失时不退化成另一个同名文件；同名候选生成经解析校验的限定路径，无法表示时提示并保留输入。拒绝绝对路径和 `..`。
- 图片 / PDF 后缀保留给附件，同名笔记使用 `.md`，例如 `![[photo.png.md]]`。`![[paper.pdf#` 不误列 `paper.pdf.md` 的标题 / 块；显式写 `.md#` 可补全。重命名笔记时不改同名附件嵌入，新名字具有附件后缀时也保留 `.md`。
- 核心提供 `scan_embeds`，返回含 `!` 的 UTF-8 范围、目标、片段和参数；普通链接扫描仍排除嵌入。尺寸参数和 PDF `#page=` 仅识别源码，此次未接入渲染。

## 自动化

新增 **16 项测试**：核心 7 项（附件 4、语法扫描 2、重命名联动 1）、补全纯函数 2 项、无头 GPUI 编辑器 6 项、工作区集成 1 项。

- 附件路径优先级、大小写 / 同名消歧、相对路径遮蔽、特殊字符转义、忽略规则、符号链接及删除检查。
- 嵌入范围、标题 / 块 / PDF 页码片段、宽度参数、表格转义、代码 / 数学 / URL / 转义上下文排除。
- 图片 / PDF / 笔记键盘插入、鼠标选择、闭括号与光标、单步撤销重做、同篇未保存标题及块 ID、callout / 表格、IME、加载 / 错误 / 空结果及旧回复保护。
- 同名笔记与附件的插入和重命名隔离、目标删除后拒绝确认。
- 关闭索引后创建附件及笔记 → 工作区请求 → 后台候选 → 插入附件 / 笔记 / 外部块嵌入 → 写入目标 ID → 保存来源；验证附件字节不变。

本机全量 **283 项通过**（app 137、core 99、往返 5、index 42），格式与 diff 检查通过。

```sh
cargo fmt -p aero-core -p aero-index -p aero-app -- --check
cargo test --locked --offline -p aero-core attachment
cargo test --locked --offline -p aero-core embed_scan
cargo test --locked --offline -p aero-core rename_preserves_attachment_embeds
cargo test --locked --offline -p aero-app editor::wiki::tests
cargo test --locked --offline -p aero-app embed_
cargo test --locked --offline -p aero-core -p aero-index -p aero-app
git diff --check
```

日志：`target/m2-embeds-attachments-tests.log`、`target/m2-embeds-scan-tests.log`、`target/m2-embeds-rename-tests.log`、`target/m2-embeds-wiki-tests.log`、`target/m2-embeds-editor-tests.log`、`target/m2-embeds-tests.log`。全量现有废纸篓 / FSEvents 测试使用正常 macOS 权限。本次未复现此前临时索引清理的 RocksDB 日志；构建仍有既有依赖 `block 0.1.6` 的 Rust 未来兼容性提示。

## 范围限制与后续

- 此次交付补全和 Markdown 源码插入。图片、PDF、整篇笔记 / 章节 / 块的实际内容渲染尚未实现；也没有附件粘贴导入、宽度编辑 UI、PDF 页码补全或翻页。
- 候选按扩展名识别：PNG、JPG / JPEG、GIF、WebP、SVG、BMP、TIF / TIFF、AVIF、HEIC / HEIF、ICO、PDF。此清单表示可补全，不代表已实现对应解码器或预览。
- 候选为菜单打开时的快照，外部新增 / 删除 / 重命名后重新打开刷新。尚未接入附件实时监听、引用计数、孤儿检测、反链或内容索引；显式附件路径文本随所属笔记改名的更新也尚未实现。
- `⌘K` 行内链接编辑现已实现，见 [行内链接验收](m2-inline-links.md)。front matter `aliases` 搜索与自动配对仍待实现。

使用无头 GPUI 测试；原生窗口视觉、真实系统输入法、大库候选延迟及个人真实语料尚未人工验收。应用包未重新打包或替换。
