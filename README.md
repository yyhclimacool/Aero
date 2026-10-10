# Aero
<img src="assets/branding/aero-icon-v1.png" alt="Aero 图标" width="112" height="112">

A blazing fast, AI Native, local-first knowledge IDE.

当前为 macOS 开发版。除编辑、全文搜索和知识互联外，已接入本地语义搜索、Mermaid 与 PDF 预览；MCP、链接卡片和自动更新仍在规划中。本文按 2026-09-26 源码核对，历史验收的日期与范围见各条目链接。

## 运行

```sh
# 首次：下载并校验 zvec 预编译 C 库到 vendor/zvec/lib（全文索引引擎；仅这份库不再于构建期下载）
scripts/fetch-zvec.sh

# 打开指定文件夹作为 vault；不带参数则恢复最近打开的库，没有则停在库选择页
cargo run -p aero-app -- ~/Knowledge

# 日志（默认只输出 warn）
AERO_LOG=debug cargo run -p aero-app -- ~/Knowledge

# 正式 crate 的单元、往返、索引与无头 GPUI 交互测试（真实模型测试默认跳过）
cargo test --locked -p aero-core -p aero-index -p aero-embedding -p aero-app

# 包括保留的 M0 spike 在内的工作区测试
cargo test --locked --workspace

# M1 自动验收（格式、测试、从当前源码打包及 DMG 检查）
bash scripts/check-m1.sh --bundle
```

M1 验收状态及复跑说明见 [docs/qa/m1.md](docs/qa/m1.md)，包含保存失败回归、输入法清单和真实性能基准；自动化通过不代表原生 IME、GUI 性能与正式公证全部通过。

`check-m1.sh` 验收 `aero-core`、`aero-index`、`aero-embedding` 和 `aero-app` 四个 crate。本地 GPUI 补丁另用 `cargo test --locked -p gpui-base` 检查。近期性能优化与测试、真实窗口记录见 [2026-09-26 验收](docs/qa/performance-followup-2026-09-26.md)和 [2026-10-08 首批优化验收](docs/qa/performance-o1-o2-2026-10-08.md)，尚未据此证明 GUI 延迟、帧率和内存预算全部达标。首次构建还需下载 Cargo 依赖与 ONNX Runtime 构建依赖；语义模型由用户在应用内另行授权下载。

全文索引存放在 `~/Library/Application Support/Aero/index/<vault-id>/`，是可随时删除重建的缓存；首次打开 vault 时后台建索引，状态栏显示进度。

无参数启动恢复最近打开且目录还在的库（已删除的条目跳过，不弹错误）；首次运行或没有可恢复的库时停在库选择页。命令行指定路径直接打开该库。选择页除了「打开文件夹…」「新建 Vault…」和最近列表，还有「查看示例库」：内置中文示例位于 `assets/demo-vault/`，编译时嵌入可执行文件，点击时才展开到 `~/Library/Application Support/Aero/demo-vault/` 并停在欢迎页。最近列表里的示例库（包括启动时自动恢复它）走同一个入口，所以每次进去都是当前版本的示例，落点也始终是欢迎页。

示例库是可编辑的笔记库，**不是可丢弃的索引缓存**：只重装应用会保留它，连同应用数据一起删除才重新初始化。每个 `.dmg` 带的都是该次构建的示例，所以再次进入示例库时会按 `.aero/demo.toml` 里的交付记录做增量更新——还是上次交付原样的示例换成新版本，你改过的保持原样，删掉或改名的不恢复，新增的示例补上。手动建的同名 `demo-vault/` 目录没有交付记录，一个字都不会被写入。Linux / Windows 下使用对应应用数据目录中的 `demo-vault/`。示例内容与验收范围见 [默认示例库验收](docs/qa/demo-vault.md)。

macOS 上 `cargo run` 会自动使用 Aero 的 Dock 图标；欢迎页也使用同一套内嵌品牌资源。

### macOS 应用打包

需要 Python 3 与 Xcode 命令行工具。脚本会构建应用、装入图标并进行本地 ad-hoc 签名：

