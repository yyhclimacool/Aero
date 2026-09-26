---
name: aero-demo-vault
description: >-
  按 Aero 当前已经实现的编辑能力重写 assets/demo-vault，包括欢迎页、体验笔记，以及写法与快捷键说明《使用方法》。守住内嵌表、链接可解析和打包进可执行文件的约束，并打进新的 Aero.app。
  在用户要求重新生成、更新、刷新或打包示例库、demo-vault、demo vault，总结使用方法、写法说明、快捷键，或问某种 Markdown（例如 [文字](url)）在 Aero 里怎么显示时使用。
---

# 示例库

示例库是编译进 `aero` 可执行文件的 Markdown，不是安装包里的 loose 文件。只改 `assets/demo-vault/` 而不重编、不打包，用户打开的仍是旧内容。

交付规则在 `crates/aero-app/src/demo_vault.rs`：首次写到应用数据目录的 `demo-vault/`，之后按 `.aero/demo.toml` 的内容指纹增量更新。用户改过、删掉、改名的笔记不会被覆盖。

## 先对照代码，再写句子

每一句操作说明都要对得上当前实现。设计文档里标成以后再做、或验收记录写着「尚未覆盖」的，不要写成已经能试。

动手前读这些，以代码为准：

- `crates/aero-app/src/actions.rs` 的 `KeyBinding::new`：只写真正绑上的键。设计稿或命令面板里有、这里没有默认键的，写成「在 `⌘⇧P` 里搜名字」。
- `crates/aero-app/src/editor/slash.rs` 的 `ITEMS`：`/` 菜单里有什么写什么。公式、表格、callout、mermaid、图片、嵌入目前不在这个菜单里，要写明「直接写 Markdown」。
- `crates/aero-app/src/editor/preview.rs`：笔记嵌入必须独占一段（`note_embed`）；和文字混在同一段里的嵌入仍是源码。图片只在整段都是图片嵌入时渲染。PDF 有文件才显示页面。
- `crates/aero-app/src/editor/images_ui.rs` 的 `open_external_link`：单方括号链接一律用浏览器打开，没有协议时补 `http://`。库内文件和笔记段落用 `[[显示名]](相对路径)`，单击在访达中定位文件或打开笔记；解析不到不创建文件。`![[ ]]` 仍是嵌入。
- `crates/aero-app/src/editor/block_menu.rs`：块菜单的条目。`⌘/` 打开的是这块菜单。
- `crates/aero-app/src/editor/math_ui.rs`：行内 `$…$` 和 `$$` 块都会排版。
- `crates/aero-app/src/settings.rs`：`⌘,` 里实际有的开关。现在是「聚焦时显示源码」和链接预览（悬停 / 按住 ⌘ / 关闭）。
- `docs/ui_design.md` 里标了「当前实现」的段落，以及对应 `docs/qa/m2-*.md` 的「尚未覆盖」。

已经能放进示例的写法：

- 行内颜色 `[文字]{.red}`、`**粗体**{.blue}`、`[文字]{.red .bg-yellow}`；黄色背景用 `==文字==`。
- 块颜色写在段末、属性在前：`正文 {.bg-blue}`，和块 ID 共存时是 `正文 {.bg-yellow} ^id`。
- 标题颜色 `# 标题 {.blue}`。链接用的是去掉标记后的标题文字。
- 调色板只有 `gray brown orange yellow green blue purple pink red`，背景加 `bg-`。
- 独占一段的 `![[笔记#^id]]`、`![[笔记#标题]]`。
- 语言为 `mermaid` 的围栏代码块。
- 正文标签：`#` 前是空格、首字符不是数字，例如 `#示例/排版`。

不要放：没有随笔记一起交付的图片或 PDF（`demo_vault.rs` 只写入 Markdown）、链接卡片、自定义色值、未解析的嵌入、文档内查找（没有 `⌘F`）、未绑定的 `⌘⇧C`。唯一故意未解析的双链是 `[[下一篇灵感笔记]]`。

## 使用方法

`assets/demo-vault/体验/使用方法.md` 是给人看的写法说明，不是设计文档。欢迎页表格要有一行链到 `[[使用方法]]`。front matter 保持 `title: 使用方法`，别名保留 `使用说明`。

