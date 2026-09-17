# Aero
<img src="assets/branding/aero-icon-v1.png" alt="Aero 图标" width="112" height="112">

A blazing fast, AI Native, local-first knowledge IDE.

## 运行（M1 开发版）

```sh
# 首次：下载并校验 zvec 预编译 C 库到 vendor/zvec/lib（全文索引引擎，构建期不再联网）
scripts/fetch-zvec.sh

# 打开一个文件夹作为 vault；不带参数则进入首次运行页 / 最近打开的 vault
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
- 快捷键（M1）：`⌘O` 打开 vault，`⌘N` 新建笔记，`⌘⇧D` 今日日记，`⌘S` 保存，`⌘P` 快速打开（标题 / 路径模糊匹配，无匹配时 ⏎ 新建），`⌘⇧F` 全文搜索 / `⌘⇧E` 回到文件树，`⌘\` / `⌘⇧\` 折叠两侧面板，`⏎` 拆块 / 续列表，`⇧⏎` 软换行，`Esc` 编辑态 → 块选中态 → 浏览态，`⌘Z` / `⌘⇧Z` 文档级撤销重做，`⌘⌥0–8` 转换块类型（文本、标题 1–3、无序 / 有序 / 任务、引用、代码块），文件树里 `⌘⌫` 移到废纸篓。
- 块选中态（编辑态按 `Esc` 进入，或点块左侧悬停出现的 `⠿`）：`↑↓` 移动、`⇧↑↓` 扩展（编辑态里光标在首 / 末行时 `⇧↑↓` 直接升级为块选区）、`⏎` 回到编辑、`⌫` / `⌦` 删除整块（可撤销）、`⌘C` / `⌘X` 以 Markdown 源码复制 / 剪切。块左侧的 `+` 在下方插入新块并打开 `/` 菜单。
- 编辑器：块首或空格后输入 `/` 弹出块菜单（↑↓ 选、⏎ / Tab 确认、Esc 关）；点击标题可重命名（有 front matter `title` 时改 `title`）；文件树右键：移到废纸篓 / 在 Finder 中显示 / 复制路径。
- 外部修改：vault 目录有文件监听（FSEvents），别的程序改动 / 新建 / 删除笔记会即时反映到文件树与索引。打开中的笔记被外部改动：没有未保存修改就静默重载并在文档顶部显示蓝色横幅；有则把磁盘版本另存为 `<name>.conflict-<时间>.md` 并显示黄色横幅（保留我的 / 使用磁盘版本 / 打开副本）；被外部删除则停止自动保存、内容留在编辑器里，红色横幅提供恢复 / 另存为 / 关闭。
- 搜索：结果按笔记分组、块级定位（点击滚到该块并高亮）；查询支持 `"短语"`、`+必含`、`-排除`、`#标签` / `tag:标签` / `-#标签`（多个标签须全部匹配；`#rust` 也匹配子标签 `rust/cli`；标签来自 front matter `tags:` 与正文 `#tag`，是笔记级的，只搜标签时每篇笔记列一条），中文用 jieba 分词；保存 / 重命名 / 删除 / 外部改动即时进索引。
- 编辑器只读渲染：`> [!note]±` callout（标题栏 + 折叠按钮，折叠状态不写回源码）、表格（横向滚动）。
- Wikilink：预览里 `[[目标]]` / `[[目标|别名]]` / `[[目标#标题]]` / `[[#标题]]` 可点击（`\|` 在表格里作别名分隔）。解析顺序：精确路径（相对链接所在目录、再相对 vault 根，可省 `.md`）→ 唯一文件名 → 唯一 front matter `title`，不区分大小写；同名多篇是歧义，只提示候选、不创建；无候选时在链接所在目录新建 `<目标>.md` 并打开，toast 可撤销（内容未动才移入废纸篓）。链接关系存在索引的 `links` 表里，目标被删 / 改名 / 改标题时自动重解析。
- 反链：右侧可切换「反链 / 大纲」。反链按来源笔记分组，显示正文摘要并强调链接文字；点击打开来源并短暂高亮引用所在块。保存、重命名、删除与外部文件变化完成索引后自动刷新；来源已变化时只打开笔记，不使用旧位置。当前最多显示 200 条已解析的笔记间引用（不含同篇链接、代码、嵌入和歧义链接）。验收范围见 [反链记录](docs/qa/m2-backlinks.md)。
- 重命名：点击没有 front matter `title` 的笔记标题修改文件名，会同步更新 vault 内实际指向该笔记的 `[[...]]` 和笔记嵌入 `![[...]]`，保留别名、标题 / 块锚点及其余原始字节。超过 20 个其他文件时先确认；版本变化则取消，提交失败尝试回滚并保留 `.aero/rename-<id>/` 恢复副本。详见 [重命名验收及范围](docs/qa/m2-rename.md)。

---

tar zcvf aero.m0.aero-core.tar.gz --exclude=Aero/target Aero/

docker run -d --name aero --hostname aero -v "$HOME":"/workspace" -w "/workspace" ubuntu:24.04 sleep infinity
