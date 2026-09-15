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
```

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

- 设计文档：`docs/project_plan.md`（技术方案）、`docs/ui_design.md`（UI 与交互）。
- 代码分层：`crates/aero-core`（文本 / 块 / 解析 / vault / 索引块抽取，无 UI）→ `crates/aero-index`（zvec 全文索引 + SQLite 元数据 + 增量同步）→ `crates/aero-app`（GPUI 窗口、文件树、块编辑器、自动保存、搜索）。
- 快捷键（M1）：`⌘O` 打开 vault，`⌘N` 新建笔记，`⌘⇧D` 今日日记，`⌘S` 保存，`⌘P` 快速打开（标题 / 路径模糊匹配，无匹配时 ⏎ 新建），`⌘⇧F` 全文搜索 / `⌘⇧E` 回到文件树，`⌘\` / `⌘⇧\` 折叠两侧面板，`⏎` 拆块 / 续列表，`⇧⏎` 软换行，`Esc` 回到浏览态，`⌘Z` / `⌘⇧Z` 文档级撤销重做，`⌘⌥0–8` 转换块类型（文本、标题 1–3、无序 / 有序 / 任务、引用、代码块），文件树里 `⌘⌫` 移到废纸篓。
- 编辑器：块首或空格后输入 `/` 弹出块菜单（↑↓ 选、⏎ / Tab 确认、Esc 关）；点击标题可重命名（有 front matter `title` 时改 `title`）；文件树右键：移到废纸篓 / 在 Finder 中显示 / 复制路径。
- 搜索：结果按笔记分组、块级定位（点击滚到该块并高亮）；查询支持 `"短语"`、`+必含`、`-排除`，中文用 jieba 分词；保存 / 重命名 / 删除即时进索引。

---

tar zcvf aero.m0.aero-core.tar.gz --exclude=Aero/target Aero/

docker run -d --name aero --hostname aero -v "$HOME":"/workspace" -w "/workspace" ubuntu:24.04 sleep infinity
