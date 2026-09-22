# Aero
<img src="assets/branding/aero-icon-v1.png" alt="Aero 图标" width="112" height="112">

A blazing fast, AI Native, local-first knowledge IDE.

## 运行（M1 开发版）

```sh
# 首次：下载并校验 zvec 预编译 C 库到 vendor/zvec/lib（全文索引引擎，构建期不再联网）
scripts/fetch-zvec.sh

# 打开指定文件夹作为 vault；不带参数则恢复最近打开的库，没有则停在库选择页
cargo run -p aero-app -- ~/Knowledge

# 日志（默认只输出 warn）
AERO_LOG=debug cargo run -p aero-app -- ~/Knowledge

# 测试：aero-core 往返测试、aero-index 索引往返（含真 zvec 引擎）、aero-app 的纯函数与无头 GPUI 交互测试
cargo test -p aero-core -p aero-index -p aero-app

# M1 自动验收（格式、测试、从当前源码打包及 DMG 检查）
bash scripts/check-m1.sh --bundle
```

M1 验收状态及复跑说明见 [docs/qa/m1.md](docs/qa/m1.md)，包含保存失败回归、输入法清单和真实性能基准；自动化通过不代表原生 IME、GUI 性能与正式公证全部通过。

全文索引存放在 `~/Library/Application Support/Aero/index/<vault-id>/`，是可随时删除重建的缓存；首次打开 vault 时后台建索引，状态栏显示进度。

无参数启动恢复最近打开且目录还在的库（已删除的条目跳过，不弹错误）；首次运行或没有可恢复的库时停在库选择页。命令行指定路径直接打开该库。选择页除了「打开文件夹…」「新建 Vault…」和最近列表，还有「查看示例库」：内置六篇中文示例位于 `assets/demo-vault/`，编译时嵌入可执行文件，点击时才展开到 `~/Library/Application Support/Aero/demo-vault/` 并停在欢迎页。最近列表里的示例库（包括启动时自动恢复它）走同一个入口，所以每次进去都是当前版本的示例，落点也始终是欢迎页。

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

凭据只从环境变量读取，脚本不保存任何秘密。M1 没有自动更新：发布就是上传 `.dmg`。