```sh
scripts/bundle-macos.sh
open target/debug/bundle/Aero.app

# Release 构建
scripts/bundle-macos.sh release
open target/release/bundle/Aero.app
```

自定义 Cargo target 目录或构建 target 时，以脚本输出的 `.app` 路径为准。图标源文件位于 `assets/branding/`，应用包使用 `Aero.icns`；版本号自动取自 Cargo 配置。

### 发布（签名、公证、.dmg）

```sh
# 本机试用：ad-hoc 签名的 Aero.app + 未签名的 dist/Aero-<版本>.dmg
scripts/release-macos.sh

# 正式分发：Developer ID 签名（hardened runtime）+ 公证并装订 .app 与 .dmg
export AERO_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export AERO_NOTARY_PROFILE=aero      # 先 xcrun notarytool store-credentials aero
scripts/release-macos.sh --notarize  # 或改用 AERO_NOTARY_APPLE_ID / AERO_NOTARY_TEAM_ID / AERO_NOTARY_PASSWORD

# 单步：scripts/dmg-macos.sh <Aero.app> <out.dmg>、scripts/notarize-macos.sh <Aero.app|Aero.dmg>
# 打包脚本自测：python3 scripts/test_bundle_macos.py
```

凭据只从环境变量读取，脚本不保存任何秘密。当前没有配置自动更新源：「检查更新」只显示本地说明，发布仍是上传 `.dmg`。

### 本地语义搜索

打开 vault 后，在 `⌘,` →「搜索与隐私」（或搜索栏「设置」）中开启「允许模型下载」，点击「下载模型」，再开启「启用本地语义搜索」。固定的 multilingual-e5-small int8 模型约 135 MB，使用 CPU 在本地计算；笔记和查询不上传，下载和语义搜索默认均关闭。模型就绪后可离线使用。

`⌘⇧F` 的「混合」模式先显示全文结果，再补充语义结果；模型未就绪或失败时保留全文搜索。索引可暂停，输入时让出；模型空闲 60 秒后卸载。模型、缓存位置与验证方法见 [本地语义搜索](docs/semantic-search.md)。

### 命令行

GUI 与 CLI 共用 `aero` 可执行文件；开发时可把 `cargo run -p aero-app --` 放在参数前：

```sh
cargo run -p aero-app -- --help
cargo run -p aero-app -- --vault ~/Knowledge search "查询" --json
cargo run -p aero-app -- --vault ~/Knowledge export note.md --format html
```

已提供 `search`、`read`、`backlinks`、`append`、`new`、`export`、`index`。CLI 搜索目前是全文搜索；各命令参数、索引占用时的行为见 [CLI 使用与验收记录](docs/qa/m2-cli.md)。

## 功能与文档