预览会藏起语法，所以每条都要两段：行内代码里的写法，加上紧挨着的真实例子。网页链接用能点开的 `[文字](https://…)`，并保留一个没有协议的例子，说明看得到链接但点不开。

这一篇覆盖：网页链接、双链与嵌入、行内格式和颜色、`/` 菜单与块菜单、图片和 PDF、找笔记和窗口快捷键。只给能动手的最短例子，长例子留在 `体验/Markdown 排版.md`、`体验/双链与别名.md`、`体验/标题与块引用.md`、`练习/移动与改名.md`，并链过去。

`[[下一篇灵感笔记]]` 只出现在 `体验/双链与别名.md`。使用方法里不要再写第二个，用文字指向那一篇。写法示例放进行内代码：`links::scan` 不扫描行内代码、代码块、数学和 `![[嵌入]]`。

## 必须守住的契约

路径和 `NOTES` 一一对应。当前是这七篇：

- `00-欢迎.md`（`WELCOME`）
- `体验/使用方法.md`
- `体验/Markdown 排版.md`
- `体验/双链与别名.md`
- `体验/标题与块引用.md`
- `项目/项目清单.md`
- `练习/移动与改名.md`

增删或改名时，同时改 `NOTES`、`demo_vault_reopen_preserves_edits_deletions_and_renames` 与 `demo_vault_update_delivers_new_content_without_touching_user_notes` 里写死的路径、`scripts/test_bundle_macos.py` 里的篇数，以及欢迎页表格。Rust 测试里的篇数用 `note_count()`（即 `NOTES.len()`）。能放进现有笔记就不要新增文件。

链接约束（`demo_vault_initializes_connected_notes` 用 `links::scan` 走一遍）：

- 解析失败的目标只能是 `下一篇灵感笔记`，且恰好出现一次。
- 同名标题、文件名、alias 不能撞车，否则测试会因 Ambiguous 失败。
- `#章节` 必须等于 `headings::list` 的显示文本：行内标记去掉，`{.blue}` 这类标题属性不计入。
- `#^id` 必须能被 `block_refs::resolve` 找到。现有块 ID 是 `seedidea`，欢迎页以外有两篇引用它，改 ID 要一起改。
- `scan` 不计代码、行内代码、数学和 `![[嵌入]]`。嵌入不计分，但目标仍必须能解析，否则页面上是失败占位。别在嵌入目标里使用那个故意缺失的笔记名。

`练习/移动与改名.md` 里要留着指向 `../项目/项目清单.md` 的普通 Markdown 链接和引用式定义，改名练习靠它们看相对路径怎么改写。

front matter 的 `title` 就是编辑器标题。欢迎页 alias 保持 `示例首页` 和 `开始体验`，双链页 alias 保持 `连接笔记`，项目页 alias 保持 `写作计划`：欢迎页的练习依赖这些名字。

覆盖范围变了的时候，改 `docs/qa/demo-vault.md` 的「示例内容」那一条，以及 `docs/ui_design.md` 第 11 节示例库那一段。不要去改 `project_plan.md` 里带日期的历史进展。

## 写完就验证，再打包

```sh
cargo test --locked -p aero-app demo_vault
bash scripts/bundle-macos.sh debug
python3 scripts/test_bundle_macos.py BundleTests.test_embedded_demo_vault_matches_the_repository
```

打包脚本会 `--locked` 构建 `aero-app`，把二进制放进 `target/debug/bundle/Aero.app`。`include_str!` 在编译期嵌入，所以这一步是示例进包的唯一步骤。需要发布包时再跑 `scripts/bundle-macos.sh release`，或 `scripts/release-macos.sh`。

本机已经有一份示例库时，新包只会替换指纹仍等于上次交付的笔记，并补上新增的示例。要看完整新内容，删掉应用数据目录里的 `demo-vault/`（macOS 一般是 `~/Library/Application Support/Aero/demo-vault`）后再点「查看示例库」。不要把这个目录当成缓存随手清掉却不告诉用户：里面可能有用户改过的笔记。