- 设计文档：`docs/project_plan.md`（技术方案）、`docs/ui_design.md`（UI 与交互）。
- 代码分层：`crates/aero-core`（文本 / 块 / 解析 / vault / 索引块抽取，无 UI）→ `crates/aero-index`（zvec 全文索引 + SQLite 元数据 + 增量同步）→ `crates/aero-app`（GPUI 窗口、文件树、块编辑器、自动保存、搜索）。
- 快捷键（M1）：`⌘O` 打开 vault，`⌘N` 新建笔记，`⌘⇧D` 今日日记，`⌘S` 保存，`⌘P` 快速打开（标题 / 别名 / 路径模糊匹配，目录加载完成后无匹配时 ⏎ 新建），`⌘⇧F` 全文搜索 / `⌘⇧E` 回到文件树，`⌘\` / `⌘⇧\` 折叠两侧面板，`⏎` 拆块 / 续列表，`⇧⏎` 软换行，`Esc` 编辑态 → 块选中态 → 浏览态，`⌘Z` / `⌘⇧Z` 文档级撤销重做，`⌘⌥0–8` 转换块类型（文本、标题 1–3、无序 / 有序 / 任务、引用、代码块），文件树里 `⌘⌫` 移到废纸篓。
- 块选中态（编辑态按 `Esc` 进入，或点块左侧悬停出现的 `⠿`）：`↑↓` 移动、`⇧↑↓` 扩展（编辑态里光标在首 / 末行时 `⇧↑↓` 直接升级为块选区）、`⏎` 回到编辑、`⌫` / `⌦` 删除整块（可撤销）、`⌘C` / `⌘X` 以 Markdown 源码复制 / 剪切。块左侧的 `+` 在下方插入新块并打开 `/` 菜单。
- 编辑器：块首或空格后输入 `/` 弹出块菜单（↑↓ 选、⏎ / Tab 确认、Esc 关）；点击标题可重命名（有 front matter `title` 时改 `title`）；文件树支持文件 / 文件夹拖放至目录或根目录，F2 / 右键重命名；笔记右键还可移到废纸篓 / 在 Finder 中显示 / 复制路径。
- 外部修改：vault 目录有文件监听（FSEvents），别的程序改动 / 新建 / 删除笔记会即时反映到文件树与索引。打开中的笔记被外部改动：没有未保存修改就静默重载并在文档顶部显示蓝色横幅；有则把磁盘版本另存为 `<name>.conflict-<时间>.md` 并显示黄色横幅（保留我的 / 使用磁盘版本 / 打开副本）；被外部删除则停止自动保存、内容留在编辑器里，红色横幅提供恢复 / 另存为 / 关闭。
- 搜索：结果按笔记分组、块级定位（点击滚到该块并高亮）；查询支持 `"短语"`、`+必含`、`-排除`、`#标签` / `tag:标签` / `-#标签`（多个标签须全部匹配；`#rust` 也匹配子标签 `rust/cli`；标签来自 front matter `tags:` 与正文 `#tag`，是笔记级的，只搜标签时每篇笔记列一条），中文用 jieba 分词；保存 / 重命名 / 删除 / 外部改动即时进索引。
- 编辑器只读渲染：`> [!note]±` callout（标题栏 + 折叠按钮，折叠状态不写回源码）、表格（横向滚动）、图片。
- 笔记嵌入：`![[笔记]]` / `![[笔记#章节]]` / `![[笔记#^块]]` 独占一段时只读渲染目标内容（左侧竖线 + 来源标题，点击跳到来源并定位）；嵌入里的表格、callout、图片照常渲染，整篇嵌入不带目标的 front matter，嵌套只渲染一层。来源笔记改动后自动重读；解析不到或片段缺失显示「源码 + 错误」。
- 图片与渲染服务：`![[x.png|400]]` 独占一段时渲染位图（`|400` 为显示宽度，默认按正文列宽、不放大），单击选中、双击全屏看图（`Esc` / `Space` 关闭）。渲染走统一的重内容服务：内存缓存 + `~/Library/Application Support/Aero/cache/<vault-id>/` 下按内容指纹存放的磁盘缓存（可随时删除），解码与缩放在后台线程，首帧占位、失败显示「源码 + 错误 + 重试」。`⌘V` 把剪贴板位图写进 `<note>.assets/` 并插入嵌入，Finder 复制的文件按原名导入且不覆盖同名；剪贴板里有文字时仍按文字粘贴。`.svg` / `.heic` 与拖放导入尚未实现。详见 [渲染服务与图片验收](docs/qa/m2-images.md)。
- 标签补全：在正文非行首、空格后输入 `#`，按使用篇数列出已有标签，支持中文 / 分级标签模糊匹配；`↑↓` 选择，`⏎` / `Tab` / 鼠标插入，`Esc` 关闭，可单步撤销。候选合并正文与 front matter 标签，并包含当前笔记的未保存内容。新版本首次启动会自动重建旧索引缓存以生成标签目录，Markdown 文件保持原样。详见 [标签补全验收](docs/qa/m2-tags.md)。
- 笔记链接补全：输入 `[[` 按标题 / 路径 / front matter `aliases` 模糊搜索，最近打开优先，显示最多 8 条；候选仍显示原 title，插入经 resolver 验证的规范目标。`↑↓` 选择，`⏎` / `Tab` / 鼠标插入，`⇧⏎` 插入并选中显示别名，`Esc` 关闭；无匹配时插入未解析链接。支持引用、callout 和表格源码中的链接；代码与数学内不触发。同名笔记使用经解析验证的路径，完整插入可一次撤销。详见 [补全验收及后续范围](docs/qa/m2-completion.md)。
- 标题链接补全：输入 `[[笔记#` 或 `[[#`，列出目标笔记或当前未保存笔记中的标题，支持中文模糊匹配、键盘 / 鼠标插入、显示别名和单步撤销。点击链接定位标题；支持 Setext、行内格式和折叠 callout 中的标题。详见 [标题补全验收](docs/qa/m2-headings.md)。
- 块引用：输入 `[[笔记#^` 或 `[[#^`，按块摘要 / 已有 ID 筛选；确认后复用 ID 或生成 6 位 ID 并写入目标，插入可点击的块链接。支持段落、列表、代码、表格、引用等；目标变化或 ID 重复时停止写入。同篇补 ID 与插链接可一次撤销，跨篇撤销保留目标 ID。详见 [块引用验收](docs/qa/m2-block-refs.md)。
- 嵌入补全：输入 `![[` 搜索笔记、图片和 PDF，优先显示最近笔记及本篇附件；支持 `.assets/`、中文 / 路径筛选、键盘 / 鼠标确认和单步撤销。`![[笔记#` / `![[笔记#^` 可补全标题和块，同篇可省笔记名。当前完成嵌入源码插入，图片、PDF 与笔记内容的实际渲染尚未实现。详见 [嵌入补全验收](docs/qa/m2-embeds.md)。
- 行内链接：选中文字按 `⌘K` 插入 `[文字](url)` 并选中地址，剪贴板是常见 URL 时自动填入；在已有行内链接中按 `⌘K` 只选中地址，保留文字与标题。无选区时打开笔记补全。支持中文、行内格式、表格 / callout 和单步撤销，也可从「编辑」菜单或命令面板调用。详见 [行内链接验收](docs/qa/m2-inline-links.md)。
- 行内格式：`⌘B` 粗体、`⌘I` 斜体、`⌘E` 行内代码、`⌘⇧S` 删除线、`⌘⇧H` 黄色高亮。选中文字包裹并保留选区，再次执行取消；无选区插入空标记并把光标放中间。编辑菜单与命令面板均可调用，支持单步撤销 / 重做、中文、表格和链接文字。详见 [行内格式验收](docs/qa/m2-inline-format.md)。
- 自动配对：逐键输入 `**`、`==`、反引号、`$`、`[[`、`(`、`"` 时补齐右侧，重复输入自动生成的闭合符只移动光标；空配对按退格一起删除，插入 / 删除可一次撤销。与笔记及嵌入补全共用闭括号，粘贴和输入法预编辑不额外配对；只跟踪当前编辑过程中生成的配对。详见 [自动配对验收](docs/qa/m2-pairs.md)。
- Wikilink：预览里 `[[目标]]` / `[[目标|别名]]` / `[[目标#标题]]` / `[[#标题]]` / `[[目标#^块ID]]` / `[[#^块ID]]` 可点击（`\|` 在表格里作别名分隔）。解析顺序：精确路径（相对链接所在目录、再相对 vault 根，可省 `.md`）→ 唯一文件名 → front matter `title` 与 `aliases` 同级匹配，不区分大小写；多篇命中是歧义，只提示候选、不创建；普通笔记链接无候选时在链接所在目录新建 `<目标>.md` 并打开，toast 可撤销（内容未动才移入废纸篓）。链接关系存在索引的 `links` 表里，目标被删 / 改名 / 改标题或 aliases 时自动重解析。
- 笔记别名：front matter `aliases` 接受字符串标量或字符串列表，trim 后按大小写不敏感去重；非法 YAML 不产生别名，不重写原文。`[[` 补全、`⌘P` 和全文搜索均支持 aliases，仍显示原 title；只有 front matter 的笔记也可按 alias 搜到，正文摘要为空，不泄露其他 front matter 字段。SQLite schema 升到 5，FTS 缓存版本同步提升；旧缓存重建，Markdown 笔记不变。
- 链接悬停与视觉：悬停 500 ms 显示纯文本、只读、可滚动预览；普通笔记取前 300 个 Unicode 字符，标题 / `^块` 链接只取对应片段。同篇读取未保存文档，跨篇后台读盘；外部 URL 只显示地址，不联网。离开链接有 150 ms 宽限以进入浮层，编辑器滚动 / 缩放 / 失焦 / 编辑 / 重载 / 删除会取消，generation 过滤旧回复。未解析链接为淡色虚线，歧义为 warning 色虚线并提示候选；跨篇缺失 fragment 到悬停读盘时才校验，暂无即时淡色保证。样式与 hover 使用仓库内 `forks/gpui-base` patch，不修改全局 registry。详见 [链接预览验收范围](docs/qa/m2-inline-links.md)。
- 反链：标题栏计数入口可打开右侧「反链」面板，也可切换「反链 / 大纲」。按来源笔记分组显示正文摘要并强调链接文字；点击打开来源并短暂高亮引用所在块。标题入口与面板 total 来自独立 COUNT，不受最多展示 200 条的限制；当前块有唯一 `^id` 时可切换「此块的引用」，筛选在 LIMIT 之前。保存、重命名、删除与外部文件变化完成索引后自动刷新；来源已变化时只打开笔记，不使用旧位置。不含同篇链接、代码、嵌入、未解析和歧义链接；仍未实现未链接提及、富文本反链片段及超过 200 条的完整列表。验收范围见 [反链记录](docs/qa/m2-backlinks.md)。
- 移动与改名：文件树支持笔记 / 文件夹拖放、空目录、F2 和右键改名，附件目录随笔记迁移；同时更新入链和相对出链，覆盖 Wikilink、笔记嵌入、普通 Markdown 链接 / 图片 / 引用定义及显式附件路径。编辑器标题已有 front matter `title` 时同步标题引用，不改文件名；手改 YAML 不触发跨文件改写。保留显示文字、锚点、查询参数与无关字节；超过 20 个其他文件先确认，冲突拒绝、过期取消、失败回滚并保留恢复副本。详见 [移动与改名验收](docs/qa/m2-rename.md)。

2026-09-21：上述四项笔记互联功能已接入，全量 395 项自动化测试通过，详见 [验证记录](docs/qa/m2-completion.md)。富文本预览、预览设置、原生人工视觉与大库性能仍待完成，M2 尚未整体完成；未替换已安装应用。

---

tar zcvf aero.m0.aero-core.tar.gz --exclude=Aero/target Aero/

docker run -d --name aero --hostname aero -v "$HOME":"/workspace" -w "/workspace" ubuntu:24.04 sleep infinity