- 设计文档：[技术方案](docs/project_plan.md)、[UI 与交互](docs/ui_design.md)。设计目标、当前实现与带日期的验收记录分别标明。
- 代码分层：`aero-core`（文本 / 块 / 解析 / vault，无 UI）、`aero-index`（zvec 全文与向量索引、SQLite 元数据）、`aero-embedding`（本地 ONNX 推理、显式模型下载）、`aero-app`（GPUI 窗口、编辑器、后台服务与 CLI）。模块说明见 [核心层 README](crates/aero-core/README.md)。
- 快捷键：`⌘O` 打开 vault，`⌘N` 新建笔记，`⌘⇧D` 今日日记，`⌘S` 保存，`⌘P` 快速打开（标题 / 别名 / 路径模糊匹配，目录加载完成后无匹配时 ⏎ 新建），`⌘⇧F` 全文搜索 / `⌘⇧E` 回到文件树，`⌘\` / `⌘⇧\` 折叠两侧面板，`⏎` 拆块 / 续列表，`⇧⏎` 软换行，`Esc` 编辑态 → 块选中态 → 浏览态，`⌘Z` / `⌘⇧Z` 文档级撤销重做，`⌘⌥0–8` 转换块类型（文本、标题 1–3、无序 / 有序 / 任务、引用、代码块），文件树里 `⌘⌫` 移到废纸篓。
- 块选中态（编辑态按 `Esc` 进入，或点块左侧悬停出现的 `⠿`）：`↑↓` 移动、`⇧↑↓` 扩展（编辑态里光标在首 / 末行时 `⇧↑↓` 直接升级为块选区）、`⏎` 回到编辑、`⌫` / `⌦` 删除整块（可撤销）、`⌘C` / `⌘X` 以 Markdown 源码复制 / 剪切。块左侧的 `+` 在下方插入新块并打开 `/` 菜单。
- 编辑器：块首或空格后输入 `/` 弹出块菜单（↑↓ 选、⏎ / Tab 确认、Esc 关）；点击标题可重命名（有 front matter `title` 时改 `title`）；文件树支持文件 / 文件夹拖放至目录或根目录，F2 / 右键重命名；笔记右键还可移到废纸篓 / 在 Finder 中显示 / 复制路径。
- 外部修改：vault 目录有文件监听（FSEvents），别的程序改动 / 新建 / 删除笔记会即时反映到文件树与索引。打开中的笔记被外部改动：没有未保存修改就静默重载并在文档顶部显示蓝色横幅；有则把磁盘版本另存为 `<name>.conflict-<时间>.md` 并显示黄色横幅（保留我的 / 使用磁盘版本 / 打开副本）；被外部删除则停止自动保存、内容留在编辑器里，红色横幅提供恢复 / 另存为 / 关闭。
- 搜索：结果按笔记分组、块级定位（点击滚到该块并高亮）；查询支持 `"短语"`、`+必含`、`-排除`、`#标签` / `tag:标签` / `-#标签`（多个标签须全部匹配；`#rust` 也匹配子标签 `rust/cli`；标签来自 front matter `tags:` 与正文 `#tag`，是笔记级的，只搜标签时每篇笔记列一条），中文用 jieba 分词；保存 / 重命名 / 删除 / 外部改动即时进索引。
- 编辑器：文字类块浏览和编辑共用控件，默认隐藏 Markdown 标记；选区工具条支持格式、颜色和块转换。表格可逐格编辑、拖列宽，任务方框可直接勾选，callout 支持折叠；复杂容器仍可能回退到源码编辑。块菜单支持颜色、转换与块引用操作，块可拖动排序。详见 [浏览与编辑一致验收](docs/qa/m2-seamless-edit.md) 和 [选区工具条](docs/qa/m2-toolbar.md)。
- 数学公式：`$$…$$` 块与行内 `$…$` 按 RaTeX 渲染成位图，跟随主题前景色与正文字号，KaTeX 字体内嵌；公式块编辑时显示实时预览，超宽公式可横向滚动。语法错误显示「源码 + 错误」，内容不丢。最初交付记录见 [数学渲染验收](docs/qa/m2-math.md)。
- 附件：文件树里有附件的笔记显示 `📎<数量>`，点开列出 `<note>.assets/` 下的附件（只读，双击用系统应用打开）；后台扫描引用计数，没被任何笔记引用的附件标「未引用」（嵌入、普通 Markdown 图片 / 链接、引用式定义都算，代码里的写法不算）。
- 笔记嵌入：`![[笔记]]` / `![[笔记#章节]]` / `![[笔记#^块]]` 独占一段时只读渲染目标内容（左侧竖线 + 来源标题，点击跳到来源并定位）；嵌入里的表格、callout、图片照常渲染，整篇嵌入不带目标的 front matter，嵌套只渲染一层。来源笔记改动后自动重读；解析不到或片段缺失显示「源码 + 错误」。
- 图片与渲染服务：`![[x.png|400]]` 独占一段时渲染位图，默认按正文列宽、不放大；选中后可选小 / 中 / 大 / 原始宽度，双击看图（`Esc` / `Space` 关闭）。渲染走后台服务、内存与磁盘缓存（`cache/<vault-id>/`），失败显示源码、错误和重试入口。`⌘V` 或 Finder 拖入可把附件写入 `<note>.assets/`，同名不覆盖；剪贴板里有文字时按文字粘贴。SVG / HEIC 图片附件尚不支持。见 [图片](docs/qa/m2-images.md)、[拖放导入](docs/qa/m2-drop.md) 与 [缓存预算](docs/qa/performance-followup-2026-09-26.md)。
- Mermaid 与 PDF：`mermaid` 围栏代码块渲染图表，编辑时实时预览，双击可导出 PNG / SVG 到本篇附件目录。`![[x.pdf#page=3]]` 可在笔记内连续滚动、输入页码或翻页、拖动阅读区域高度；明确翻页写回 `#page=`，滚动只改变阅读位置。双击 PDF 用系统阅读器打开。PDF 编辑、批注和正文索引尚未实现。
- 标签补全：在正文非行首、空格后输入 `#`，按使用篇数列出已有标签，支持中文 / 分级标签模糊匹配；`↑↓` 选择，`⏎` / `Tab` / 鼠标插入，`Esc` 关闭，可单步撤销。候选合并正文与 front matter 标签，并包含当前笔记的未保存内容。新版本首次启动会自动重建旧索引缓存以生成标签目录，Markdown 文件保持原样。详见 [标签补全验收](docs/qa/m2-tags.md)。
- 笔记链接补全：输入 `[[` 按标题 / 路径 / front matter `aliases` 模糊搜索，最近打开优先，显示最多 8 条；候选仍显示原 title，插入经 resolver 验证的规范目标。`↑↓` 选择，`⏎` / `Tab` / 鼠标插入，`⇧⏎` 插入并选中显示别名，`Esc` 关闭；无匹配时插入未解析链接。支持引用、callout 和表格源码中的链接；代码与数学内不触发。同名笔记使用经解析验证的路径，完整插入可一次撤销。详见 [补全验收及后续范围](docs/qa/m2-completion.md)。
- 标题链接补全：输入 `[[笔记#` 或 `[[#`，列出目标笔记或当前未保存笔记中的标题，支持中文模糊匹配、键盘 / 鼠标插入、显示别名和单步撤销。点击链接定位标题；支持 Setext、行内格式和折叠 callout 中的标题。详见 [标题补全验收](docs/qa/m2-headings.md)。
- 块引用：输入 `[[笔记#^` 或 `[[#^`，按块摘要 / 已有 ID 筛选；确认后复用 ID 或生成 6 位 ID 并写入目标，插入可点击的块链接。支持段落、列表、代码、表格、引用等；目标变化或 ID 重复时停止写入。同篇补 ID 与插链接可一次撤销，跨篇撤销保留目标 ID。详见 [块引用验收](docs/qa/m2-block-refs.md)。
- 嵌入补全：输入 `![[` 搜索笔记、图片和 PDF，优先显示最近笔记及本篇附件；支持 `.assets/`、中文 / 路径筛选、键盘 / 鼠标确认和单步撤销。`![[笔记#` / `![[笔记#^` 可补全标题和块，同篇可省笔记名。独占段落的图片、PDF 与笔记嵌入均已有渲染；与文字混排的嵌入仍显示源码。详见 [嵌入补全验收](docs/qa/m2-embeds.md)。
- 行内链接：选中文字按 `⌘K` 插入 `[文字](url)` 并选中地址，剪贴板是常见 URL 时自动填入；在已有行内链接中按 `⌘K` 只选中地址，保留文字与标题。无选区时打开笔记补全。支持中文、行内格式、表格 / callout 和单步撤销，也可从「编辑」菜单或命令面板调用。详见 [行内链接验收](docs/qa/m2-inline-links.md)。
- 行内格式：`⌘B` 粗体、`⌘I` 斜体、`⌘E` 行内代码、`⌘⇧S` 删除线。选中文字包裹并保留选区，再次执行取消；无选区插入空标记并把光标放中间。编辑菜单与命令面板均可调用，支持单步撤销 / 重做、中文、表格和链接文字。`⌘⇧H` 另用于应用上次使用的颜色（默认黄色背景），也可从颜色菜单与选区工具条挑选。详见 [行内格式验收](docs/qa/m2-inline-format.md) 与 [颜色验收](docs/qa/m2-colors.md)。
- 自动配对：逐键输入 `**`、`==`、反引号、`$`、`[[`、`(`、`"` 时补齐右侧，重复输入自动生成的闭合符只移动光标；空配对按退格一起删除，插入 / 删除可一次撤销。与笔记及嵌入补全共用闭括号，粘贴和输入法预编辑不额外配对；只跟踪当前编辑过程中生成的配对。详见 [自动配对验收](docs/qa/m2-pairs.md)。
- Wikilink：预览里 `[[目标]]` / `[[目标|别名]]` / `[[目标#标题]]` / `[[#标题]]` / `[[目标#^块ID]]` / `[[#^块ID]]` 可点击（`\|` 在表格里作别名分隔）。解析顺序：精确路径（相对链接所在目录、再相对 vault 根，可省 `.md`）→ 唯一文件名 → front matter `title` 与 `aliases` 同级匹配，不区分大小写；多篇命中是歧义，只提示候选、不创建；普通笔记链接无候选时在链接所在目录新建 `<目标>.md` 并打开，toast 可撤销（内容未动才移入废纸篓）。链接关系存在索引的 `links` 表里，目标被删 / 改名 / 改标题或 aliases 时自动重解析。
- 笔记别名：front matter `aliases` 接受字符串标量或字符串列表，trim 后按大小写不敏感去重；非法 YAML 不产生别名，不重写原文。`[[` 补全、`⌘P` 和全文搜索均支持 aliases，仍显示原 title；只有 front matter 的笔记也可按 alias 搜到，正文摘要为空，不泄露其他 front matter 字段。索引缓存按版本升级重建，Markdown 笔记不变。
- 链接悬停与视觉：悬停 500 ms 显示只读 Markdown 预览；普通笔记约前 300 个字符，标题 / `^块` 链接显示对应片段。同篇读取未保存文档，跨篇后台读盘；外部 URL 只显示地址，不联网。可在设置、视图菜单或命令面板中选择「悬停 / 按住 ⌘ / 关闭」。未解析链接为淡色虚线，歧义使用 warning 色虚线并提示候选。详见 [链接预览记录](docs/qa/m2-link-preview.md)。
- 反链：位于文档底部，标题计数入口和「视图 → 反链」滚动到引用区。按来源分组显示富文本片段，点击跳转来源块；当前块有唯一 `^id` 时可筛选「此块的引用」。计数独立于最多 200 条的展示上限；保存、改名、删除和外部修改进索引后刷新。不含同篇链接、代码、嵌入、未解析和歧义链接。未链接提及和完整分页仍未实现。历史验收见 [反链记录](docs/qa/m2-backlinks.md)。
- 移动与改名：文件树支持笔记 / 文件夹拖放、空目录、F2 和右键改名，附件目录随笔记迁移；同时更新入链和相对出链，覆盖 Wikilink、笔记嵌入、普通 Markdown 链接 / 图片 / 引用定义及显式附件路径。编辑器标题已有 front matter `title` 时同步标题引用，不改文件名；手改 YAML 不触发跨文件改写。保留显示文字、锚点、查询参数与无关字节；超过 20 个其他文件先确认，冲突拒绝、过期取消、失败回滚并保留恢复副本。详见 [移动与改名验收](docs/qa/m2-rename.md)。
- 工作区与设置：支持多标签页、收藏、面包屑导航；大纲在编辑区的导航浮层中，反链在文档底部。`⌘,` 打开设置对话框，提供编辑器、搜索与隐私、关于三个分组；字体、主题自定义、快捷键改键等仍待实现。

原生 IME、全面的 GUI 性能预算与正式签名公证仍需按对应验收记录关闭；功能已接入不等于里程碑整体完成。
