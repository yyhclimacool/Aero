# Aero · Local-First AI Knowledge Workspace 技术方案

> 状态：方案 v0.13（2026-10-10，完成 Zed 对照计划的 O2 剩余部分与 O3–O6）
> 投入假设：单人、业余时间（约每周 10 小时）。排期见第 18 节。
> 本文同时保留设计目标与实施记录；标为「规划」的能力尚未交付，带日期的进展与测试数字只代表当时范围。M0 技术选型已完成，见 `decisions/`；当前使用入口见 [README](../README.md)。
> 2026-09-22：默认编辑观感定为「所见即所得，源码按需打开」，见 2.2、9.1 与第 22 节第 20 条。后续编辑器开发以 9.1 为准。
> UI 与交互设计见 `docs/ui_design.md`。
> 当前状态（2026-09-26）：编辑、全文搜索、知识互联、CLI、Mermaid / PDF 与本地语义混合搜索已接入；MCP 与链接卡片未实现。最近验证见 [性能续作记录](qa/performance-followup-2026-09-26.md)，原生 IME、性能预算与正式分发仍以 [M1 验收记录](qa/m1.md) 的待验项为准。

---

## 1. 项目定位

目标是开发一款兼具 **Notion 的编辑体验、Obsidian 的本地开放性、Zed 的性能与工程理念，以及 AI-native 检索能力** 的本地知识管理软件。

产品不以"Notion Clone"或"Obsidian Clone"为目标，而是定位为：

> **Local-First AI Knowledge Workspace / Knowledge IDE**

核心理念：

- 数据首先属于用户，而不是应用。
- Markdown/纯文本是知识的长期载体。
- Block 是编辑和索引的基本单位。
- 本地文件是 Source of Truth。
- 搜索索引、向量索引、数据库均为可重建缓存。
- AI 是知识库的另一个 Client，而不是数据存储层。
- 应用即使消失，用户的数据仍应具有完整可读性。

格式路线明确为 **扩展 Markdown**：不发明私有格式，在 CommonMark/GFM 之上分层叠加扩展语法，且每一层扩展都必须在第三方 Markdown 工具中"可降级阅读"。详见第 6 节。

Notion、Obsidian、Zed 都只是思路参考。特别地，**与 Obsidian 的兼容不是目标**：借鉴它的语法是因为这些设计本身好且可降级，需要偏离时可以偏离。

---

## 2. 核心需求

产品需要优先解决以下六个问题。

### 2.1 极致本地性能

所有正常文档浏览、编辑、搜索都不依赖网络。

目标体验：

- 启动后立即可用
- 打开文档无网络等待
- 搜索结果毫秒级返回
- 大型知识库仍保持流畅
- 编辑过程不等待数据库或云服务

架构采用：

```text
Local Files
    ↓
Local Index
    ↓
Local Application
```

云端只承担：

```text
Sync / Backup / Optional AI
```

而不是：

```text
Cloud
 ↓
Application
```

具体性能预算见第 19 节。

### 2.2 Notion 级的块编辑体验

- 以 Block 为单位组织、选择、拖拽、转换类型。
- 默认不把 Markdown 暴露给用户。选出一段文字，弹出的是格式（粗体、颜色、块类型），不是源码标记。`/` 与 `# `、`- `、`> `、`**` 是输入加速键，打完立刻变成块或样式。
- 磁盘上 Markdown 仍是唯一存储。源码按块或按篇按需打开，并可把「聚焦时显示源码」设为偏好。这是同一个编辑器的两种呈现，不做第二种「纯 Markdown 编辑器」。细则见 9.1。
- 中文输入法（IME）是一等公民，而不是事后修补。

### 2.3 开放、可降级的扩展 Markdown 格式

- 文件用任何编辑器都能读、能改。
- 扩展语法优先借鉴社区已广泛使用的约定（Obsidian、GitHub 等），不自造轮子；但不以任何工具的兼容为约束。
- 应用只重写用户实际编辑过的部分，绝不"顺手"重排未改动的文本。

### 2.4 知识互联

- 双向链接、块引用、嵌入、标签。
- 反向链接和未解析链接实时可见。
- 图谱是索引之上的查询视图，而非独立数据。

### 2.5 AI-native 检索与接入

- 全文检索 + 语义检索，索引粒度到 Block。
- 通过 MCP 与 CLI 把知识库暴露给任意 AI 客户端；v1 不做应用内对话/摘要，AI 能力全部经由 MCP/CLI。
- Embedding 默认本地；明确哪些数据会离开本机。

### 2.6 对外部修改与同步友好

- 文件被其他工具或同步软件修改时，应用能正确感知、合并、不覆盖。
- 索引和缓存不进入 vault，不给同步工具制造噪音。
- 不绑定任何同步方案。

---

## 3. 总体技术栈

```text
Rust (edition 2024)
│
├── GPUI + gpui-component / gpui-base      UI 框架、组件、文本引擎
│   ├── Window / Rendering
│   ├── Virtual List
│   ├── Keyboard / Focus / IME
│   └── Block Editor（自研，见第 9 节）
│
├── Rope + Anchors                          文档文本模型（第 7 节；ropey 2.0 + marker 表，ADR 0004）
│
├── Markdown Parser                         comrak（默认）/ markdown-rs（备选），第 8 节
│
├── Document / Block Model                  第 7 节
│
├── SQLite (rusqlite)                       元数据、链接、标签、索引状态
│
├── 全文 + 向量索引                         zvec-rust（一套引擎，ADR 0003）
│                                           jieba 分词，dylib 随应用分发（第 10、16 节）
│
├── Embedding（已接入）                     ort + tokenizers，本地 multilingual-e5-small int8
│
├── 重内容渲染（第 9.9 节）
│   ├── RaTeX                               LaTeX 数学（纯 Rust KaTeX 移植）
│   ├── merman                              mermaid（headless 纯 Rust）
│   ├── hayro                               PDF 页面光栅化（纯 Rust）
│   ├── resvg                               SVG → 位图
│   └── 链接卡片元数据抓取（规划）
│
├── notify + ignore                         文件监听、vault 扫描
│
├── GPUI Executor（主）/ Tokio（受限）      异步模型见第 15 节
│
├── MCP Server (rmcp，规划)                 AI 接入
│
└── CLI (clap)                              脚本与 AI 的通用接口
```

### 3.1 UI 层

```text
gpui-kit（crates.io 单一依赖，自带匹配版本的 GPUI）
├── GPUI（以 gpui-pre 发布）
├── gpui-base（无样式基础层：Input / Textarea / Editor / 文本引擎）
└── gpui-component（有样式组件：列表、菜单、对话框、VirtualList 等）
+
自研 Design System
```

原则：

> 学习 Zed，复用 GPUI，但不要把产品变成 Zed Fork。

不 fork Zed 的原因有二：工程上 Zed 的 `editor` 与其 `project`/`language`/`multi_buffer` 深度耦合，剥离成本高于自研；许可上 GPUI 是 Apache-2.0，而 Zed 的 `editor` 等 crate 是 GPL/AGPL，引入会改变产品的许可边界。

GPUI 处于 pre-1.0，版本间有 breaking change。上游已把 GPUI + gpui-base + gpui-component 打包为 crates.io 上的 `gpui-kit`（M0 验证时为 0.6.1，见 ADR 0001），不再需要 git 依赖。策略：在根 `Cargo.toml` 精确锁定版本（`=x.y.z`），按季度评估升级，升级作为独立任务而非顺带完成。

### 3.2 Crate 分层

当前四个正式 crate：

```text
aero/
├── crates/
│   ├── aero-core        文本、块模型、Markdown、vault、链接、附件、编辑与索引抽取；无 UI
│   ├── aero-index       zvec 全文 / 向量索引、SQLite 元数据、增量同步与 RRF 融合
│   ├── aero-embedding   ort + tokenizers，本地 CPU 推理与固定版本模型的显式下载
│   └── aero-app         GPUI 桌面应用、后台服务与 CLI（src/cli.rs）
├── forks/gpui-base/     应用使用的文本控件补丁
├── spikes/             保留的 M0 实验，仍是 workspace 成员
└── docs/
```

GUI 与 CLI 共用核心层和索引层；CLI 目前与 GUI 编入同一个 `aero` 二进制，没有独立 `aero-cli` crate。`aero-mcp` 仍是规划。Tantivy 未采用。

---

## 4. 核心架构原则

```text
Markdown = Source of Truth

Block = Editing Unit

Index = Disposable Cache

AI = Another Client
```

由此推出的四条工程纪律：

1. **只重写被编辑的 Block。** 未改动的字节原样写回，序列化器没有"格式化整篇文档"的入口。
2. **未知语法原样保留。** 解析不了的内容成为 `Raw` block，渲染为源码，写回时逐字节一致。
3. **每个扩展语法必须可降级。** 在 GitHub / 任意 Markdown 编辑器 / 纯文本编辑器中打开，内容仍然可读、顺序不乱。
4. **索引不进 vault。** vault 内只放小体积、值得同步的配置。

---

## 5. 数据存储设计

### 5.1 Vault

一个知识库本质上就是普通文件夹：

```text
~/Knowledge/
├── daily/
│   ├── 2026-09-13.md
│   └── 2026-09-14.md
├── rust/
│   ├── ripgrep.md
│   ├── ripgrep.assets/
│   │   ├── 01k5a8.png
│   │   └── architecture.svg
│   ├── cargo.md
│   └── tokio.md
├── projects/
│   └── knowledge-app.md
└── .aero/
    ├── vault.toml
    ├── config.toml
    └── templates/
```

真正不可丢的数据是：

```text
*.md
*.assets/**
.aero/*.toml（可选，丢了只损失偏好设置）
```

### 5.2 `.aero/` 只放配置

```text
.aero/
├── vault.toml       vault_id（ULID）、格式版本
├── config.toml      附件目录、链接解析偏好、新建内容的序列化风格
└── templates/       新建笔记模板
```

其中 `vault_id` 用来把 vault 与应用数据目录里的索引关联起来。

### 5.3 索引与缓存放在应用数据目录

macOS 根目录为 `~/Library/Application Support/Aero/`；Linux 使用 `$XDG_DATA_HOME/aero`（默认 `~/.local/share/aero/`），Windows 使用 `%APPDATA%\Aero\`。当前布局：

```text
Aero/
├── index/<vault-id>/
│   ├── meta.sqlite          全文索引元数据、链接、标签
│   ├── fts/                 zvec 全文 Collection
│   └── semantic/            独立向量 Collection 与 metadata.sqlite
├── cache/<vault-id>/        图片、公式、图表和 PDF 位图
├── models/<固定模型版本>/   下载并校验的 ONNX 模型与 tokenizer
├── state.toml              最近库、标签页、收藏、面板与编辑器偏好
├── search.toml             模型下载许可、语义搜索与暂停状态
└── demo-vault/             可编辑示例库
```

索引与渲染缓存可在退出应用后删除重建，不进入 vault。模型可重新下载，但删除后离线语义搜索需等模型重新就绪。`state.toml`、`search.toml` 是用户偏好，`demo-vault/` 是真实笔记，都不属于可丢弃的索引缓存；不要为清索引而删除整个应用数据目录。示例库更新规则见 [README](../README.md)。

Debug 构建支持用 `AERO_DATA_DIR` 指向隔离目录，供测试使用；release 构建不使用该覆盖。

### 5.4 附件

- **默认每篇笔记一个附件子目录**：`<note>.md` 的附件放在同级的 `<note>.assets/`。加 `.assets` 后缀而不是直接同名，是为了与"同名笔记文件夹"（如 `rust/tokio/` 下还有笔记）区分，文件树中可以把 `*.assets/` 折叠到所属笔记之下。
- 可在 `config.toml` 改为 vault 根 `assets/` 或笔记同级目录。
- 粘贴图片：写入所属笔记的附件目录，文件名 `<ulid-short>.<ext>`，插入 `![[01k5a8.png]]`。
- 图片/附件路径解析顺序：所属笔记的 `.assets/` → 相对笔记所在目录 → vault 内唯一文件名。
- 重命名/移动笔记时一并移动其 `.assets/` 目录并更新引用；删除笔记时若附件目录非空，提示用户一并删除或保留。
- 附件目录只被建立索引（用于引用计数与孤儿附件检测），内容不进全文索引。

### 5.5 文件写入策略

- 自动保存，编辑停止 500 ms 后落盘；失焦、切换文档、退出时立即落盘。
- 原子写：写临时文件后 `rename`。保留原文件权限；因 `rename` 会改变 inode，对依赖 inode 的监听器做重新注册。
- 保留原文件的行尾风格（LF/CRLF）、BOM、末尾换行；新文件统一 LF、末尾单换行、无 BOM。
- 写入前校验磁盘上的 mtime/hash 与上次读取一致，不一致进入冲突流程（第 12 节）。

---

## 6. Knowledge Markdown 规范

### 6.1 设计原则

Knowledge Markdown 是分层的：

```text
L2  Aero 扩展层      directives、数据块         —— v1 不启用，M4 之后再评估
L1  惯用扩展层       wikilink、embed、^id、#tag、callout、==、%%、$math$
L0  基础层           CommonMark + GFM + YAML Front Matter
```

规则：

1. 下层的语义不因上层扩展而改变；上层解析失败时退回下层解释。
2. 所有行内扩展在代码块、行内代码、数学、链接 URL 内一律不生效。
3. L1 借鉴 Obsidian、GitHub 等社区已广泛使用的语法，取其设计而非追求兼容；需要偏离时在本节写明理由。只有没有现成惯例时才考虑 L2。
4. 每个扩展必须在本节写明"降级表现"。

### 6.2 L0：CommonMark + GFM + Front Matter

- CommonMark 0.31 全集。
- GFM 扩展：表格、任务列表、删除线、自动链接、脚注。
- YAML Front Matter：文件首行 `---` 开始的块。保留键：

```yaml
---
id: 01K5A8Z3QF7P9R2M4N6T8V0X1Y   # ULID，可选，按需生成（见 6.3.4）
title: ripgrep                   # 缺省取一级标题或文件名
aliases: [rg]
tags: [rust, cli]
created: 2026-09-14              # 仅在应用内新建笔记时写入一次
---
```

- `created` 只在新建时写入（可在 `config.toml` 关闭）；**不自动维护 `updated`**，避免每次保存都改动文件头、污染 diff。"最近修改"一律取文件 mtime。用户手写的 `updated` 键原样保留，不解释、不覆盖。
- 未知键原样保留。应用修改 front matter 时只改动目标键所在行，不重新序列化整块 YAML。

### 6.3 L1：惯用扩展层

以下语法借鉴自 Obsidian 等工具的社区惯例。它们在这些工具中"恰好"能原生渲染是附带收益，不是设计约束。

#### 6.3.1 Wiki Links

```text
[[target]]
[[target|显示文本]]
[[target#标题]]
[[target#^block-id]]
```

解析顺序：精确路径（含或不含 `.md`）→ 唯一文件名 → front matter `title`/`aliases` → 多个候选时视为歧义，编辑器提示消歧 → 无候选为未解析链接（渲染为可点击创建）。重命名文件时全库更新链接文本；若目标笔记有 `id`，链接索引同时记录 id，作为路径变化时的兜底解析。

降级：GitHub 显示为字面 `[[target]]`；支持 wikilink 的编辑器可原生渲染。

#### 6.3.2 Embeds

```text
![[note]]                   嵌入整篇
![[note#标题]]              嵌入章节
![[note#^block-id]]         嵌入单个块
![[01k5a8.png]]             图片（按 5.4 的顺序解析路径）
![[01k5a8.png|400]]         指定宽度
```

嵌入内容只读渲染，点击进入源文档编辑。循环嵌入检测深度上限 5。

降级：字面显示。

#### 6.3.3 Block IDs

```text
这是一个段落。 ^para-id

- 列表项
- 列表项

^list-id
```

- 段落：`^id` 置于段落最后一行末尾。
- 多行块（列表、代码块、表格、引用）：`^id` 独占一行，与块之间空一行。
- 字符集 `[A-Za-z0-9-]`，自动生成为 6 位 base32。

**持久化策略：只有被引用的块才写入 `^id`。** 用户在其他地方引用某块时，编辑器自动生成并写入；删除所有引用后 `^id` 保留（避免破坏外部工具的链接），用户可手动清理。未被引用的块不会在文件中留下任何标记。

降级：显示为末尾的 `^xxx` 字面文本。

#### 6.3.4 Note ID（front matter `id`）

- 类型 ULID。
- **按需生成**，触发条件：用户显式要求；被其他笔记以 id 引用；配置 `always_assign_id = true`。
- 打开已有 vault 时绝不批量注入 `id`。

#### 6.3.5 Tags

```text
#rust  #rust/cli  #中文标签
```

行内标签与 front matter `tags` 等价，索引合并。规则：`#` 后首字符不能是数字；允许 Unicode 字母、`-`、`_`、`/`；标题行内的 `#` 不是标签。

降级：字面文本。

#### 6.3.6 Highlight 与 Comment

```text
==高亮文本==
%%不渲染、不导出的注释%%
```

`%%` 内容进入全文索引但标记为 comment，搜索结果可选是否包含。`==` 等价于"黄色背景"（见 6.3.10），是背景色中唯一有专用语法的一种。

降级：字面文本。

#### 6.3.7 Callouts

```text
> [!note] 可选标题
> 正文
> [!warning]- 默认折叠
> [!tip]+ 默认展开
```

类型集合：note / tip / warning / danger / info / question / example / quote，未知类型按 note 渲染。折叠标记 `-`/`+` 用来承担 Notion "toggle" 的角色，因此**不另设 toggle 语法**。

降级：GitHub 把 NOTE/TIP/IMPORTANT/WARNING/CAUTION 渲染为 alert，其余显示为普通引用；纯文本可读。

#### 6.3.8 Math

```text
$E = mc^2$

$$
\int_0^1 f(x)\,dx
$$
```

- 渲染器 [RaTeX](https://github.com/erweixin/ratex)（MIT，KaTeX 的纯 Rust 移植，>99.5% KaTeX 语法覆盖，自带 KaTeX 字体约 548 KB）。
- M1 显示带高亮的源码；M2 起渲染位图；M4 改为直接消费 RaTeX 的 display list 用 GPUI 绘制，公式随主题变色、随字号缩放、行内公式按 baseline 对齐。
- **落地路径（2026-09-22 调研 `0.1.14`）**：RaTeX 在 crates.io 上按职责拆成多个 crate（`ratex` 本体是占位包，不要依赖它）。四步串起来：
  ```rust
  let ast = ratex_parser::parse(latex)?;                       // LaTeX → AST
  let lbox = ratex_layout::layout(&ast, &LayoutOptions::default()
      .with_style(style)          // 行间 $$ / 行内 $
      .with_color(color));        // 排版阶段就能给颜色
  let list = ratex_layout::to_display_list(&lbox);
  let png = ratex_render::render_to_png(&list, &RenderOptions {
      font_size, padding, background_color: 透明, device_pixel_ratio: 窗口倍率, font_dir: 空
  })?;
  ```
  颜色在 `layout` 阶段设定，所以公式直接按主题前景色排版，**不需要** 9.9 里"单色 SVG + alpha 蒙版着色"那条绕路；相应地缓存键要带上主题色。字体用 `ratex-render` 的 `embed-fonts` 特性内嵌 KaTeX TTF，运行时不依赖 `font_dir`，打进 `.app` 即可用。`ratex-svg`（→ resvg 光栅化）与 `ratex-pdf` 暂不需要，留给导出。
  渲染服务（9.9）的请求"源"因此要从"vault 内文件"泛化成"文件 | 公式源码"：图片按文件内容指纹入盘，公式按源码 + 字号 + 颜色指纹入盘。
- 渲染失败或语法不支持时显示源码 + 错误提示，内容不丢。
- 备选 `typst` + `mitex`（质量更高但二进制多 5–10 MB），仅在 RaTeX 停滞时启用。ReX 上游已停更，不考虑。

降级：GitHub 原生渲染 `$$` 数学；纯文本可读。

#### 6.3.9 图表、PDF 与链接卡片

这三类内容都不引入新语法，全部复用 L0/L1 已有形式，靠渲染层识别。

**mermaid**

````text
```mermaid
flowchart TD
  A[Start] --> B[Done]
```
````

- 渲染器 [merman](https://github.com/Latias94/merman)（MIT/Apache-2.0，headless 纯 Rust，对齐 mermaid@11.15，不启动浏览器或 JS 运行时）。使用其 `render_svg_resvg_safe_sync()` 管线（去掉 `<foreignObject>`），SVG 经 resvg 转位图。
- 接入 merman 的宿主文本测量回调，用 GPUI 文本系统测量标签宽度，保证中文标签不被裁切。图内 `$$` 数学标签 merman 默认调用 RaTeX，与 6.3.8 一致。
- 不支持的图类型或语法错误：按普通代码块显示源码 + 错误提示。
- 备选 `mermaid-rs-renderer`（更快，覆盖窄）。

降级：GitHub 原生渲染 mermaid 代码块；其他工具显示为代码块。

**PDF 嵌入**

```text
![[paper.pdf]]
![[paper.pdf#page=3]]
```

- 渲染器 [hayro](https://github.com/LaurenzV/hayro)（纯 Rust，`forbid(unsafe_code)`，回归集 1000+ PDF）。已知限制：不支持加密 PDF、部分混合模式，性能尚未优化，复杂页面可能秒级——因此一律后台渲染并缓存页面位图。
- 形态：嵌入块显示指定页（默认首页）的位图与页码翻页，双击用系统阅读器打开。不做 PDF 编辑、批注、正文搜索（正文进全文索引为 M4 之后的独立事项）。
- 排除 MuPDF（AGPL）与 poppler（GPL）；`pdfium-render` 需分发动态库，仅在 hayro 性能不可接受时作为回退。

降级：显示为字面 `![[paper.pdf]]`，文件本身仍在 `.assets/` 中。

**链接卡片**

```text
https://example.com/article        独占一行的裸 URL → 渲染为链接卡片
[标题](https://example.com)         行内链接 → 普通链接，悬停显示卡片
```

- 网页相关能力**只做链接卡片**：抓取目标页的 title / description / OpenGraph 图片 / favicon，原生渲染为卡片块。不做网页剪藏，不做内联或侧栏的实时网页视图（gpui-wry 为实验性、只支持 macOS/Windows、且永远覆盖在 GPUI 元素之上）。
- 抓取走 `reqwest` + `scraper`，运行在第 15 节的 Tokio 边界内；元数据按 URL 缓存在 `cache/`，默认 7 天过期；离线或失败时显示纯链接。
- 隐私：抓取会向目标站点发出请求，属于"内容离开本机"的一种，在设置中可整体关闭（默认开启，因为只发送 URL 本身而非笔记内容）。

降级：就是普通 URL / 链接，任何工具可读可点。

#### 6.3.10 属性列表与颜色

Markdown 没有颜色语法。借鉴 Pandoc 与 markdown-it-attrs 共有的**属性列表** `{.class key=value}` 约定，用它承载 Notion 式的文字颜色与块颜色，同时给将来其他块/行内属性留出位置。

```text
[文本]{.red}                      行内文字颜色
[文本]{.bg-blue}                  行内背景色
[文本]{.red .bg-yellow}           两者叠加
**粗体**{.blue}                   属性可紧跟任何行内元素（强调、代码、链接、==高亮==）
==文本==                          黄色背景的专用写法，等价于 [文本]{.bg-yellow}

这是一个段落。 {.bg-blue}          块颜色：属性列表置于块末，前有空格
这是一个段落。 {.red} ^para-id     与 ^id 共存时属性在前、id 在后
# 标题 {.red}                     标题（与 Pandoc header attributes 完全一致）
- 列表项 {.green}                 单个列表项

- 列表项
- 列表项

{.bg-gray}                        多行块：独占一行，与块之间空一行，规则同 ^id
```

- **调色板**（与 Notion 对齐，9 色）：`gray brown orange yellow green blue purple pink red`；文字色直接用类名，背景色加 `bg-` 前缀。具体色值由主题定义，浅色/深色各一套。
- 解析分工：标题、代码段、链接/图片、围栏代码后的 `{...}` 由 comrak 原生解析（ADR 0002）；`[文本]{...}` 与块末属性由 aero-core 后处理。
- **块颜色**作用于整个块及其子块（列表项含子列表、callout 含正文）。
- 未知的类名与 `key=value` 原样保留、不渲染、不丢失；`{}` 内容不进全文索引。
- 识别规则：行内属性列表必须紧跟行内元素或 `[文本]` 结束的 `]`，中间无空格；块属性列表必须位于块末且前有空格，或按 `^id` 的独占行规则。其他位置的 `{...}` 都是普通文本。`\{` 取消含义。
- **写回规则**：用户选"黄色背景"时写 `==文本==`（兼容性最好）；其他颜色写属性列表。选区恰好覆盖一个已有行内元素时，把 `{}` 挂在该元素之后而不是再包一层 `[ ]`。去掉全部颜色后删除 `{}`，若 `[ ]` 不再承载任何属性也一并去掉。
- 被否决的写法：内联 HTML `<span style="color:...">`（GitHub 会过滤 `style`，源码噪音大，且本方案对 HTML 只做透传不渲染）；自造 `=={red}文本==`（没有任何工具认识）。

降级：GitHub 与纯文本显示为字面 `{.red}`、`[文本]{.red}`，噪音水平与 `^id` 同级；Pandoc 与 markdown-it-attrs 系工具（VitePress 等）识别为 class，可通过 CSS 上色。

### 6.4 L2：Aero 扩展层

**L2 整体不进入 v1**（M4 之后再评估），本节只是预留语法空间，保证 v1 的解析器为这些形式留出 `Raw` 透传路径，避免将来引入时破坏往返。新增任何 L2 语法必须先回答：为什么 L1 不够、如何降级。

#### 6.4.1 容器 Directive

采用 CommonMark generic directives 提案的语法：

```text
:::name {key=value another="v"}
任意 Markdown 内容，可嵌套
:::
```

- 名称 `[a-z][a-z0-9-]*`，属性为 `{}` 内的 `key=value` 列表。
- 围栏至少 3 个冒号，闭合围栏的冒号数必须与开启围栏一致；嵌套时外层使用更多冒号（如 `::::columns` 内嵌 `:::column`），与代码围栏的规则相同，避免闭合歧义。
- 未知 name：内容按普通 Markdown 渲染，`:::` 行作为不可见的 `Raw` 边界保留。v1 中所有 directive 都按此处理。
- 首个候选容器：`::::columns` / `:::column`（多列布局），M4 后评估。

降级：`:::` 行显示为字面文本，内容顺序、可读性不受影响。

#### 6.4.2 行内 Directive

```text
:name[显示文本]{key=value}
```

预留，v1 不解析，按普通文本处理。

#### 6.4.3 数据块

以特定 info string 的代码围栏承载结构化内容，例如：

````text
```query
tag: #rust
sort: updated desc
```
````

- 数据块内部不是 Markdown，由对应处理器解释；v1 中一律作为普通代码块渲染。
- 首个候选 `query`（按标签/目录/属性列出笔记），M4 后评估。

降级：显示为代码块，内容可读。Obsidian Dataview / Bases 采用同样的做法。

### 6.5 转义与冲突

- `\[[`、`\#`、`\==`、`\%%`、`\:::`、`\{` 取消扩展含义。
- 代码块、行内代码、数学内不解析任何 L1/L2 语法。
- 段落末尾的 `^id` 只在紧贴行尾且前有空格时成立；`a^b` 不是 block id。
- 属性列表 `{...}` 只在 6.3.10 规定的位置成立，且内部必须全部是 `.class`、`#id` 或 `key=value` 形式的 token；`f(x) = {1, 2}` 之类留在原地不动。`[文本]{...}` 优先于 GFM 的链接引用（`[文本]` 无对应定义时 CommonMark 本来就按字面处理，因此不冲突）。
- Callout 标记 `[!type]` 只在引用块首行首个 token 位置成立。
- 与 GFM 冲突处：`[[x]]` 优先于链接引用定义 `[x]`；`==` 仅在成对且不跨行时成立。

### 6.6 降级总览

- **纯文本编辑器**：一切可读；扩展语法以字面形式出现，顺序不变。
- **GitHub**：L0 全部正确；callout 部分渲染为 alert；wikilink/embed/tag/`^id`/`{.color}`/`:::` 显示字面文本。
- **支持 wikilink 的 Markdown 编辑器（如 Obsidian）**：L1 大部分能原生渲染，这是附带收益而非承诺。

### 6.7 示例

````markdown
---
id: 01K5A8Z3QF7P9R2M4N6T8V0X1Y
title: ripgrep
created: 2026-09-14
tags:
  - rust
  - cli
---

# ripgrep

安装：

```bash
brew install ripgrep
```

升级：

```bash
brew upgrade ripgrep
```

^upgrade-command

> [!tip]- 常用参数
> `-t rust` 按类型过滤，`-g '!target'` 排除目录。

相关内容：[[Rust CLI]]，另见 #rust/cli

架构图（位于 `ripgrep.assets/architecture.svg`）：

![[architecture.svg]]

嵌入另外一篇文档：

![[Rust Search]]
````

---

## 7. 文档与 Block 模型

### 7.1 Block 的定义

Block 与 CommonMark 的块级节点一一对应，分两类：

- **Leaf Block（编辑单位）**：Paragraph、Heading、CodeFence（围栏；DataBlock 6.4.3 只是特定 info 的 CodeFence）、CodeBlock（缩进）、MathBlock、Table、ThematicBreak、HtmlBlock、FrontMatter、Raw、Marker（独占行的 `^id` / 属性列表，见 7.2）。
- **Container Block（结构单位）**：List、ListItem（每个列表项是一个 block，与 Notion 一致；List 本身只是容器）、BlockQuote / Callout、FootnoteDefinition、Directive。

编辑器的光标永远位于某个 Leaf Block 内；拖拽、多选、缩进操作作用于 Leaf 或 ListItem。

### 7.2 数据结构

```rust
struct Document {
    path: VaultPath,
    text: Rope,                    // 唯一文本真相
    front_matter: Option<Range<Anchor>>,
    blocks: BlockTree,             // 只持有 range，不复制文本
    version: u64,
}

struct Block {
    id: BlockId,                   // 会话内稳定的内部 id
    kind: BlockKind,               // callout 类型 / 折叠、directive 名、围栏 info、任务勾选等都在 kind 的参数里
    range: Range<Anchor>,          // 随编辑自动漂移的锚点，覆盖块的全部源文本
    suffix_len: u32,               // 尾部 ` {.red} ^id` 标记占的字节数，编辑器显示正文时去掉
    persisted_id: Option<Arc<str>>,// 文件中的 ^id（如有）
    attrs: Attrs,                  // 属性列表（颜色等）
    hash: u64,                     // 块源文本的 xxh3，供 7.3 身份匹配与索引增量
    children: Vec<Block>,
}
```

独占一行的 `^id` / `{属性列表}` 在树里是一个 `Marker` 块：区间照实覆盖那一行（往返、间隙校验都不用特判），语义（id、属性）已挂到前一个兄弟块上，UI 不渲染它。

Inline 树节点同样带 `attrs`，承载 6.3.10 的行内属性列表。

选择"一份 Rope + Block 树持有 range"而不是"每个 Block 一个 buffer"，原因：跨块选区、统一 undo 栈、按字节偏移写回都更直接。

**已定（ADR 0004）**：`Rope` = `ropey =2.0.0-beta.1`，与 gpui-base 内部同版本、同 feature（`metric_chars` / `metric_lines_lf` / `metric_utf16`），随 gpui-kit 一起升级。ropey 2.0 是字节索引 API，与 comrak 的源位置、gpui-base 的光标 / 选区偏移三方一致；10 MB 文档上编辑 0.4 µs、`clone()` 快照 0.02 µs、行 / char 换算 0.05–0.2 µs、物化整篇 0.7 ms、堆开销 11%。

`Anchor` 不是 rope 的功能，由 `aero-core` 自己实现：一张按位置排序的 marker 表，块区间 = `(start, Bias::Left)` + `(end, Bias::Right)`（块首、块尾的输入都归入该块）。每次编辑二分定位后线性平移，10 MB（19 万 marker）上 49 µs；每次重解析后整表重建。选区、光标、IME 标记区间同样用 marker 表达。spike 回放 1 万次随机编辑后 8.5 万个未触碰块的文本全部与原文一致。

### 7.3 Block 身份

- 内部 `BlockId` 在会话内稳定，供 UI 状态（折叠、选中）与索引增量更新使用。
- 重新解析后沿用旧 id 的匹配分四级（`aero-core::document`，随机编辑序列的属性测试保证：未被触碰、仍在原位的块 id 不变，且撤销后仍不变）：
  1. `(kind, 内容 hash)` 相同且起点重合——旧块的起点取已漂移的 anchor，与新区间同一坐标系；
  2. `(kind, 内容 hash)` 相同、起点最近——同内容的"孪生块"必须先做第 1 级，否则一个被改动后其余会在先序贪心里被错位认领；
  3. 同 kind、起点最近——内容被改过的块；
  4. 不限 kind、起点相差 ≤ 2 字节——段落 ↔ 标题 ↔ 列表项这类改前缀的类型转换保住身份，`/` 菜单切换块类型不丢焦点与折叠状态。
  其余分配新 id。
- 持久化身份只有 `^id` 与 front matter `id`，规则见 6.3.3 / 6.3.4。未被引用的块身份允许漂移。

### 7.4 编辑管线

当前 GUI 按键路径：

```text
键入 / 块操作
  → Rope 编辑、undo 记录、marker 平移
  → Document::reparse()：安全的普通段落局部解析，否则整篇解析
  → 更新块与可见控件，保持焦点和滚动锚点
  → 500 ms debounce → 后台校验与原子保存
  → 保存成功后排队更新全文 / 语义索引
```

核心层仍提供 `Snapshot::parse` / `apply_parse` 供后台解析使用，旧版本结果会被丢弃；但当前 `Editor::after_doc_change` 直接同步调用 `reparse`，尚未按文档大小自动转后台。ADR 中的 Rope / 解析微基准不能直接当作 GUI 输入延迟结论。

---

## 8. 解析与无损往返

### 8.1 解析策略：段落快速路径 + 整篇解析回退

2026-09-26 起，`Document` 对普通顶层段落内部、不会改变结构的编辑局部重解析；仅在块类型、区间、属性与持久 ID 校验一致时安装结果。边界、换行、结构标记、上下文敏感引用或解析警告等情况回退整篇解析，仍按 7.3 匹配 BlockId。引用语法及 comrak 的全局引用预算也会使快速路径失效。

这不是通用增量 Markdown 解析器。随机编辑、撤销 / 重做与引用预算回归对比局部路径和整篇结果；大文档的后台解析能力与 GUI 接线状态见 7.4。

### 8.2 解析器选型

要求：带准确源位置（block 与 inline 级）、支持 front matter / 表格 / 脚注 / 数学 / wikilink，可插入自定义 inline 语法。

**已定：comrak**（ADR 0002，2026-09-15）。M0 spike 用 CommonMark spec 655 用例 + 45 篇真实笔记 + 合成 1 MiB 文档对比了 comrak 0.55 与 markdown-rs 1.0：

- 速度：comrak 快 10–60 倍；markdown-rs 在 1 MiB 上超线性（1.13 s），无法支撑整篇重解析。
- 覆盖：comrak 原生支持 wikilink、`==高亮==`、`[!note]` callout、`:::` 块级容器（按冒号数嵌套）、标题/代码段/链接的 `{.class}` 属性、CJK 友好强调；markdown-rs 均无。
- 位置：两者节点 100% 带位置。comrak 有两个具体缺陷——缩进的表格行内单元格位置左移、链接引用定义之后的同段文本错位；另外 `FootnoteDefinition` 被移到 AST 末尾、链接引用定义无节点、`$$` 块公式是段落内的 display Math。全部在后处理层补偿。

```text
原文
 → comrak（front matter / strikethrough / table / tasklist / footnotes / math / wikilinks /
           highlight / block_directive / attributes / cjk_friendly_emphasis；alerts 关闭）
 → 转换：sourcepos → 字节区间；越出父区间 / 非 char 边界的夹回并计入报告
 → 后处理（aero-core::parse）：
      兄弟块按区间排序（FootnoteDefinition 归位）→ 兄弟重叠时截断前者（块起点比终点可靠）
      → 仅含 display Math 的段落 → MathBlock
      → 段尾 / 独占行的 ^id 与 {属性列表}（独占行成为 Marker，语义挂到前一个兄弟）
      → 引用块首行 [!type]± 标题 → Callout（Aero 的 8 种类型与 GitHub 的 5 种走同一条路）
      → 顶层块之间含非空白内容的间隙 → Raw（链接引用定义等 comrak 不产节点的内容）
 → Block 树（带字节区间）；Inline 树（embed / #tag / %% / [文本]{属性} / 行内 directive）为 M1a 事项
```

后处理带一份 `ParseReport`（夹回 / 丢弃 / 补 Raw 的计数与明细）。CommonMark spec 655 例、样本文件与合成文档上要求块级零夹回；真实笔记里目前只见过"列表项内段落末尾多算一个空格"这一种 1 字节夹回。

### 8.3 无损往返规则

1. 未修改的 Block 写回时逐字节等于原文（包括缩进、空行数、转义方式、软换行）。
2. 修改一个 Block 只重新序列化该 Block 的区间；块的分裂/合并只影响相邻区间。
3. `Raw`/`HtmlBlock` 永远逐字节透传。
4. 属性测试：对语料中每个文件，`serialize(parse(x)) == x`；对随机编辑序列，未触及区间保持不变。
5. 解析器版本升级必须通过全部往返测试后才能合入。

### 8.4 序列化风格只作用于新内容

`config.toml` 里的风格配置（bullet 用 `-`、强调用 `**`/`*`、缩进 2 空格、表格是否对齐等）只影响编辑器新生成的文本，绝不用于重排已有内容。

---

## 9. Block Editor

### 9.1 形态：默认所见即所得，源码按需打开

**已定（2026-09-22）**。普通用户面对的是块和格式，不是 Markdown 语法；极客仍能改源码。磁盘上只有 Markdown。这是一个编辑器的两种呈现，不是两个产品。交互细则仍以 `docs/ui_design.md` 为准，与本节冲突时以本节为准。

**浏览与编辑一致（硬性要求，2026-09-23）**。宗旨：用户不感知 Markdown 语法。以下每条都是验收标准，新的块类型、新的行内格式上线前逐条过一遍：

1. **视觉一致**：编辑态与浏览态是同一副样子。不许浏览时美观、编辑时冒出不相干的元素，也不许露出任何一截语法（`**`、`[[`、`]]`、`](地址)`、`}`、`{.red}`、`# `、`> `、代码围栏等）。唯一例外是用 ⌘K 改链接时，显示正在改的地址。
2. **点击零跳变**：点进任何一块，所有块的位置、高度、折行都不变；字号、字重也不变，不许浏览时字体正常、点进去变粗或变细。只多出一个光标。
3. **表格**：列宽可以拖动调整，并写回文件；点某一格、在格里打字都不改变列宽；格内长文字自动换行。
4. **选字**：在任何一块上按下鼠标就能拖动选字，不要求先点进那一块、或先把光标放到文字后面。
5. **callout**：`> [!tip]`、`> [!note]` 等在浏览与编辑时是同一个外框、图标和标题位置，不跳变。
6. **任务**：方框可以直接点击勾选 / 取消，不进编辑态，不引起任何跳变。

某类内容暂时做不到这几条时，宁可保持「预览 + 点进去编辑源码」并在下文列出，也不能半藏半露。回归由 `editor::layout_tests` 守住（逐个点进每一段、所有块外框不变；聚焦后语法字节不占宽度；跨块直接拖选；方框就地勾选等），真实窗口里的前后截图逐像素比对用 `.agents/skills/aero-ui-test` 的 `aero-ui probe` 完成（第 20 节，[浏览与编辑一致验收](qa/m2-seamless-edit.md)）。

默认观感已经按本节落地（2026-09-23 修订）：段落、标题、列表项与任务、只含段落的引用、callout 的标题与正文、表格的每个单元格、围栏代码块里的代码，浏览与编辑由**同一个文本控件**画——没聚焦时只是没有光标。点进去不换控件，字的位置、字重、折行都不变，按下鼠标就能拖选。成对标记、行首 `# ` / `> `、`{.red}`、`\` 转义、代码围栏始终藏着，光标贴着也不露出来；唯一的例外是 ⌘K 改链接时，光标落进 `(地址)` 那段地址才显示。块菜单「编辑源码」只打开当前块；视图菜单「查看源码」打开整篇；「聚焦时显示源码」记在 `state.toml`，打开后就是原来的 Live Preview。

仍按预览 + 源码编辑的：数学块、带行内公式的段落（公式留在预览里，只编辑两侧文字）、独占一段的图片 / 笔记嵌入、正文含列表或代码等结构的引用与 callout、含 `<br>` 的段落与表格。

**默认路径**

- 未聚焦块：完全渲染（标题、加粗、链接、图片、callout）。已实现。
- 聚焦块：只显示样式后的文字。成对标记（`**`、`*`、`~~`、`==`、行内代码、`[[ ]]`、行内 `{.class}`）配对完成后始终隐藏，光标贴着也不露；只有正在输入、还没配对上的那个符号看得见。`# `、`- `、`> `、`1. ` 打完立刻转成对应块，不停留在标记上。
- 选出一段文字：浮动工具条只放源码能诚实写回去的格式——粗体、斜体、删除线、行内代码、链接、颜色、转换块类型。不放字体族、字号、字间距、段距：这些要么写不进可降级的 Markdown，要么要发明私有样式。段落相关走「转换块类型」。工具条是发现路径，每一项都有等价快捷键（`ui_design.md` 5.9 / 7.4）。设置里的正文字体与字号是阅读偏好，不写进文件，也不出现在选区上。
- 悬停块：拖拽手柄重排，`+` 在下方插入。拖拽、转换类型、缩进都是对源码的编辑，进同一条 undo 栈（9.5）。

**源码怎么够到**

不做应用级「纯 Markdown 模式」。那是两套控件、两套选区，往返一定弄脏格式。极客要的是源码够得到，不是整页变成文本框。

| 层 | 行为 |
| --- | --- |
| 默认 | 聚焦块也是所见即所得，标记藏起来 |
| 按块 | 块菜单「编辑源码」只打开当前块。数学本来就编辑源码 |
| 按篇 | 「查看源码」整篇或分栏，给需要改写整篇、或从别的编辑器迁过来的人 |
| 偏好 | 「聚焦时显示源码」就是现在的 Live Preview。同一编辑器上的开关，不关掉块模型、拖拽、`/` 和块选中 |

复制、剪切、保存、用其他编辑器打开，始终是 Markdown。块选中态的 `⌘C` / `⌘X` 已是源码。

**藏不住就不要假装藏住**

- 数学的正文就是源码，在弹窗里改。代码块只编辑围栏里的代码，不解析 Markdown。
- 表格按单元格编辑：点哪格改哪格，列宽不因为点击或打字而变，长文字在格内折行；输入的 `|` 自动写成 `\|`，⏎ 到下一行、Tab 到下一格。拖动列边界调列宽，比例写回分隔行里 `-` 的个数（各列一样多时按内容估算列宽），别的工具里它仍是一张普通表格。增删行列还没做。
- 颜色在文件里是 `{.red}` / `{.bg-blue}`（6.3.10）。界面里不能让用户看见这串标记；在 GitHub 或纯文本里露出字面属性，是已经接受的降级噪音。
- 查找可以匹配源码（因此搜得到 `[[`），高亮落在渲染后的文字上，不为了命中把标记翻出来。
- 渲染失败的块显示源码加一行错误。内容不丢，也不写回文件。

**模型不变**

```text
磁盘 Markdown  ←→  Rope + 块树  ←→  块 UI
     真相              编辑 / 索引         观感
```

格式条、拖拽、转标题都改源码前缀或行内标记。往返无损仍是结构保证，不是事后证明。Phase 1 借用 gpui-base 文本引擎，先得到正确的 IME、选区、换行；标记隐藏是同一模型上的呈现层。若需要更细的控件，按 9.3 换掉聚焦块编辑器，不动 `Document`。

**排期**

标记隐藏、按块源码、整篇源码视图，以及「聚焦时显示源码」偏好已经落地。字符级跨块选区、表格增删行列、协作光标仍留在后面（9.8）。

### 9.2 渲染结构

- 当前用 `editor/viewport.rs` 按视口管理块行，缓存高度并保留滚动锚点；大列表可按顶层列表项拆分，带属性的容器和嵌套子树保持整体渲染。可见区前后保留预渲染余量。
- 文字类块浏览与编辑共用文本控件，容器绘制缩进、引用竖线与 callout 外框；控件按可见性维护，聚焦块受保护。
- 图片、公式、Mermaid、PDF 经 9.9 的后台渲染服务；笔记嵌入由单独的来源读取缓存交给编辑器预览。链接卡片仍未实现。
- 长文滚动、点击和关闭保存的验证见 [2026-09-26 性能记录](qa/performance-followup-2026-09-26.md)，不据此宣称帧率或输入 p99 已达标。

### 9.3 聚焦块编辑器

当前文字类块使用 gpui-base 文本控件，浏览与编辑复用同一套控件和标记隐藏规则（9.1）。段落、标题、列表项、表格单元格与简单 callout 按各自编辑单元呈现；源码入口按需显示标记。

控件的输入写回 `Document`，文档持有统一撤销历史，再由 7.4 的管线更新结构和保存。核心与控件使用相同版本的 ropey，但不是直接共享一个可变 Rope 实例；控件文本、文档范围与光标由应用层同步。当前视口负责保留可见控件和聚焦状态，不再在每次浏览 / 编辑切换时换成另一套渲染布局。

### 9.4 选区与键盘模型

- 块内：字符级选区。
- 跨块：升级为**块级选区**（选中整块），与 Notion 行为一致。字符级跨块选区列为 Phase 2。
- `Enter` 在段落末尾新建段落；`Shift+Enter` 软换行；空列表项上 `Enter` 退出列表；`Tab`/`Shift+Tab` 调整列表层级；`Backspace` 在块首合并到上一块。
- 上下箭头跨块移动时保持视觉列位置。
- 完整的交互状态机、快捷键总表与界面布局见 `docs/ui_design.md`。

### 9.5 Undo / Redo

单一 undo 栈，记录 Rope 编辑操作。所有块操作（拖拽、转换类型、缩进）都表达为文本编辑，因此天然可撤销。相邻 300 ms 内的字符输入合并为一组；IME 组合期间不产生 undo 记录。

### 9.6 IME

- 中文、日文输入在 macOS 上是 M1 的验收项，包括：组合中下划线、候选框位置跟随光标、组合期间按 Esc/方向键、组合文本跨语法边界（例如在 `**` 内输入）。
- 在 Linux（fcitx5 / ibus）与 Windows 上分别在 M4 验收。

### 9.7 块操作

- `/` 菜单：切换块类型、插入 callout / 代码块 / 表格 / 分割线 / 嵌入 / 日期。
- 拖拽手柄：块级拖动重排，跨列表层级时调整缩进。
- 类型转换：段落 ↔ 标题 ↔ 列表 ↔ 引用 ↔ callout，均为对源文本前缀的编辑。
- 块引用：`[[note#^` 触发块选择器，自动写入 `^id`。

### 9.8 明确不做

- 应用级「纯 Markdown 模式」（第二种编辑器）。源码入口见 9.1。
- 选区上的字体族、字号、字间距、段距。
- 表格的增删行列、合并单元格（单元格编辑与列宽已做，见 9.1）。
- 字符级跨块选区、协作光标。

语法标记隐藏不再列入不做，已按 9.1 落地。

### 9.9 重内容渲染服务

当前图片、数学、Mermaid 与 PDF 共用 `crates/aero-app/src/render.rs`；链接卡片尚未接入。

```text
请求（源、宽度 / 字号、HiDPI 倍率，公式 / 图表含主题信息）
  → 内存缓存命中：返回位图
  → 后台检查内容指纹与磁盘缓存 cache/<vault-id>/<kind>/
  → 未命中：后台解码 / 渲染 → 原子写缓存 → 通知界面
```

- 图片等比缩小；数学由 RaTeX 按主题色直接光栅化；Mermaid 经 merman 生成 SVG，再由 resvg 光栅化；PDF 经 hayro 渲染。
- 首帧显示占位，失败保留源码和错误提示。坏磁盘缓存按未命中重建，不修改笔记内容。
- 渲染并发 2，待派发队列最多 32；非 PDF 内存缓存 64 项、PDF 12 页，共享约 128 MiB 位图软预算。当前帧可见资源受保护，可能暂时超预算；该预算不是应用总内存上限。
- 图片宽度按 64 逻辑像素分档；任务代次丢弃过期尺寸和回调。已启动渲染的全面协作式取消仍不是已交付保证。
- 磁盘缓存尚无容量淘汰。笔记嵌入另走来源读取缓存，再由同一套预览显示；它不是位图缓存。

后台文件操作、缓存与真实窗口回归见 [性能续作记录](qa/performance-followup-2026-09-26.md)。

---

## 10. 索引管线

### 10.1 总览

```text
文件事件 / 编辑保存
  → 解析（复用 aero-core）
  → 元数据 & 链接 & 标签 → SQLite（同事务）
  → 全文 → FullTextIndex（批量提交；引擎见 10.3）
  → 向量 → 后台低优先级队列 → VectorIndex（Phase 3，默认 zvec）
```

- 索引粒度到 Block：搜索结果可以直接定位到块。
- 每个文件在 SQLite 记录 `(mtime, size, hash)`，启动时只重索引发生变化的文件。
- 任何索引可以删除后全量重建；重建在后台进行，UI 显示进度。

### 10.2 元数据库（SQLite）

```sql
notes  (note_id, path, title, front_id, mtime, size, hash, created, updated, fm_json)
blocks (note_id, block_ord, kind, byte_start, byte_end, hash, persisted_id)
links  (src_note, src_block_ord, raw_target, kind, dst_note, dst_anchor, resolved)
tags   (note_id, block_ord, tag)
assets (path, referenced_by_count)
index_state (name, version, last_built)
```

`links.resolved = 0` 的记录即"未解析链接"，用于反链面板和新建笔记建议。

### 10.3 全文检索

无论引擎为何，索引文档 = Block，字段：`note_id`、`block_ord`、`title`、`body`、`tags`、`path`。`aero-core` 只依赖 `FullTextIndex` trait，引擎实现在 `aero-index` 内可替换。

**引擎：zvec FTS（已定，ADR 0003）**。随 zvec-rust 一并获得（见 10.4）：原生 BM25、短语、布尔 `+term -term "phrase"`，tokenizer 用 `jieba`（`cut_mode=search`）+ `lowercase` filter。全文与向量共用 zvec 引擎但使用独立 Collection；当前混合结果由 `aero-index::semantic::fuse_results` 做 RRF 融合（10.4）。无 snippet API，高亮在客户端基于 block 源文本自行完成。

M0 实测（`bench-10k`，120k block，Apple M5 Pro）：查询 p95 ≤ 3 ms；保存一篇（≈12 block）后 2 ms 内可搜到，不需要 `flush()`；冷打开 100 ms。Tantivy 与 SQLite FTS5 不再评估，`FullTextIndex` trait 只作为架构隔离保留。

引擎使用约束（全部来自 spike 实测）：

- **保存路径 = `upsert(新 block)` + `delete(旧有而新无的 pk)`**。旧 pk 列表来自 SQLite 的 block 表。**禁用 `delete_by_filter`**：它会留下僵尸主键——doc 查不到了，但同一 pk `insert` 报 `AlreadyExists`、`delete` 报 `NotFound`，`flush` / `optimize` 都救不回，只有 `upsert` 能覆盖。
- **正在编辑的文档**：当前在保存成功 / 外部重载后排队更新全文索引，没有未保存正文的内存 overlay，也没有按键后 300 ms 直接 upsert 的通道。500 ms 自动保存与有序写队列保证保存后进入索引。
- 刷盘与整理：保存后不 flush；空闲 30 s 无编辑时 `flush()`；后台全量重建**每 1 万 block `flush()` 一次**（否则峰值内存冲到 900 MB，分批后 ≈ 280 MB）；碎片段会让 p95 从 3 ms 涨到 9 ms，每日或段数超阈值时 `optimize()`（10k 上 0.5 s）。
- **jieba 词典裁剪（已实现）**：全词典（349k 词）常驻 ≈150 MB，是索引侧最大的内存项。`scripts/trim-jieba-dict.sh` 按词频截到 ~100k 词（磁盘 1.4 MB），长尾交给 HMM 与 search 模式的子词切分。实测常驻并非线性下降——cppjieba 的 `DictTrie` 节点开销高，100k 词裁剪后仍 ≈110 MB（省 ≈60 MB；裁到 50k 省 ≈85 MB），原先"裁后 45 MB / 省 150 MB"的估计过于乐观。领域词：zvec 的 `JiebaTokenizer` 只加载 `jieba.dict.utf8` + `hmm_model.utf8`，**不读**单独的 `user.dict.utf8`，故 `assets/jieba/user.dict.utf8` 里的领域词由脚本直接并进主词典。裁剪词典由 `fetch-zvec.sh` 产出到 `vendor/zvec/lib/data/jieba_dict_trimmed`（不入库，随 fetch 重生）；运行时 `aero-index` 显式 `set_default_jieba_dict_dir` 覆盖 zvec 自动发现的全词典，打包时装进 `Resources/jieba_dict`。
- `ngram` tokenizer 不作备选：构建慢 2 倍、磁盘 2.5 倍，英文按字母二元组匹配 precision 太差。

### 10.4 本地语义索引（已接入）

- `aero-embedding` 直接使用 `ort` + `tokenizers`，固定 multilingual-e5-small int8 模型，384 维、CPU 单线程、每批最多 4 段、每段最多 256 tokens。当前没有 fastembed、模型档位或远程 Embedding。
- 模型下载与语义搜索是两个默认关闭的开关；用户先允许下载，再显式点击下载。约 135 MB 的固定文件经过大小与 SHA-256 校验后发布；笔记与查询不上传。
- `aero-index::semantic` 用独立 zvec HNSW / cosine Collection 与 SQLite 元数据，放在 `index/<vault-id>/semantic/`，不占用全文 Collection 的锁。模型 ID 或缓存格式变化可重建语义缓存。
- 按可索引正文块提取纯文本，长块切成带有限重叠的片段并保留 `block_ord`；不是把相邻短块合并。每段加限长标题前缀，超过 2 MiB 的笔记等超预算输入不进入语义索引。
- 全文与语义查询独立返回；先显示全文，再由 `fuse_results` 以 RRF（k=60）按路径和块去重融合，稳定同分时优先全文。当前未使用引擎 `MultiQuery`。模型不可用时全文搜索继续工作。
- 专用后台线程在输入空闲 800 ms 后分批索引，可手动暂停，查询优先于索引；模型空闲 60 秒后卸载。保存、删除、外部变化与忽略规则变化触发增量同步，版本校验避免发布过期片段。

开启方法、资源边界和真实模型测试见 [本地语义搜索](semantic-search.md)。zvec API 同步，均在后台使用；全文的词典和分批写入约束仍见 10.3。

### 10.5 一致性

- SQLite 是索引状态的权威；全文/向量索引若与 SQLite 的 `hash` 不一致，以文件为准重建对应块。
- 索引格式带版本号，升级时按需重建。

### 10.6 与第三方索引工具的关系

vault 就是 Markdown 文件，用户可以同时用 [zvec-grep](https://github.com/zvec-ai/zvec-grep)（`zg`，Node.js CLI + MCP，同样基于 zvec）等第三方工具对 vault 建索引并接给自己的 agent，这是"AI = Another Client"的自然结果。Aero 不复用 zg 作为组件（Node 运行时、索引落在项目根目录都与本方案冲突），但借鉴其检索管线设计：prose 按 section/chunk 分块并保留源位置、"语义发现 → BM25 锚定 → 精确文本验证"三段式、MCP 工具签名（`query` + `fts[]` + `fuse`）。规划中的 Aero MCP 将提供：Block 级结构、链接/反链/标签、带审阅的写入。

---

## 11. 链接、反链与图谱

- 链接解析规则见 6.3.1，由 `aero-core::links` 统一实现，GUI/CLI/MCP 共用。
- 反链面板：按 `links.dst_note` 查询，显示来源块的渲染片段。
- 未解析链接：聚合展示，一键创建目标笔记（放入默认目录或链接所在目录，可配）。
- 重命名/移动：在同一事务内更新所有 `links.raw_target` 对应的文本（只改链接字符串，不触碰其他字节），并写回相关文件。
- 图谱：`links` 表的可视化，M4 提供局部图（当前笔记 2 跳），全局图列入更远的路线。

---

## 12. 文件监听与外部修改

- `notify` 监听 vault；初次扫描使用 `ignore` crate（支持 `.gitignore` 与 `.aero/ignore`）。
- 事件 debounce 200 ms；忽略 `.aero/`、附件目录以外的非 `.md` 文件，以及自身刚写入的路径（按 `(path, hash)` 在 2 s 窗口内过滤）。
- macOS FSEvents 会合并事件、其他编辑器的原子保存会产生 `Remove + Create`，统一按"路径最终状态"处理，不依赖事件类型。
- 磁盘文件变化且当前文档**无未保存修改**：静默重载，光标按块 hash 匹配复位。
- 磁盘文件变化且**有未保存修改**：当前保留双方版本，以 `<name>.conflict-<time>.md` 保存磁盘版本并提示选择。基于上次读取内容的三方自动合并仍是后续目标，尚未接入。
- 文件被外部删除：文档转为"已删除"状态，保留内容，用户可另存。

---

## 13. AI 接入：CLI 与 MCP

### 13.1 CLI

```text
aero search "<query>" [--tag] [--limit]        全文检索，输出块级结果
aero read <note|id> [--block ^id]
aero backlinks <note>
aero append <note> --text "..."
aero new <path> [--template]
aero index rebuild | status
aero export <note> --format md|html|json
```

上述七个子命令已在 `aero-app/src/cli.rs` 实现，输出支持 `--json`；`search` 当前只查全文。按现存路径读、导出、追加和新建可以绕开被 GUI 占用的索引；搜索、反链、状态在锁冲突时读取临时全文副本，重建仍需独占。参数和新鲜度限制见 [CLI 记录](qa/m2-cli.md)。MCP 尚未实现。

### 13.2 MCP Server（规划，尚未实现）

- 实现基于 `rmcp`，传输先支持 stdio（Claude Desktop / Cursor 等本地客户端），Streamable HTTP 列入 Phase 3。
- 工具：`search_notes`、`semantic_search`（Phase 3）、`read_note`、`read_block`、`list_notes`、`get_backlinks`、`get_tags`、`create_note`、`append_to_note`、`replace_block`。
- 资源：`aero://note/<id>`、`aero://block/<note>/<id>`。
- **写权限模型**：默认"审阅模式"，AI 的写入以待确认变更出现在应用内（diff 视图），用户确认后落盘；可对单个客户端授予直接写入权限。写入同样只重写目标块。
- 隐私：MCP Server 只监听本机；应用设置页明确列出所有会把内容发往本机以外的功能（远程 Embedding、远程 LLM），默认全部关闭。
- **v1 不做应用内 AI**（对话、摘要、改写等）。用户在自己选择的 AI 客户端（Claude Desktop、Cursor、Codex 等）里通过 MCP 使用知识库，Aero 只负责把知识库暴露得足够好。若日后加入应用内 AI，须复用同一套 `aero-core` API，并通过 OpenAI 兼容接口优先对接本地模型（Ollama / llama.cpp）。

---

## 14. 同步

- **v1 不实现同步引擎，做到 sync-agnostic**：vault 交给 iCloud / Dropbox / Syncthing / git 等工具，应用负责第 12 节的外部修改处理，这是 local-first 与"Markdown = Source of Truth"下最一致的方案。
- 内建 git 支持（自动提交、历史查看）作为 M4 候选，它与文件真相天然兼容。
- CRDT（Loro / Automerge）方案的前提是并发编辑期间以 CRDT 状态为真相、Markdown 为投影，这与第一条原则冲突。只有在明确需要实时协作时才重新评估，并且要求 CRDT 层完全可从 Markdown 重建。

---

## 15. 并发与异步模型

下列为架构约束与目标；当前 GUI 同步解析的边界见 7.4，文件操作后台化和切库保存屏障见 [2026-09-26 记录](qa/performance-followup-2026-09-26.md)。语义服务使用独立线程，模型下载用同步 `ureq`；MCP 与远程 Embedding 尚未实现。

- 主执行器是 GPUI 的 `ForegroundExecutor` / `BackgroundExecutor`；`aero-core` 的异步接口不绑定运行时（返回 `Future`，内部用 `async-channel` / `futures` 原语）。
- Tokio 只存在于 `aero-mcp`（`rmcp` 依赖）与远程 HTTP 调用（链接卡片抓取、可选的远程 Embedding）中，运行在独立线程的 runtime 上，与 GPUI 侧通过 channel 通信。GUI 代码不得直接依赖 Tokio。
- 后台任务分三级优先级：用户可感知（解析、保存、FTS 增量）> 结构性（反链、缩略图）> 可延后（向量索引、全量重建）。低优先级任务在用户输入期间让出。
- 所有磁盘 IO 在后台线程；主线程只做布局与绘制。

---

## 16. 平台与分发

- **macOS 首发，最低 macOS 15**（与 gpui-component 的要求一致，只跟最新两代）；Linux（Wayland/X11）与 Windows 列入 M4，单人投入下视 M1–M3 的实际进度决定是否推进。
- **无移动端**：GPUI 不支持 iOS/Android。策略是坦白告知：vault 就是 Markdown 文件，配合任意手机 Markdown 编辑器与同步工具即可读写。产品文档中明确写出。
- 当前分发：macOS `.app` / `.dmg` 打包、签名与公证脚本已有；正式签名公证的验收仍未关闭。自动更新、Linux AppImage/Flatpak 与 Windows MSIX 为后续目标；当前「检查更新」没有配置更新源。
- **零远程遥测**：不自动上传使用数据或崩溃信息。仅在本机保存有上限的诊断记录，不含 vault 正文、标题、查询、剪贴板或真实路径；由用户检查后决定是否手动提交，详见 16.1。
- **开源**：仓库已采用 Apache-2.0，M1 可用后公开；此前保持私有以避免在编辑器骨架不稳时消耗精力应对外部反馈。
- **原生动态库**：zvec-rust 依赖 `libzvec_c_api.dylib / .so / .dll`（macOS 版 23 MB，只依赖系统库，minos 14.0）。链路已在 M0 跑通（ADR 0003，`spikes/zvec-index/bundle.sh`）：
  - 构建不联网：`scripts/fetch-zvec.sh` 下载预编译包并比对 `scripts/zvec-prebuilt.sha256`，解到 `vendor/zvec/lib`（gitignore）；`.cargo/config.toml` 设 `ZVEC_LIB_DIR` + `ZVEC_AUTO_BUILD=0`，zvec-rust 关掉 `bundled` feature。
  - **rpath 由 `aero-app` 自己的 build.rs 声明**（zvec-sys 的 `rustc-link-arg` 不会传给下游二进制）：开发期加 `ZVEC_LIB_DIR` 绝对路径，另加 `@executable_path/../Frameworks`；打包时 `install_name_tool -delete_rpath` 删掉绝对路径。
  - macOS：dylib 进 `Contents/Frameworks`，jieba 词典进 `Contents/Resources/jieba_dict`，先签 dylib 再签 app（Developer ID 加 `--options runtime`），再公证；Linux 打入 AppImage/Flatpak（rpath `$ORIGIN/../lib`）；Windows 与可执行文件同目录。
  - 预编译包只有动态库，上游 CMake 也只提供 shared 目标，**静态链接放弃**。
  - 预编译库的 glibc 基线为 2.28（Ubuntu 20.04 / CentOS 8），作为 Linux 最低支持版本。
  - 依赖形态：crates.io 的 0.7.0 不带词典分发与词典 API，暂锁 git tag `v0.7.1`；git 依赖会递归拉上游 C++ 树的全部 submodule（~730 MB、~10 分钟），上游发 0.7.1 到 crates.io 后立刻切回版本号，此前 CI 缓存 `~/.cargo/git`。

---

### 16.1 本地日志与诊断实施方案（2026-10-10）

**目标**：在用户机器上还原故障前的操作顺序、后台任务时序和性能阶段，同时保持本地优先、低开销和内容隐私。现有 `log` → stderr 默认 warn 的机制升级为结构化本地记录；不接自动上报服务。

**基础设施**：

- Rust `tracing` 事件与 span，兼容已有 `log` 调用。统一 schema，记录 UTC 时间、单调时钟耗时、会话 ID、操作 / 父操作 ID、模块、线程、事件名及明确允许的数值 / 状态字段。后台任务显式携带关联上下文，不依赖跨 await 的 thread-local span guard。
- macOS 默认 `~/Library/Logs/Aero/`；debug 的隔离数据目录同时隔离日志。每进程 / 会话独立文件，避免 CLI 与 GUI、多实例混写。
- 后台写入、有界队列、定期刷新，轮转并按 7 天 / 总量 50 MiB 清理；保留当前活跃会话。队列满不阻塞 UI，低优先级记录可丢弃，累计丢弃数必须可见。日志目录不可写、磁盘满等故障不得使编辑器崩溃。
- 普通模式常开：生命周期、业务操作、错误、慢任务；详细模式仅增加安全的阶段事件，开启后 15 分钟自动恢复，退出不继承。保留 `AERO_LOG` 终端调试入口。

**操作与性能**：

- 在共同业务入口覆盖开 / 关库、开 / 切 / 关笔记、创建 / 导入 / 改名 / 删除、保存、搜索 / 补全、编辑结构命令、撤销重做和退出。记录接受、排队、执行、完成 / 失败 / 取消 / 过期结果丢弃。
- 第一批重点接通打开笔记、保存、局部 / 全量解析、索引、重内容渲染与退出链路。记录文档版本、任务代次、匿名文档编号、输入 / 候选 / 队列规模；计时区分排队、后台执行及主线程装回。
- 连续输入、滚动、焦点等高频行为限频或聚合，不记逐键文本。主线程心跳记录明显停顿，排除休眠 / 非活跃窗口等情况；日志不能替代 CPU / GPU 采样分析。

**隐私边界**：

- 所有持久化字段采用白名单；正文、标题、文件名 / 绝对或相对路径、搜索词、标签文本、剪贴板、公式 / Mermaid 源码、任意对象 Debug 均不落盘。文档 / 附件使用会话内匿名编号，不使用可跨会话推断文件名的裸哈希。
- 旧 `log` 记录仅保留级别、来源位置等安全元数据；原始错误和第三方输出只保留在主动开启的终端调试中，新的业务事件使用稳定错误类别 / 状态。修正现有公式、Mermaid 和路径日志。
- panic payload 和系统崩溃报告可能含用户信息：默认 panic 报告保存位置、栈和 payload 类型，不直接保存任意 payload；系统报告单独由用户选择，不隐式打包用户状态、索引或 vault 文件。

**异常结束与导出**：

- 每会话持久化开始 / 正常结束信息；下次启动识别已结束进程遗留的未完成会话，标为“未正常结束”，不把强制结束 / 断电误称为已确认崩溃。
- 全局 Rust panic hook 尽力通过独立紧急通道保存线程、位置、调用栈与操作上下文；panic 不等于进程终止，局部 catch_unwind 可恢复。异常退出时末尾尚未写出的记录可能丢失，不承诺零丢失。
- 原生 SIGSEGV / SIGABRT 依赖 macOS `.ips` 报告；发布构建记录 commit、版本、平台、二进制 UUID，归档匹配的 dSYM 和动态库身份。不得以 catch_unwind 宣称捕获原生崩溃。
- 设置提供详细诊断开关、打开日志目录和导出诊断包；CLI 也提供导出入口用于 GUI 无法启动时。诊断包只选取受管理的会话日志 / panic 报告与构建清单，使用 create-only 发布，用户手动提交。

**验收**：所有测试直接在本机 macOS 执行，不在沙箱中运行。自动覆盖字段隐私、并发关联、轮转 / 容量、队列丢弃、写盘失败、panic / 正常退出 / 异常结束及导出范围；真实 Aero 窗口使用隔离 vault 与数据目录验证日志、详细开关和导出。记录可重复的写入开销测量，不以微基准宣称 GUI 性能预算已达标。

**状态**：已按本节方案实现本地结构化记录、操作关联、性能汇总、panic 报告、详细模式、设置 / CLI 导出与 release 符号归档。所有验证直接在本机 macOS 执行：正式模块 936 项、本地 GPUI 补丁 924 项、打包检查 12 项通过；最终保存 → 索引通知关联补充后，应用 603 项与链接集成 17 项再次通过。真实窗口验证内容隐私、导出、正常退出及重启恢复；release 运行时 / 应用 / dSYM UUID 匹配。范围、开销和限制见 [诊断验收记录](qa/local-diagnostics-2026-10-10.md)。更细的帧级采样和人工提供系统崩溃报告仍为后续诊断能力，不扩大为自动远程遥测。

## 17. 非目标（v1）

- 实时多人协作
- 移动端
- 应用内 AI 对话 / 摘要 / 改写（AI 一律经由 MCP 与 CLI）
- 与 Obsidian 或任何其他工具的兼容性保证
- L2 扩展层（directives、数据块）
- Notion 式数据库 / 关系型属性视图
- 插件系统（MCP + CLI 是 v1 的扩展面；WASM 插件列入长期路线）
- 云账号、托管同步
- 表格增删行列与合并单元格（单元格编辑、列宽拖动已实现）
- 应用级「纯 Markdown 模式」，以及写进文档的字体族 / 字号 / 字间距 / 段距（9.1）
- 网页能力只做链接卡片：不做网页剪藏、不做内联或侧栏的实时网页视图（WebView）
- PDF 编辑、批注、正文全文索引（只做页面预览）
- 除 `.md` 以外的文档格式（PDF、Office 等只作为附件）
- 遥测与自动崩溃上报

---

## 18. 里程碑与排期

每个里程碑交付可运行的软件，按垂直切片而非按模块推进。

**2026-09-26 状态摘要**：M2 的互联、颜色、源码入口、表格单元格编辑、标签页与 CLI 已接入；M3 中 Mermaid、PDF 和本地语义混合搜索已提前接入，MCP 与审阅写入仍待做。以下带日期的进展保留当时范围，不能把旧条目里的「尚未做」直接视为当前状态。

### 18.1 排期假设

- 单人、业余，按每周约 10 小时、每月约 40 小时估算；估算误差按 ±30% 看待。
- 原计划假设 2026 年 10 月启动；实际 M0 已于 2026-09-15 完成，M2 与部分 M3 能力已提前落地。下方月份保留为最初估算，不作为当前交付承诺；以带日期的进展和验收记录判断完成度。
- 每个里程碑带一份**砍单清单**：进度落后时先砍清单里的项，而不是延期。

### 18.2 节奏纪律（单人项目最重要的部分）

- Spike 一律限时（一周内），到期就按现有数据决策，不追求完美对比。
- 能复用就不自研：gpui-base 文本引擎、zvec、comrak、`ignore`、`notify`。
- 先做窄而深的垂直切片（打开 → 编辑 → 保存 → 搜索），再横向铺功能。
- 不做里程碑范围之外的「顺手」功能；新增想法先记在对应里程碑的后续范围中。
- 每个里程碑结束发一个可安装的 build 给自己日常使用，用自己的 vault 吃自己的狗粮。

### M0 · Spike（约 60 小时，2026-10 → 2026-11 中）

- ~~GPUI + gpui-component 锁定 rev，跑通窗口、VirtualList、Textarea；macOS 中文 IME 手工测试通过。~~ **已完成（2026-09-15，ADR 0001）**：采用 `gpui-kit =0.6.1`，12 项验收全部通过。
- ~~comrak vs markdown-rs：在测试语料上测 inline 位置精度与往返，限时一周定型。~~ **已完成（2026-09-15，ADR 0002）**：comrak。
- ~~Rope 选型：anchor 漂移、10 MB 文件的编辑延迟。~~ **已完成（2026-09-15，ADR 0004）**：`ropey =2.0.0-beta.1`（与 gpui-base 同型，字节索引）+ aero-core 自建 marker 表做 Anchor；10 MB 上编辑 0.4 µs、快照 O(1)、marker 平移 49 µs，1 万次随机编辑后锚点零错误。M0 四个 spike 全部关闭。
- ~~zvec-rust 专项：`bench-10k` 查询延迟、高频小 upsert 的写后可见延迟、macOS dylib 进 `Contents/Frameworks` 并签名公证的完整链路、`ZVEC_LIB_DIR` 离线构建、是否可静态链接。~~ **已完成（2026-09-15，ADR 0003）**：zvec 定为唯一索引引擎。查询 p95 ≤ 3 ms、写后 2 ms 可见、bundle 与离线构建跑通；静态链接放弃；发现 `delete_by_filter` 僵尸主键 bug 与 jieba 全词典 200 MB 内存两项，均有绕过方案（10.3）。
- ~~产出：`aero-core` 骨架、`FullTextIndex` / `VectorIndex` trait、往返测试框架、`docs/decisions/` 记录。~~ **已完成（2026-09-15）**：`crates/aero-core` 含 `text`（Rope + Edit）、`anchors`（marker 表）、`block`、`parse`（comrak + 后处理，8.2）、`document`（两段式编辑管线、undo、7.3 身份匹配）、`index`（trait + 内存实现）、`vault`（路径、枚举、原子写）；`tests/roundtrip.rs` 覆盖 CommonMark spec 655 例、样本、合成文档、随机编辑序列（proptest）与可选的真实笔记目录（`AERO_CORPUS_DIR`）。spikes 目录保留到 M1a 开始前再删。

### M1a · 能打开、能编辑、能保存（约 160 小时，2026-11 中 → 2027-03）

- 打开 vault、文件树（`*.assets/` 折叠到所属笔记下）、打开/新建/重命名笔记。
- 块级 Live Preview 编辑器：段落、标题、无序/有序/任务列表、代码块、分割线；引用/callout 与表格以 `Raw` 显示并可编辑源码。
- 单一 undo 栈、块级选区、`/` 菜单的最小集合（切换块类型）。
- 自动保存、原子写、外部修改静默重载；有未保存修改时的冲突副本。
- 验收：往返测试 100% 通过；输入延迟与启动指标达标；IME 用例通过；**自己开始用它记日常笔记**。
- 砍单清单（按顺序）：`/` 菜单 → 任务列表 → 有序列表 → 文件重命名。
- **进展（2026-09-15，第一刀）**：`crates/aero-app` 可运行（`cargo run -p aero-app -- <vault>`）。已有：首次运行页（打开 / 新建 vault、最近列表）、三栏 + 状态栏（可拖拽、可折叠、状态持久化到 `state.toml`）、文件树（`daily/` 倒序、⌘N 新建到选中目录）、右侧大纲、块级 Live Preview 编辑器（段落 / 标题 / 列表 / 任务 / 代码块 / 分割线渲染，引用 / callout / 表格 / 脚注 / directive 整体作为源码单位）、⏎ 拆块与列表续行、块首 ⌫ 合并 / 脱离列表、↑↓ 跨块、文档级撤销（600 ms 内输入合并）、500 ms 自动保存 + 失焦 / 切换 / 退出落盘。编辑器的结构性操作（`editor/ops.rs`）是纯函数，配合 `TestAppContext` 无头测试（`editor/tests.rs`）覆盖了整条"按键 → Document → 磁盘"链路（输入、⏎ 拆块 / 续行 / 退出列表、⇧⏎ 软换行、代码块内 ⏎ 只换行、块首 ⌫、↑↓ 跨块、撤销 / 重做后光标落在受影响区间、debounce 与失焦落盘）。
  两个实现上的教训：⏎ 必须作为 `SplitBlock` 动作在 `Input` 上下文截获——gpui-base 的 `PressEnter` 事件在 submit 模式下会继续 propagate，平台随后把 `\n` 当作输入塞回控件；控件内 ↑↓ 依赖像素布局，无头测试里不可靠，跨块的 ↑↓ 由我们自己按逻辑行判断所以能测。
  外部修改（第 12 节的 M1a 子集）：编辑器每 2 s `stat` 一次 + 窗口激活时检查 + 每次写盘前校验指纹（mtime/size 变了再比内容 hash）；无未保存修改则静默重载，有则先把磁盘版本写成 `<name>.conflict-<本地时间>.md` 再落盘本地版本并提示——绝不静默覆盖。`notify` 监听与三方合并留给 M1b。`*.assets/` 目前直接不显示（M2 有附件后再折叠到所属笔记下）。
  点击定位：点浏览态的块先进入编辑态，隔两帧把同一次点击重放（`Window::dispatch_event`）给刚出现的控件，由它按像素放光标；渲染文本与源码只差行内标记，所以是"近似到字符"（真正的源码位置映射要等 TextView 暴露 source map，或自绘渲染层）。
  尚未做：单一 undo 与控件内选区的联动、块级选区、文件被外部删除的"已删除"状态、软换行时首/末行按视觉行判断（现在按逻辑行）。
- **进展（2026-09-15，第二刀）**：砍单清单上的东西都做了，没砍。
  `/` 菜单（`editor/slash.rs` + `ops::convert`）：块首或空格后的 `/` 触发，`/query` 字面留在文档里、边输入边过滤（标签 / 英文 / 拼音首字母），↑↓ 选、⏎ / Tab 确认、Esc 只关菜单保留字符、无匹配自动关；确认 = 删掉命令再转换当前块（两步 `edit` + `edit_coalescing` 合成一个撤销组），空段落上则写出骨架（`- [ ] `、```` ``` ````…）。条目是 7.1 的最小集合：文本、标题 1–3、无序 / 有序 / 任务、引用、代码块、分割线、今天日期。`⌘⌥0–8` 直接转换。浮层用 `deferred(anchored())` 锚在控件的 `cursor_layout()` 下一行，越界贴窗口边。
  转换是纯函数：标题去 `#`、列表项从行首吃掉缩进与标记、代码块去围栏，再套目标前缀；分割线在非空块后面插一条并新开段落。引用整体是源码单位，从引用转出去暂不支持。
  重命名（5.1）：点标题变成单行输入框，⏎ 提交、Esc / 失焦取消；有 front matter `title:` 时改它（含 `:` 等字符自动加引号），否则发 `RenameRequested` 让工作区做 `Vault::rename`（拒绝覆盖、`<note>.assets/` 跟着搬、目录不存在则建）。链接同步更新仍是 M2。
  日记（3.4）：`⌘⇧D` 打开 / 按 `# YYYY-MM-DD` 模板创建 `daily/YYYY-MM-DD.md`。
  删除（4.1）：文件树右键"移到废纸篓 / 在 Finder 中显示 / 复制路径"，树内 `⌘⌫`，文件菜单同项；用 `NSFileManager trashItemAtURL:` 进系统废纸篓（可放回）。删打开中的笔记时先丢掉编辑器——它排队中的自动保存会把文件写回来。`.assets/` 的询问对话框（10.5）留到 M2 有附件时。
  教训：`/` 菜单的 ⏎ / ↑↓ / Tab 都得先于块操作检查菜单是否开着，Tab 还要单独绑一个 `MenuConfirm` 动作，因为 gpui-base 把 Tab 用于缩进；带数据的动作（`ConvertBlock(u8)`、`TrashNote(String)`）用 `#[action(no_json)]` 免掉 serde / schemars 依赖。
  尚未做：单一 undo 与控件内选区的联动、块级选区、块 gutter（`⠿` / `+`）、冲突 / 删除横幅（现在是通知）、外部删除的"已删除"状态、设置页。
- **进展（2026-09-16，第三刀，M1a 收尾）**：块选中态、块 gutter、横幅、"已删除"状态都做了；设置页按 1.7"少设置"原则留到有第二个值得反复调的项再说。
  块选中态（`ui_design.md` 6）：选区以**编辑单位下标**记 `anchor / head`，与 `focused` 互斥。编辑态 Esc → 选中当前块（空位上直接回浏览态），再 Esc → 浏览态；↑↓ 收成一块并移动、⇧↑↓ 扩展、到头停住；编辑态里光标已在首 / 末行时 ⇧↑↓ 把字符选区升级为块选区（否则 propagate 给控件）；⏎ 编辑 `head` 那块（光标在块末）；⌫ / ⌦ 删除（`ops::delete_units`：整行删、连带之后的空行、删到文末时收掉前面多出的空行，一个撤销组，选区落到原位置的下一块，文档空了回浏览态）；⌘C / ⌘X 以 Markdown 源码复制 / 剪切（`ops::units_span`：列表项按整项含嵌套子项）；撤销 / 重做在块选中态下重新选中受影响处的块。键位绑定在 `Editor` 上下文：这些绑定在控件聚焦时也匹配但深度更浅，只在 `Input` 层全部 propagate 之后才轮到，处理器在没有块选区时一律 propagate，所以不会抢控件的 ⌫ / ⌘C。选中块画成 `theme.selection` 底色；`head` 那块超出视口时最小滚动进来（搜索定位仍放视口 1/3）。
  块 gutter（5.3）：每个编辑单位左侧悬停出现 `⠿ +`。`⠿` 进入块选中态；`+` 在块下方新开一个内容为 `/` 的段落并把焦点放在 `/` 后，随后按 `/` 菜单的常规逻辑开菜单（Esc 关菜单时 `/` 留在文档里，与手敲一致；列表项后插会把列表断开，菜单里选列表类型即续上）。布局上把正文列左内边距多留 48 px，每个单位行 `ml(-reach) pl(reach)` 向左伸出去画按钮（嵌套列表 `reach` 再加标记列宽，各层级按钮左对齐），这样鼠标从块移到按钮上仍在同一个 `group_hover` 内、按钮不会半路消失；正文仍是 720 宽。块菜单、拖拽重排、`⌘D` 复制块、`⌘A` 两次全选留到 M2 之后。
  无头测试：`block_selection_keyboard_flow`（Esc 两段、↑↓ / ⇧↑↓、⏎、⌘C / ⌫ / ⌘Z / ⌘X、删空）、`shift_arrows_upgrade_to_block_selection_at_block_edges`、`gutter_inserts_below_and_selects`（布局对齐 + 720 宽 + 点击行为）；`ops` 层的 `units_span` / `delete_units` / `insert_slash_below` 单测。
  尚未做（有意留下）：单一 undo 与控件内选区的联动、软换行时首 / 末行按视觉行判断、块选中态下直接输入字符进入编辑态（IME 组合键下 key_char 不可靠，等有源码位置映射再做）。

### M1b · 能搜（约 60 小时，2027-03 → 2027-04）

- 全文搜索（zvec FTS + jieba，含内存 overlay）、搜索结果定位到块、命令面板。
- callout 渲染、表格只读渲染。
- 分发链路：签名、公证、`.dmg`、~~自动更新~~（已砍，见下）。
- **M1b 结束时公开仓库。**
- 砍单清单：自动更新 → 表格只读渲染 → 命令面板。（实际只砍了自动更新。）
- **进展（2026-09-15，第一刀，提前动工）**：全文搜索这条垂直切片先做了，因为它决定 `aero-core` 的块抽取形态。
  `aero-core::index`：`blocks_for_note` 把文档的叶子块变成 `IndexBlock`（front matter / 空块不进索引），`plain_text` 去掉标题 `#`、列表标记、围栏、行内标记与 `[[a|b]]` 的别名侧（`![[img|400]]` 取目标侧），`{...}` 属性整段丢弃。主键 `note_id#ord`——zvec 的 pk 不允许 `:`。
  `crates/aero-index`（新）：`ZvecFts` 实现 `FullTextIndex`（jieba + lowercase，`+term -term "phrase"` 走布尔表达式，其余按自然语言、相邻词 AND；集合损坏时删掉重建）；`Meta`（SQLite，`notes` 表记 `mtime/size/hash` 与块数，带 `index_state` 版本号，版本不符就重建）；`Indexer` 串起 Vault / Meta / FTS：`plan_sync` 列出变化与消失的笔记，`sync_step` 逐篇处理，保存路径严格是 `upsert` + `delete(旧 pk)`（10.3），每 1 万块 flush 一次。索引在 `~/Library/Application Support/Aero/index/<vault-id>/{meta.sqlite,fts/}`。
  `aero-app`：`SearchService` 在专用 OS 线程跑全量同步（每篇一次拿锁，搜索与增量更新可以插队），状态栏显示"正在索引 n/m"；保存 / 重命名 / 删除 / 外部重载都走增量；空闲 30 s `flush()`，退出时同步 flush。`build.rs` 给二进制加 rpath（10.4 的 bug 绕过）。
  搜索视图（`⌘⇧F`，左侧栏 文件 / 搜索 分段切换）：150 ms 防抖，结果按笔记分组，snippet 与高亮在客户端基于块正文做（zvec 无 snippet API），↑↓ / ⏎ / Esc 键盘可用；点击命中打开笔记并滚到该块（放在视口 1/3 处）、短暂高亮 1.5 s。滚动定位靶在块 `div` 的 `on_children_prepainted` 里算偏移写回 `ScrollHandle`——GPUI 的普通滚动容器没有 scroll-to-element。
  快速打开（`⌘P`）：标题 / 路径模糊匹配（前缀、连续、词首加分，长路径轻罚），空查询列最近打开；无匹配时 ⏎ 直接新建同名笔记。它就是命令面板的雏形，命令面板后续复用这个壳。
  无头测试覆盖：索引往返（内存引擎与真 zvec 各一份）、同步计划 / 增量三条路径、搜索视图端到端、快速打开过滤与新建、编辑器 reveal 滚动与高亮。
  尚未做：`optimize()` 的定时策略、`notify` 文件监听（现在只在启动同步 + 编辑器自己的 2 s stat）、分发链路。
- **进展（2026-09-15，第二刀）**：`#tag` 过滤与 callout / 表格只读渲染。
  标签是**笔记级**的：front matter `tags:` / `tag:`（标量、列表、逗号分隔的老写法都认，单项不合规只丢那一项）+ 正文行内 `#tag`（comrak 树上只看 Text 节点：标题、图片 alt、链接 URL、`[[#heading]]` 一类 wikilink 锚点、代码、`{...}` 属性都不算），归一成小写；同一篇的每个 `IndexBlock` 带同一组。查询语法 `#tag` / `tag:#tag`，多个标签 AND；`FtsQuery::parse` 把它们从查询文本里摘出来，引号内的 `#` 保留字面。zvec 侧多一个 `tags_filter` 字段，存 `|<hex(tag)>|` 串，用 `LIKE '%|hex|%'` 做精确匹配（hex 里没有通配符，也不会被 SQL 注入）；`FTS_VERSION` 记在 `meta.sqlite`，不符或 `fts/` 被重建时整体重索引。**纯标签查询每篇笔记只返回 `block_ord == 0`**——标签是笔记级的，否则一篇长笔记的几百个块会吃满 `limit`、挤掉别的笔记；带文本仍是块级。
  callout（`editor/preview.rs`）：借 `TextView::markdown_block_parser` 把 Blockquote 节点识别成 `[!kind]±`，自绘标题栏（图标 / 颜色 / 折叠按钮），正文再起一个 TextView 递归渲染（嵌套 callout 天然支持）；折叠状态放 `PreviewState`（按 视图 id + 块内偏移 存），不写回源码，外部重载时清空；搜索定位到折叠 callout 里的块会把它展开。表格：`TextViewStyle.table` 加横向 overflow scroll，表头半粗，横向滚动手势不冒泡到文档。
  尚未做：`-#tag` 排除与 `tag:rust`（不带 `#`）语法、`#rust` 匹配子标签 `rust/cli`（现在是精确匹配）、查询里 `#rust,` 这类带尾标点的标签不识别。
- **进展（2026-09-16，第三刀，M1b 收尾）**：上面两段"尚未做"里除自动更新之外的全部。
  `notify` 监听（第 12 节，`aero-app/src/watch.rs`）：`RecommendedWatcher`（macOS 上是 FSEvents）递归监听 vault 根；事件进专用线程，按 300 ms 静默窗口合并、单批最多攒 2 s，一批分成 `changed`（现在存在的笔记）/ `removed`（已不在的笔记）/ `rescan`（目录级变化或事件溢出）；隐藏路径、`*.assets/` 内的东西直接忽略。前台**定时取批**而不是被线程唤醒——GPUI 的测试调度器不允许外部线程唤醒任务（索引服务同款做法）。FSEvents 报的是解析过符号链接的真实路径（`/var` → `/private/var`），所以按用户给的根与 canonicalize 后的根各试一次 strip_prefix。工作区收到一批：打开中的编辑器立刻 `check_external`、文件树重扫、索引对 `changed ∪ removed` 逐篇 `refresh_note`（按元数据指纹筛掉自己刚保存的那些），`rescan` 时再做一次增量 `sync_all`。监听起不来（权限、fd 耗尽）时退回原来的 2 s 轮询并弹一条警告。监听只回答"哪些路径动过"，不判断是谁动的：编辑器自己的保存也会触发事件，下游按指纹筛。
  横幅（`ui_design.md` 10.2，替代之前的通知）：文档顶部非模态一条。蓝"已重新载入"10 s 自动消失；黄"冲突"（磁盘版本已另存为副本，本地已照常落盘）有"保留我的 / 使用磁盘版本 / 打开副本"；红"已删除"（文件在外部被删）停止自动保存、内容留在编辑器里，有"恢复 / 另存为… / 关闭"。`save_now` 写前的 `disk_state()` 统一给出 `Unchanged / Changed / Missing` 三态；文件消失时**不写**——自动保存不该把用户刚删的文件变回来，"恢复"走越过校验的 `start_write`。副本、另存、恢复触碰的路径以 `PathsTouched` 事件告知工作区，文件树与索引跟上。
  `optimize()` 策略（10.3）：全量同步后若索引 / 删除的笔记 ≥ 50 篇合并一次段文件；平时计增量写入，空闲 flush 时"累计 ≥ 200 次且距上次 ≥ 10 分钟"才 optimize，之后计数清零。
  标签查询小项：`-#tag` / `-tag:x` 排除（zvec 的过滤表达式没有 `NOT LIKE`，用多取一批再在 Rust 侧剔除实现）、`tag:rust` 不带 `#` 也算包含、`#rust` 匹配子标签 `rust/cli`（索引侧 `tags_filter` 写入全部祖先，`FTS_VERSION` 升到 3 触发重建；内存实现用 `tag_matches`）、`#rust,` 之类的尾标点剥掉。
  分发链路（第 15 节）：`scripts/bundle-macos.sh` 设了 `AERO_SIGN_IDENTITY` 就用 Developer ID 正式签名（`--options runtime` + `resources/macos/entitlements.plist`，zvec dylib 一并签），否则 ad-hoc；`scripts/dmg-macos.sh` 用 `hdiutil` 出 UDZO 镜像（内含 Aero.app 与 `/Applications` 快捷方式，有身份时也签 .dmg）；`scripts/notarize-macos.sh` 用 `xcrun notarytool submit --wait` 公证并 `stapler staple`，凭据来自 `AERO_NOTARY_PROFILE`（钥匙串配置）或 `AERO_NOTARY_APPLE_ID / TEAM_ID / PASSWORD`，提交前先拦下 ad-hoc 签名（注意别用 `| grep -q`：pipefail 下 grep 提前退出会让 codesign 吃 SIGPIPE，判定不稳定）；`scripts/release-macos.sh [--notarize] [dist/]` 串起来。`scripts/test_bundle_macos.py` 覆盖 bundle 结构、签名、dmg 内容与挂载后签名、公证脚本的预检（缺凭据 / ad-hoc / `--notarize` 不触发构建）。**自动更新按砍单清单砍掉**：M1 的发布 = 上传 `.dmg`。
  M1 的功能主流程至此收口；这不等于 IME、性能与正式分发已全部验收，当前状态以 [M1 验收记录](qa/m1.md) 为准。
- **验收补充（2026-09-16）**：修复冲突副本写入失败后仍覆盖原笔记的问题，磁盘校验失败时停止保存；冲突副本以原子 create-only 方式创建，同名时另取名称。保存失败阻止切换笔记、切换/关闭 vault，普通 Quit 与窗口关闭也检查保存结果。新增保存失败/重试、连续冲突、IME 预编辑/提交/取消及文档撤销回归测试；新增 `scripts/check-m1.sh`、GitHub Actions 工作流和 `aero-index/examples/m1_bench.rs`。本机自动化通过情况、原生 IME 人工清单、性能结果与正式公证待验项分别记录在 `docs/qa/`，不将未验项目标为通过。
- **进展（2026-09-16，wikilink 提前动工 + 复审修复）**：M2 的 `[[...]]` 解析、跳转与从未解析链接建笔记先做了一版（另一位 agent 实现），复审后修了下面这些。
  `aero-core::links`：`scan` 走 comrak 树（代码块 / 行内代码 / `![[embed]]` / 跨行的都不算），`[[a\|b]]` 与 GFM 表格里的 `\|` 一致当别名分隔；`[[#标题]]` 保留为"空目标 + 片段"表示同篇锚点。`Resolver` 用哈希表按 路径 → 唯一文件名 → front matter 标题 三级解析，**大小写不敏感**，多个候选时返回 `Ambiguous`（之前当作未解析会另建一篇同名笔记）；`contested()` / `neighbors()` 给索引侧做定向重判。索引时标签抽取与 wikilink 扫描共用一次 comrak 解析（`parse::inline_scan` / `index::blocks_and_links_for_note`）。
  `aero-index`：`links` 表（源 / 目标 / 已解析 note_id / 片段 / 字节区间，`schema 3`）。全量同步先给未入库的新文件预分配 ULID，链接在入库时就能解析；链接行按 256 篇一个事务攒批写，`finish_sync` 结束时只重判"未解析 + 名字有竞争"的行。删除 / 重命名 / 改标题时把指向这篇的行退回未解析再重判（之前删了重建的笔记，别人指向它的旧行永远不会更新）。100k 基准回到 M1 基线水平（见 `docs/qa/performance.md`）。
  点击链路：`preview.rs` 之前把 `aero://note/` 前缀剥掉再发事件、工作区又拿它去 `decode_note_url`，解码必失败——所有点击静默无效；块的 `on_mouse_down` 进编辑态又抢在 `TextView` 的 `on_link_click`（mouse up）之前把预览换成了输入框。现在预览直接发 `LinkClicked { target, fragment }`，进编辑态改到 `on_click`；工作区处理 打开 / 歧义提示（列出候选、不建文件）/ 同篇锚点 / 新建 + 撤销（撤销先移到废纸篓成功再解绑编辑器，已编辑或正在保存的不撤销）。`reveal_heading` 比较前去掉行内标记、折叠空白、忽略大小写。
  **退出崩溃**（三份 2026-09-16 的崩溃报告，`SIGSEGV` / `SIGABRT` 都在 `libzvec_c_api`）：主线程 `-[NSApplication terminate:]` → `exit()` 在跑 C++ 静态析构（销毁 zvec 全局线程池）的同时，最后一个持有 `Arc<Mutex<Option<Indexer>>>` 的后台任务（比如刚保存触发的 `note_saved`）在 GCD 线程上 drop 了 `Indexer` → `zvec_collection_close` 与静态析构撞车。修法：`SearchService::close_blocking` 在退出 / 关闭 / 切换 vault 前于**主线程**停掉同步线程（join）、flush、把句柄从 `Option` 里拿出来就地关掉，之后的后台任务看到 `None` 直接返回；`Drop` 走同一条路。顺带修 `ZvecFts::open`：引擎报 `Can't lock read-write collection` 时不能再"删目录重建"——那是另一个进程（或本进程还没关掉的旧句柄）正在用这份索引，删了对方的后台线程立刻段错误（15:26 日志里 `cargo run` 与 dmg 里启动的 Aero.app 同时开着同一 vault 就是这么撞的）；现在报错、搜索不可用、编辑照常。`open_vault` 改成新 vault 打得开才关旧的。原生冒烟：应用包身份启动，索引建好后 / 同步途中各退出 5 次，退出码全 0、无新崩溃报告、下次启动无"crash residue"。
  当时尚未做：未解析链接的视觉区分（`TextView` 的链接样式不能按 URL 区分）、`![[embed]]` 渲染、反链面板（后续进展见 M2）。

### M2 · 知识互联（约 155 小时，2027-05 → 2027-09 初）

- **状态核对（2026-09-22，按当前实现）**：链接、反链、重命名同步和补全主流程已落地，aliases / hover / 未解析视觉 / 反链计数筛选已实现；M2 尚未完成。

  | 范围 | 已实现 | 尚缺 |
  | --- | --- | --- |
  | 笔记互联 | wikilink 解析 / 跳转、未解析链接创建、标题与块定位、反链面板、`aliases` 解析 / 搜索、**富文本链接悬停预览**、预览设置（悬停 / 按住 ⌘ / 关闭）、未解析 / 歧义链接视觉区分、反链计数 / 块引用筛选、**反链列表富文本片段**、**跨篇 fragment 缺失立刻虚线** | 原生视觉验收 |
  | 补全与输入 | 笔记、标题、块、标签、嵌入 / 附件补全，按需写 `^id`，`⌘K`，自动配对，粗体 / 斜体 / 代码 / 删除线 / 黄色高亮快捷键 | 颜色入口及浮动工具条见下 |
  | 重命名与移动 | 文件 / 文件夹树内拖放与跨目录移动、F2 / 右键改名，入链与相对出链同步（Wikilink、普通 Markdown / 图片 / 引用定义、显式附件路径），标题入口联动 front matter title 引用、**拖到文件树的 Finder 导入**、**跨文件系统移动（硬链接失败时复制再删源）**、**外部改名按内容指纹改写入链** | 大库性能 / 原生人工验收 |
  | 标签 | 正文 / front matter 索引、补全、搜索过滤、**标签侧栏分级树与点击搜索（4.3）** | 右键重命名标签（M4）。正文 / 属性条点标签仍是编辑，不跳搜索 |
  | 附件与嵌入 | 图片 / PDF 枚举、路径解析与源码插入、渲染服务骨架（两级缓存 / 占位 / 失败重试）、图片块渲染与全屏看图、`⌘V` 导入位图与文件、**拖放到编辑区（7.6）**、**拖到文件树的 Finder 导入**、笔记 / 章节 / 块嵌入的只读渲染、`.assets/` 在树里折叠展示（📎 计数）、引用计数与孤儿标记、**图片宽度预设（小 240 / 中 400 / 大 680 / 原始）**、**SVG 光栅化**、**附件改名 / 删除**、**引用目标写入索引** | PDF 页面（M3） |
  | 数学 | 语法识别、源码保留、`$$` 块按 RaTeX 渲染位图（主题色、内嵌字体、失败回退）、**行内 `$…$`**、**编辑公式块时源码下的实时预览**、**超宽公式横向滚动** | M4 的 display list 直绘 |
  | 颜色与格式 | 块属性等基础解析及源码保留、**块与行内颜色的写回 / 读回**（9 色调色板、叠加、去色、保留未知属性与 `^id`、黄底走 `==`）、**13.3 的 `content.*` 色值令牌（浅深两套、AA 对比度）与预览上色**、**`⌘⇧H` 应用上次使用的颜色**、**颜色菜单（7.7）与 `/颜色` / `/红色` 斜杠命令**、**选区浮动工具条（5.9）**、**工具条 `⋯`（复制块链接 / 添加 `^id`）**、**选区盖住整块时颜色菜单的「应用到整块」**、**块菜单（转换 / 颜色 ▸ / 复制链接 / ^id / 复制 / 删除）**、**源码态淡化颜色标记**、**多行块颜色写在独占行**、**块拖拽重排（插入线、列表右侧缩进为子项、`Esc` 取消、落位高亮 300 ms；不跨笔记）** | |
  | CLI | 桌面程序接受 vault 路径参数、**`search / read / backlinks / append / new / index / export`**（可 `--json`）、**图形界面占用索引锁时，读 / 导出 / 新建 / 追加仍可用，搜索 / 反链 / index status 复制一份索引来读** | `index rebuild` 仍要等引擎放开排他锁 |

  PDF 页面渲染、Mermaid、MCP 与语义检索属于 M3；标签重命名、未链接提及在 UI 计划中列入 M4。以上是源码状态，不代表已安装应用已更新。
- **下一步**：字符级跨块选区、表格增删行列仍不在这一刀。编辑器默认观感（9.1）已落地：聚焦块隐藏标记，源码按块或按篇打开，「聚焦时显示源码」是偏好。
- **修复（2026-09-23，打开大库卡死）**：打开 `~/code/wLog` 后卡死约 3 分钟。一是每块一个文本控件后，gpui-base 的光标在没聚焦时也会被编辑触发开始闪，上千个计时器让整窗每秒重绘 6–8 次、空闲 CPU 90–100%；改为只在聚焦后闪。二是文件监听每批（包括自己的自动保存）都在界面线程上同步重扫整个库，安全软件审计文件访问时一遍一秒以上；改为只在增删、目录、附件变化时重扫，并放到后台。修后同一篇空闲 CPU 1.5%，自动保存不再触发重扫。详见 [浏览与编辑一致验收](qa/m2-seamless-edit.md) 的性能修复一节。
- **进展（2026-09-23，标签页与窗口外框）**：编辑区有了标签页，每篇文档一页：已打开就切过去，空白的「新标签页」被复用，搜索 ↑↓ 共用一个预览页；⌘T / ⌘W / ⌘⇧T / ⌘⇧[ ] / ⌃⇥ / ⌘1–9，中键关闭、拖动排序。没显示的页保留编辑器（撤销、滚动），切走时落盘进索引，切回时检查外部修改；标签页按 vault 记在 `state.toml`，重启后恢复（只加载当前页）。标题栏按三栏对齐：文件 / 搜索 / 标签与左侧折叠、标签条与 `+`、反链 / 大纲与右侧折叠。编辑区顶部显示 `vault › 文件夹 › 笔记.md`，可点，带收藏星标。资源管理器加上收藏分区、vault 名与路径、文件夹开合图标和已打开笔记的标记。三栏布局重写：两侧宽度固定、窗口变化只改编辑区，拖动时锁定光标，拖过最小宽度收起、双击复位；修掉左栏收起后拖右侧分割线跳到上限、窗口缩放时侧栏按比例变宽，以及状态栏一直被挤出窗口的问题。补上两个一直画成空白的图标（新建笔记、新建 Vault），并加了「源码里用到的图标都打进资源包」的检查。新增 11 项回归，全量 **564 项通过**（app 328、链接绘制集成 17、core 162、往返 5、index 51、反链计数集成 1）。详见 [标签页与窗口外框验收](qa/m2-workspace-chrome.md)。
- **进展（2026-09-23，浏览与编辑同一副样子）**：文字类块在浏览与编辑时由同一个文本控件画，点进去不跳（9.1）。标记始终藏着（⌘K 改地址时除外）；←→ 跳过藏起来的字节，⌫ / ⌦ 删看得见的字、删空一对标记时连标记一起删；光标不停在藏起来的 `# ` / `> ` 前。折行按整行字形排版测宽，并处理中日文避头尾（`。` `，` 不落到行首）。表格按格编辑、可拖列宽；任务方框可点；callout 同一个外框、折叠按钮与缺失的图标补齐；代码块不再露出围栏；反链面板的 `==` 与 `{.red}` 按预览渲染。新增 8 项回归（点进每一段所有块纹丝不动、跨块直接拖选、方框就地勾选、单元格输入不拆表、聚焦后不露语法、行尾的字不被裁掉、↑↓ 按视觉行与表格列走、代码块只编辑围栏之间），全量 **553 项通过**。详见 [浏览与编辑一致验收](qa/m2-seamless-edit.md)。
- **进展（2026-09-22，M2 续作）**：补上几项状态表里还空着的能力。选区工具条 `⋯` 可以复制块链接、给没有 `^id` 的块补上并复制。Finder 拖到文件树：`.md` 进目标目录，其他文件拖到笔记上则进它的 `.assets/`，拖到文件夹上的非笔记会说明原因。选中图片后出现小 / 中 / 大 / 原始，分别写 `|240`、`|400`、`|680` 或去掉宽度参数。反链列表用来源块的 Markdown 做预览，不再是剥掉标记的纯文本。`aero export <note> --format md|html|json` 可用；图形界面占着索引锁时，读、导出、新建、追加仍走磁盘，搜索、反链和 `index` 会说明要等退出（引擎的 LOCK 是排他的，只读打开也拿不到）。行内 `$…$` 渲染成位图，编辑 `$$` 块时源码下面保留实时预览，超宽公式横向滚动。跨篇 `#标题` / `#^块` 若目标里没有，打开笔记就画虚线，不用等悬停。同一知识库内的移动若硬链接报跨设备错误，改为复制再删除源。详见各验收记录。
- **进展（2026-09-22，拖放导入）**：编辑区可以接收拖放了（7.6）。从 Finder 拖进来的文件和 `⌘V` 同一套规则：复制进当前笔记的 `.assets/`，图片 / PDF 插入嵌入，其他文件插入链接，重名追加 `-2` 不覆盖，文件夹直接跳过。从文件树拖一篇笔记进来插入 `[[路径]]`，按住 ⌥ 则是 `![[路径]]`；文件夹和别的库的条目不接收。拖过编辑区时有高亮。插入位置与粘贴相同，一次插入是一个撤销组，撤销不删已经落盘的附件。拖到文件树上的 Finder 导入还没做。新增 2 项回归，全量 **514 项通过**（app 285、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。详见 [拖放导入验收](qa/m2-drop.md)。
- **进展（2026-09-22，CLI）**：`aero` 在子命令下不再打开窗口。`search` 走现有全文索引（块级结果，`#tag` / `--tag` 与搜索框同一套语法）；`read` 按路径、文件名、标题、别名或 note id 读出笔记或 `^块`；`backlinks` 列出入链；`append` 在末尾加一段 Markdown；`new` 新建笔记（不覆盖，可用 `.aero/templates/`）；`index status` / `index rebuild` 查看或按磁盘重建索引。库目录用 `--vault`、环境变量 `AERO_VAULT`，或最近打开且还在的库。索引仍在应用数据目录的 `index/<vault-id>/`，和图形界面是同一份。`--json` 把结果打到标准输出。Aero 正开着这个库时索引文件被锁住，命令会说明先退出。新增 6 项回归，全量 **512 项通过**（app 283、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。详见 [CLI 验收](qa/m2-cli.md)。
- **进展（2026-09-22，富文本悬停预览与预览设置）**：链接悬停浮层不再是剥掉标记的纯文本，而是用同一套预览渲染目标片段：标题、强调、列表、callout、颜色都在，只读、可滚动。整篇仍大约前 300 个字符（按纯文本计，不切开一个字符），从包含第一块的根块起切，所以 callout / 列表的标记还在；`#标题` / `#^块` 仍是整个片段。同篇用未保存文档，跨篇后台读盘，外部链接只显示地址。视图菜单「链接预览」和命令面板可以在「悬停 / 按住 ⌘ / 关闭」之间换，记在应用级 `state.toml`（和侧栏开关同一份）。「按住 ⌘」是悬停着按下才出现、松开就关，不再等 500 ms；「关闭」则完全不弹出。完整的 `⌘,` 设置页还没有。反链列表面板仍是纯文本摘要。新增 5 项回归，全量 **505 项通过**（app 276、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。详见 [链接预览验收](qa/m2-link-preview.md)。
- **进展（2026-09-22，标签视图）**：左侧栏第三页「标签」接上了。`rust/cli` 显示为 `rust` 下的 `cli`，只出现在子标签里的祖先也会占一个节点；每项带篇数，按「点这项会搜到多少篇」算（父标签含子标签，与 `tag:#rust` 同一条规则）。默认全部展开，点左侧三角折叠。点击标签切到搜索视图并立刻执行 `tag:#rust/cli`（不等搜索框那 150 ms 防抖）。目录读索引的 `note_tags`，同步未完成或引擎不可用时扫磁盘；保存后树跟着刷新。命令面板「显示标签」、视图菜单「标签」都能打开这一页。右键改名仍是 M4；正文 / 属性条里的标签点击还是编辑，不跳搜索。新增 4 项回归，全量 **500 项通过**（app 271、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。详见 [标签视图验收](qa/m2-tag-view.md)。
- **进展（2026-09-22，选区浮动工具条）**：5.9 的选区浮动工具条接上了。编辑态拖出或用键盘选出一段文字后，选区首行上方 8 px 浮出小条（顶部放不下就翻到下方）：粗体 / 斜体 / 删除线 / 行内代码 / 链接 / `A ▾` 颜色 / `转换 ▾`。键盘改选区后等 300 ms 才出现（打字过程中不闪），鼠标松手立刻出现。当前选区已带的格式与颜色显示按下态，再点一次取消。`A ▾` 打开的就是 7.7 的颜色菜单；`转换 ▾` 列出与 `/` 菜单相同的块类型，点一项走现有的 `convert_at`。打开子菜单时工具条不消失；`Esc` 先关子菜单、再关工具条、再退到块选中态。块选中态不画工具条。`⋯`（复制块链接、添加 `^id`）跟 5.3 块菜单一起做，这一轮不放空按钮。核心层补了 `inline_format::active` 给按下态。新增 5 项回归（core 1、app 4），全量 **496 项通过**（app 267、链接绘制集成 17、core 156、往返 5、index 50、反链计数集成 1）。详见 [选区浮动工具条验收](qa/m2-toolbar.md)。
- **进展（2026-09-22，颜色菜单）**：7.7 的颜色菜单接上了，挑颜色终于有界面。菜单是「上次使用」一行（带 `⌘⇧H` 提示，点它等于按那个键）＋ 过滤输入框 ＋ 文字 / 背景两组网格（默认 + 9 色，每组两行五个，样本用真实 `content.*` 色值画 `A`，当前颜色打勾）。`⌘/`（14 节里块菜单的键位，块菜单本身还没做，先直接开它唯一做好的这一项）、`/颜色`、命令面板的「颜色…」都能打开；焦点借给菜单自己的过滤输入框，所以中文也能筛（「蓝」命中两组的蓝、`bg` 整组命中背景、`blue` / `bg-blue` / 「蓝色背景」都算），`←→` 沿阅读顺序走、`↑↓` 换行并保持列号、`⏎` / `Tab` 应用、`Esc` 只关菜单不动文档也不退出编辑态。改什么在打开那一刻就定下来：编辑态有选区就是那段文字，没选区就是当前块，块选中态是选中的那几块（一个撤销组）。应用完记住这次的颜色，`⌘⇧H` 接着用它。`/` 菜单里颜色是"直接命中"——只在输入了东西之后参与匹配，不把默认菜单刷满 20 项，候选行也画真实颜色的样本。为此把 `/` 菜单的候选从"`ITEMS` 下标"换成 `Match::{Item, Color}`，颜色数据（名字、别名、过滤、网格走位）单独放在 `editor::color_menu` 里，菜单与 `/` 菜单共用一份。核心层补了 `colors::inline_color`（读回选区当前颜色，菜单画勾、5.9 的工具条显示按下态都要它）。顺手修掉 `slash_confirm` 的一处字节切片崩溃：删 `/命令` 时"把前面的空格一起吃掉"是逐字节回退的，`/` 前面是多字节字符（「段落 /红色」）时会切在半个字上 panic——改成按字符串裁尾部空格。新增 6 项回归（core 1、app 5），全量 **491 项通过**（app 263、链接绘制集成 17、core 155、往返 5、index 50、反链计数集成 1）。详见 [颜色验收](qa/m2-colors.md)。5.9 的选区浮动工具条（`A ▾` 打开的就是这个菜单）、块菜单里的"颜色 ▸"、菜单顶部的"应用到整块"开关不在本轮范围。
- **进展（2026-09-22，颜色令牌与预览上色）**：颜色终于看得见了。主题里定义 13.3 的 `content.*`：9 色 × 文字 / 背景 × 浅深两套，深色下文字提亮、背景降饱和压暗而不是反相；取值按 WCAG AA 卡着回归——文字色对编辑区底色、正文色对背景色都 ≥ 4.5:1，`content.bg.yellow` 就是 `==` 的高亮色（源码态与预览态、`==` 与 `{.bg-yellow}` 同一个令牌）。块颜色在渲染路径上生效：背景在最外层那个带颜色的块上铺一次（铺满块矩形、左右各多 8 px，含子块），文字色往下传给子块与 `TextViewStyle` 的正文色。行内颜色把 `aero_core::highlight::preview` 从"只处理 `==`"扩展成"`==` 与行内属性列表共用一条路"：先扫出承载者（`[文本]{...}` 与紧跟行内元素的 `{...}`，含 comrak 自己解析掉的行内代码 / 链接属性），把标记换成私有使用区的哨兵字符后重新解析，出 HTML 时再换成 `<span class="red bg-blue">`，色值由 app 按令牌填上——core 始终不认识主题。代码 / 数学 / 转义 `\[` 里的 `{...}` 不算属性列表，引用式链接那种被两种形式同时认出的只算一处，跨硬换行的 span 每行各自开合。顺带补上标题：comrak 把 header attributes 收进 `ast.attrs` 但 sourcepos 仍含 `{...}`，现在按段落同样的规则记 `suffix_len`，`# 标题 {.red}` 不再显示字面属性。gpui-base 的富文本层原来只有 `<mark>` 的背景色没有文字色，`forks/gpui-base` patch 给 `TextMark` 加了 `color` 并让 `<span style="color: …; background-color: …">` 生效，显式颜色压过链接色。`⌘⇧H` 从"固定黄色高亮"改成"应用上次使用的颜色"（应用级状态，默认黄色背景，所以行为与从前一致）：编辑态作用于选区、块选中态作用于选中的块（多块一个撤销组），颜色菜单接上后调用的是同一个 `Editor::apply_color`。新增 7 项回归，全量 **483 项通过**（app 256、链接绘制集成 17、core 154、往返 5、index 50、反链计数集成 1）。详见 [颜色验收](qa/m2-colors.md)。颜色菜单（7.7）、选区浮动工具条（5.9）、`/红色` 斜杠命令、源码态的淡色标记不在本轮范围——没有挑颜色的界面，眼下 `⌘⇧H` 只会给出默认的黄色背景。
- **进展（2026-09-22，颜色写回地基）**：新增 `aero-core::colors`，把 6.3.10 的属性列表变成可执行的源码编辑。9 色调色板与 Notion 对齐，`Slot::{Text, Background}` 管类名与归属（文字 `{.red}`、背景 `{.bg-blue}`，两组可叠加）。`block_edit` 负责块上色 / 换色 / 去色：块末追加 ` {.类名}`，换色只动本组，去色后属性空了就删掉 `{}`，不认识的类名 / `key=value` / `#id` 原样保留，`^id` 始终排在属性之后；重复上色与调色板外的颜色都不产生编辑。`block_color` 反向读回当前颜色，供菜单显示选中标记。`inline_edit` 负责行内：没有承载者就包 `[文本]{.类名}`，选区恰好是行内元素时属性挂在元素后（`**粗**{.blue}`），已有承载者就地改，去色后连 `{}` 与包裹的 `[ ]` 一起收掉，黄色背景返回 `Inline::Highlight` 交给 `==文本==` 那条路；改完用 `same_structure` 校验结构未变才放行。4 项回归，全量 **476 项通过**。这一轮只有核心层，UI（调色板菜单、选区浮动工具条、预览上色、`⌘⇧H` 记住上次颜色）与行内颜色写回都还没做，详见 [颜色验收](qa/m2-colors.md)。
- **进展（2026-09-22，数学渲染）**：`$$…$$` 块在预览态渲染成位图，管线是 `ratex_parser::parse` → `ratex_layout::layout`（带主题色与行间 / 行内样式）→ `to_display_list` → `ratex_render::render_to_png`；颜色在排版阶段给定，省掉 9.9 的"单色 SVG + alpha 蒙版"一跳，主题色进缓存键。字体走 `embed-fonts` 内嵌 KaTeX TTF，运行时不依赖 `font_dir`。为此把渲染服务的"源"从"vault 内文件"泛化成 `Source::File | Source::Formula`：图片按文件内容指纹入盘，公式按"源码 + 行间/行内 + 颜色"的指纹入盘（`cache/<vault-id>/math/`），占位、失败回退（源码 + 一行错误）、后台渲染与取消全部沿用图片那套。公式按正文字号排版、按窗口 HiDPI 倍率出设备像素；块渲染拿不到 `Window`，倍率由 `Editor::render` 每帧记下。`aero_core::hash_bytes`（xxh3-64，与 `FileStamp::hash` 同一函数）改为公开，供跨进程稳定的磁盘缓存键使用。新增 4 项回归；全量 **472 项通过**（app 250、链接绘制集成 17、core 149、往返 5、index 50、反链计数集成 1）。详见 [数学渲染验收](qa/m2-math.md)。行内 `$…$`、源码态实时预览、超宽公式滚动与原生视觉验收不在本轮范围。
- **进展（2026-09-22，附件树与引用计数）**：文件树里有附件的笔记行显示 `📎<数量>`，点它展开 / 收起 `<note>.assets/` 下的附件子行（点徽标不打开笔记），附件行只读、双击用系统应用打开、不参与行内重命名；`*.assets/` 仍不作为文件夹出现。新增 `aero_core::attachments::References::read`：扫全库算引用计数，`![[附件]]`、普通 Markdown 图片 / 链接、生效的引用式定义都算，代码 / 数学 / 转义里的不算，`./` 与 `../` 归一到 vault 内路径且不越根；零引用即孤儿。报告由工作区后台算（打开 vault、附件变化、重扫时各一次）并交给文件树标「未引用」，报告未就绪时不标记。新增 3 项回归；全量 **468 项通过**（app 246、链接绘制集成 17、core 149、往返 5、index 50、反链计数集成 1）。详见 [嵌入验收](qa/m2-embeds.md)。视图菜单的「显示附件目录」开关、附件改名 / 删除入口、把计数写进 SQLite 不在本轮范围。
- **进展（2026-09-22，嵌入渲染）**：`![[笔记]]` / `![[笔记#章节]]` / `![[笔记#^块]]` 独占一段时只读渲染目标内容——左侧竖线 + 来源标题，点击按 wikilink 同一条路打开并定位。渲染的是目标源码并交给同一套预览渲染器，嵌入里的标题、行内格式、表格、callout、图片都照常渲染；整篇嵌入剥掉目标的 front matter。同篇嵌入用未保存文档，跨篇后台读盘并缓存；内容由预取维护（打开、正文变化、笔记目录就绪、失效各一次），渲染只读缓存，避免在渲染中修改编辑器自身。被嵌入的笔记保存 / 外部改动 / 删除后工作区让缓存失效重读，片段消失即报错而不复用旧内容。解析不到、歧义、片段缺失统一落到「源码 + 一行错误」。嵌套只渲染一层（`EMBED_DEPTH = 1`），自嵌套与互相嵌入不会转不出来。顺手修掉 `ops::delete_units` 的一处字节切片崩溃：删末块时"收掉前面的空行"用 `text[start - 2..start]`，上一行以多字节字符（如 `。`）结尾就 panic——点图选中块再按 `⌫` 正好触发。新增 4 项回归；全量 **465 项通过**（app 245、链接绘制集成 17、core 147、往返 5、index 50、反链计数集成 1）。详见 [嵌入验收](qa/m2-embeds.md)。`.assets/` 树展示、引用计数 / 孤儿检测、PDF 页面渲染不在本轮范围。
- **进展（2026-09-22，渲染服务与图片）**：按 9.9 落地 `crates/aero-app/src/render.rs`：请求按 `{kind, source, width, scale}` 命中内存缓存，磁盘缓存 `cache/<vault-id>/<kind>/<内容指纹>-w<宽>@<倍率>.png` 按内容指纹命中（改名 / 复制 / 重装都能复用，内容一变自然失效），解码与缩放全在后台线程，首帧按上次已知像素尺寸占位、渲染完成替换，失败统一为「源码 + 一行错误 + 重试」且不写缓存不碰笔记，坏缓存当未命中重渲染，缓存写入走临时文件 + rename。只等比缩小不放大，倍率与宽度都是缓存键。格式藏在 `Renderer` trait 后，数学 / 图表 / PDF 之后各加一个 kind 即可复用缓存、占位、失败回退。附件解析复用 `![[` 补全那套优先级，目录后台枚举一次、之后按文件变化增量维护，不在 UI 线程走整库。图片方面：`![[x.png|400]]` 独占一段时渲染位图（`|400` 为显示宽度，默认正文列宽），混排嵌入仍显示源码；单击选中块、双击全屏看图（`Esc` / `Space` / 点击关闭）；`⌘V` 把剪贴板位图写进 `<note>.assets/<ulid 后 8 位>.png` 并插入嵌入，Finder 复制的文件按原名导入（重名追加 `-2`，不覆盖），图片 / PDF 插嵌入、其他插普通链接，剪贴板里有文字则交还文本控件。文件监听新增 `attachments` 通道（含 `*.assets/` 内的图片 / PDF）只让渲染缓存失效、不触发文件树重扫。新增 16 项回归；全量 **461 项通过**（app 241、链接绘制集成 17、core 147、往返 5、index 50、反链计数集成 1），格式与构建通过。详见 [渲染服务与图片验收](qa/m2-images.md)。拖放导入、宽度预设菜单、SVG 光栅化、笔记 / 章节 / 块嵌入渲染、`.assets/` 树展示与引用计数不在本轮范围，原生视觉验收未做。
- **进展（2026-09-22，示例库）**：无参数启动恢复最近打开且目录还在的库（失效条目跳过、不报错），首次运行或无可恢复的库时停在库选择页；选择页新增「查看示例库」入口，命令行给了路径仍直接打开该库。六篇中文示例（排版、双链与别名、标题 / 块引用、项目清单、移动改名练习）源码在 `assets/demo-vault/`，编译期 `include_str!` 嵌入可执行文件，每个 `.dmg` 带的就是该次构建的示例，不依赖包内文件。首次交付先在临时目录写完整副本再整体 `rename` 发布，失败只留错误不留半个库。示例是用户的笔记而不是缓存，所以交付是增量的：`.aero/demo.toml` 记录每篇最近一次交付的内容指纹，再次进入只替换指纹吻合（用户没动过）的示例并补上新增的示例，改过的保持原样、删掉或改名的不恢复；没有交付记录的同名目录一个字都不写。示例库进最近列表，从最近列表点进去、或启动时自动恢复到它，都走同一条更新路径并停在欢迎页。`Workspace` 改为持有应用数据目录以便测试注入，测试不再写真实用户目录。新增 14 项回归（六篇互链 / 章节 / 块引用全部可解析、重开保留编辑 / 删除 / 改名、版本更新只换未改动的示例、不写非交付目录、占位文件与符号链接拒绝、每篇在编辑器中绘制、内嵌表与 `assets/` 一致、四条启动路径含真实点击）外加一项打包断言（可执行文件里的示例与仓库当前内容逐字节一致）；全量 **445 项通过**（app 227、链接绘制集成 17、core 145、往返 5、index 50、反链计数集成 1），格式、打包与构建通过。详见 [示例库验收](qa/demo-vault.md)。原生窗口视觉、已签名 `.app` 首启动、英文示例与"重置示例库"入口不在本轮范围。
- **进展（2026-09-22，移动与改名）**：文件树支持笔记 / 文件夹拖放到目录或根目录、空目录展示、F2 / 右键行内重命名。复用全库准备 / 备份 / 回滚事务，同步 Wikilink、笔记嵌入、普通 Markdown 链接 / 图片 / 生效引用定义及显式附件路径，同时重基被移动笔记的相对出链。仅编辑器标题入口对 front matter `title` 做引用联动，手改 YAML 不触发跨文件写入；标题碰撞或图片命名空间冲突改用安全笔记路径。保留显示文字、锚点、查询参数、BOM 和混合换行，拒绝覆盖、符号链接祖先与目录自嵌套，目标忽略规则改变可见性时回滚。目录移动索引在有序队列里与磁盘整体对齐。新增原生事件驱动的拖放 / F2 / 标题 / 再次保存回归；全量 **431 项通过**（app 213、链接绘制集成 17、core 145、往返 5、index 50、反链计数集成 1），格式与构建通过。详见 [移动与改名验收](qa/m2-rename.md)。Finder 导入、外部移动识别、跨文件系统移动、全库撤销及原生人工 / 大库性能验收不在本轮完成范围。
- **进展（2026-09-21，aliases / hover / 未解析视觉 / 反链计数筛选）**：front matter `aliases` 支持字符串标量 / 字符串列表，trim 后按大小写不敏感去重；非法 YAML 不产生 alias，不改写原文。resolver 优先级为路径 → 文件名 → 同级的 title / aliases，跨笔记冲突返回歧义、提示候选而不建文件。`[[` 补全、`⌘P` 与全文搜索支持 aliases，候选显示原 title，补全使用经 resolver 验证的规范目标；仅有 front matter 的笔记也可按 alias 检索，正文为空，不暴露其他 front matter 字段。SQLite schema 升为 5，FTS 缓存版本提升；旧缓存重建，不修改 Markdown 笔记。
  仓库内 `forks/gpui-base` patch 提供按 URL 的 solid / dashed 样式及 hover 回调，不修改全局 registry。悬停 500 ms 显示纯文本只读可滚动预览：普通笔记前 300 个 Unicode 字符，标题 / `^块` 链接取对应片段；同篇使用未保存文档，跨篇后台读盘，外部 URL 仅显示地址、不联网。离开链接有 150 ms 宽限以进入浮层，编辑器滚动 / 缩放 / 失焦 / 编辑 / 重载 / 删除会取消，generation 拒绝旧回复。未解析为淡色虚线，歧义为 warning 色虚线并提示候选；跨篇 missing fragment 到悬停读盘才校验，暂无即时淡色保证。
  标题栏反链入口与面板 total 使用独立 COUNT，不从最多 200 条的展示列表推算；当前块有唯一 `^id` 时可切换块筛选，filter 在 LIMIT 前执行。同篇链接、代码、嵌入、未解析 / 歧义链接不计；仍只展示最多 200 条，未链接提及、富文本反链片段与预览设置未实现。
  索引写任务按 FIFO 排队，查询等待调用前已提交的写入；启动期间 rename 在队列内等待同步，连续改名时按磁盘状态清理失效的中间路径。关闭仍在主线程释放索引并取消未开始的任务。快速打开目录加载 / 失败不误建笔记，删除恢复与另存为同步本地别名。全量 **395 项通过**（app 202、链接绘制集成 17、core 120、往返 5、index 50、反链计数集成 1），覆盖旧缓存重建、异步时序及原高亮 / 表格回归；范围与日志见 [补全](qa/m2-completion.md)、[行内链接 / 预览](qa/m2-inline-links.md)、[反链](qa/m2-backlinks.md)、[重命名](qa/m2-rename.md)。原生人工视觉、真实 IME 与大库性能未验收，未替换已安装应用。
- **进展（2026-09-20，行内格式）**：实现 `⌘B` 粗体、`⌘I` 斜体、`⌘E` 行内代码、`⌘⇧S` 删除线、`⌘⇧H` 黄色高亮，并接入编辑菜单及命令面板。有选区包裹并保留正文选区，再次执行取消；无选区插入成对标记、光标放中间，空标记可再次取消。格式操作独立撤销，连续正文输入另成一组；命令面板恢复原选区。核心计划用 AST 校验目标格式及块结构，支持中文、嵌套格式、Markdown 链接文字、表格转义竖线和 callout；代码 / 数学内部、URL、元数据、图片 / wikilink、跨行或改变结构的选区不改写，带属性的代码不剥离其承载元素。行内代码按内容选择足够长的反引号。新增 11 项测试，全量 320 项通过；属性保护收尾后定向回归见 [行内格式验收](qa/m2-inline-format.md)。其他颜色与“上次使用的颜色”仍待调色板功能。
- **进展（2026-09-20，自动配对）**：支持 `**`、`==`、反引号、`$`、`[[`、`(`、`"` 逐键配对；在自动生成的右侧输入同字符时跳过，空配对退格整体删除。配对插入 / 删除各为独立撤销组，跳过不修改文档；与 `[[` / `![[` 补全复用闭括号。仅观察聚焦正文的原生按键，匹配实际编辑结果后作最小修正，粘贴、选区替换、输入法预编辑 / 提交不额外补齐；代码、数学、链接地址、元数据与转义上下文有保护。仅跟踪当前编辑会话产生的配对，切换块、撤销 / 重做后恢复的标记按普通文本处理。新增 13 项测试，全量 309 项通过；验收及限制见 [自动配对记录](qa/m2-pairs.md)。
- **进展（2026-09-18，⌘K 行内链接）**：编辑器选中文字按 `⌘K` 包裹为 Markdown 链接并选中地址，剪贴板中的常见 URL 经校验和 Markdown 转义后自动填入，否则选中 `url` 占位。已有行内链接只选中地址，不改标签、标题或撤销栈；无选区插入 `[[]]` 并打开已有笔记补全，半成品 wikilink 直接重开菜单。支持编辑菜单、命令面板与焦点 / 选区恢复；中文、行内格式、表格转义竖线、callout、单步撤销与 IME 保护已覆盖。核心编辑计划经 Markdown AST 验证，新包裹保持块层级、类型、属性及 ID；代码内部、数学、图片 / 嵌入、引用式链接、自动链接、跨行或跨链接选区不改写。详见 [行内链接验收](qa/m2-inline-links.md)。
- **进展（2026-09-18，嵌入 / 附件补全）**：`![[` 混合列出笔记、图片与 PDF，按标题 / 文件名 / 路径模糊筛选，最近笔记及本篇附件优先；支持键盘 / 鼠标确认、单步撤销、表格与 IME 防护。标题和块嵌入复用已有候选与 ID 写入流程，同篇使用未保存内容。后台枚举进入 `.assets/`，遵守隐藏文件、`.gitignore` 与 `.aero/ignore` 规则，不跟随符号链接、不读取附件内容；确认前检查文件存在与 vault 边界。附件按本篇目录 → 相对路径 → 显式 vault 路径 → 唯一文件名解析，歧义时写限定路径；图片 / PDF 后缀保留给附件，同名笔记显式带 `.md`，重命名同步遵守同一规则。嵌入的 `|` 是尺寸等参数，`⇧⏎` 只插入嵌入、不生成显示别名。每次打开菜单读取快照，过期回复丢弃；未匹配时只插源码、不建文件。实际内容渲染、粘贴导入、附件引用计数与 PDF 页码补全未实现。验收见 [嵌入补全记录](qa/m2-embeds.md)。
- **进展（2026-09-18，块引用）**：`[[笔记#^` / `[[#^` 列出块摘要（前 60 个 Unicode 字符），支持按摘要 / 已有 ID 模糊筛选，键盘 / 鼠标确认与显示别名。选择前不写文件，确认时复用唯一 ID 或生成 6 位 base32 ID；段落行尾、其他块独立标记行，保留引用 / 列表上下文，并重解析验证结构和 ID 归属。外部目标两次校验指纹、原子写入单个标记，保留 BOM / 混合换行 / 其他字节，失败不插链接；同篇使用未保存文档，补 ID 与插链接组成一个撤销组。跨篇撤销保留已写 ID（6.3.3）。点击按显式 ID 定位并展开 callout，缺失 / 重复 ID 提示，不误用标题匹配；目标笔记缺失时不创建空笔记。写入后刷新索引、反链与文件树。验收见 [块引用记录](qa/m2-block-refs.md)。
- **进展（2026-09-17，标题链接补全）**：`[[笔记#` / `[[#` 按实际标题补全，支持中文模糊筛选、标题级别提示、键盘 / 鼠标确认、显示别名、表格源码和单步撤销。其他笔记在后台解析，当前笔记使用未保存版本；笔记名 / 标题 / 路径共用链接解析与消歧，片段经解析器校验后插入。标题提取统一供补全、大纲和跳转使用，识别 ATX / Setext、行内格式、引用式链接与嵌套标题；跳转到 callout 内标题时定位并展开其容器。同文本标题定位首处；无匹配 / 歧义 / 目标删除时保留输入。旧请求不能覆盖切换后的菜单，IME 预编辑不确认。验收见 [标题补全记录](qa/m2-headings.md)；`#^` 块引用后续已实现，见上。
- **进展（2026-09-17，标签补全）**：非行首、空格 / tab 后输入 `#` 打开标签菜单；候选按包含该标签的笔记数排序，同篇正文与 front matter 去重，不额外生成祖先标签，支持中文与分级标签模糊筛选、键盘 / 鼠标插入和单步撤销。复用索引的 Markdown 上下文规则，标题、代码、数学、链接锚点等不触发。SQLite 增加 `note_tags`（schema 4），标签与笔记指纹同事务更新；保存 / 改名 / 删除后维护目录。读取在后台执行，启动同步或索引不可用时从磁盘补齐，当前笔记用未保存版本替换并排除正输入的半成品。旧缓存按版本策略自动重建，笔记文件不变。验收见 [标签补全记录](qa/m2-tags.md)；侧栏标签树、全库标签改名、点击标签搜索仍未实现。
- **进展（2026-09-17，笔记链接补全）**：输入 `[[` 在光标处打开候选菜单，支持标题 / 路径模糊匹配、最近打开优先、键盘与鼠标确认、`⇧⏎` 插入并选中显示别名、无匹配时插入未解析链接。候选在后台读取，未索引文件从磁盘补标题；错误与加载态不当作空结果，过期请求丢弃。源码生成复用链接解析器验证目标，处理同名、相对路径遮蔽、中文、转义和表格别名；完整插入作为一个撤销组，输入法预编辑不确认菜单。段落、标题、列表、引用 / callout 和表格均可使用，代码 / 数学等非链接上下文不触发。front matter `aliases` 搜索仍待后续（标题、块引用、`#tag`、嵌入补全与 `⌘K` 后续实现见上）；验收见 [补全记录](qa/m2-completion.md)。
- **进展（2026-09-17，重命名同步链接）**：`aero-core::rename::RenamePlan` 从磁盘建立完整解析表，后台准备同目录文件重命名及入链改写，覆盖 Wikilink 和笔记嵌入；保留别名 / fragment / 原始字节，标题链接仍能解析时不改写，新名字有歧义时使用可验证的路径。UI 先保存，准备期间发生编辑则取消；超过 20 个其他文件先确认。逐文件校验、原件恢复副本、失败回滚已接入；附件目录目标冲突提前拒绝，笔记新路径 create-only 发布。成功后编辑器换路径并刷新自引用、文件树、搜索和反链；启动索引尚未结束时延后应用索引更新。恢复副本是 `.aero/` 下临时的事务数据，成功后删除，失败保留供恢复。该方案不宣称多文件崩溃原子性，跨目录移动、普通 Markdown 链接 / 显式附件路径文本、front matter 标题改名和全局撤销仍待实现。验收见 [重命名记录](qa/m2-rename.md)。
- **进展（2026-09-17，反链面板）**：右侧新增「反链 / 大纲」切换，按来源路径分组展示引用正文摘要、链接文字高亮，点击打开来源并定位到引用所在编辑块。复用 SQLite `links_target_idx` 查询，不改变存储格式；后台按来源读取一次文档，最多展示 200 条。初次索引完成及保存 / 重命名 / 删除 / 外部变化完成后自动刷新，切换或关闭笔记时取消旧查询；来源指纹变化或有未保存编辑时拒绝使用旧偏移。新增查询和 GPUI 点击回归，覆盖中文 / 别名 / CRLF / BOM、歧义、删除重建、外部修改、过期定位和连续切换。详细验收及限制见 [反链记录](qa/m2-backlinks.md)。未链接提及、块级引用筛选和富文本片段渲染尚未实现。
- wikilink/embed 解析与跳转、反链面板、未解析链接、重命名更新链接与附件目录。
- 块引用与 `^id` 按需写入、`#tag` 索引与标签面板。
- 9.9 渲染服务骨架（两级缓存、占位、失败回退）；图片粘贴（写入 `<note>.assets/`）与渲染、嵌入渲染、孤儿附件检测。
- 数学公式渲染（RaTeX → SVG → 位图，约 15 小时）。
- 属性列表解析/写回（6.3.10）、文字与块颜色、选区浮动工具条（约 20 小时；UI 见 `ui_design.md` 5.9 / 7.7）。
- CLI（`search / read / backlinks / append / new / index`）。
- 砍单清单：数学渲染（回退显示源码）→ 孤儿附件检测 → 颜色与浮动工具条 → 嵌入渲染 → 标签面板。

### M3 · AI 与富内容（约 135 小时，2027-09 初 → 2027-12 初）

- MCP Server（stdio）、审阅模式写入。
- 语义索引（zvec-rust）与混合检索（应用侧 RRF）、本地 Embedding；当前实现见 10.4，其他模型档位仍待评估。
- mermaid 渲染（merman，接入 GPUI 文本测量回调，约 15 小时）。
- PDF 嵌入（hayro，首页/指定页位图 + 翻页，约 20 小时）。
- 砍单清单：PDF 嵌入 → mermaid → 快速档评估 → 混合检索（先只做纯语义）→ 审阅模式（先只开只读工具）。

### M4 · 打磨与扩展（2028-01 起，按需）

- 链接卡片（OpenGraph 抓取 + 缓存 + 隐私开关，约 10 小时）。
- 编辑器后续：字符级跨块选区、表格增删行列；数学公式改为直接绘制 display list。语法标记隐藏、按需源码、表格单元格编辑已提前，见 9.1。
- 局部图谱、git 集成评估。
- L2 扩展层评估（`::::columns`、`query` 数据块）。
- Linux / Windows：仅在 M1–M3 未严重超期时启动，各平台 IME 单独验收。

---

## 19. 性能预算与基准

参考机：Apple M2 / 16 GB。基准 vault 由脚本生成：`bench-10k`（1 万篇、平均 3 KB、含中英混排与链接）与 `bench-100k`（10 万篇），另加作者自己的真实笔记库。

- 冷启动到窗口可交互：< 500 ms（索引加载不阻塞 UI）。
- 打开 1 MB 笔记到首帧：< 100 ms；10 MB 笔记：< 1 s，且期间可编辑。
- 输入延迟（按键到绘制）：p99 < 16 ms（一帧）。
- 全文搜索：`bench-100k` 上 p95 < 50 ms。
- 增量索引：单篇保存后 < 300 ms 内可搜索到（M0 实测 zvec 2 ms，余量留给解析与 SQLite 事务）。
- 全量重建：`bench-100k` 元数据 + FTS < 3 min（M0 从 10k 外推 zvec 部分 ≈ 3 min，卡在边缘，M3 用真实数据复测；重建期间峰值内存 < 300 MB，靠每 1 万 block flush 一次）。
- 常驻内存：`bench-10k` 打开 10 个文档 < 300 MB，其中索引引擎 ≈ 210 MB（zvec FTS 索引 ≈ 100 MB + 裁剪后的 jieba 词典 ≈ 110 MB）。注：原定索引引擎 < 150 MB 假设词典裁后 ≈45 MB，实测 cppjieba `DictTrie` 常驻远高于此（10.3），100k 词裁剪只能压到 ≈110 MB，此预算已按实测上修。
- 滚动：`bench` 中最长文档 60 fps 无卡顿。
- 重内容渲染（9.9，缓存未命中时）：行内公式 < 5 ms、块级公式 < 20 ms；典型 mermaid 图 < 200 ms；PDF 单页 < 1 s；均在后台线程，不计入输入延迟。缓存命中时与普通图片无差别。

预算写入 CI 基准，回归超过 20% 阻断合入。

### 19.1 性能优化实施计划（2026-10-08）

本轮先保持 Rope、Markdown 文件、现有 GPUI 控件和索引引擎，再改进编辑与文件更新的调度。参考本地 Zed 的 `text::Subscription` 编辑补丁、`language::Buffer::reparse` 单任务后台解析、`worktree::Snapshot` 路径增量扫描和 `fuzzy::match_strings` 候选预处理 / 协作取消。借鉴这些机制，不引入 Zed 的 CRDT 文本模型或整个 workspace。

代码对照发现的开销与实施顺序如下。静态分析只证明调用链与复杂度，不能当作 GUI 性能测量；本节状态随实际验证更新。

| 顺序 | 优先级 | 范围与实施方式 | 验收标准 | 状态 |
| --- | --- | --- | --- | --- |
| O1 | P0 | 去掉输入 / 重绘路径中的重复全文处理。字数按编辑范围精确增量维护，状态栏直接读取；嵌入扫描使用快照、后台合并与版本校验，保留 Markdown 上下文识别。文本控件首次解析直接使用最终扩展配置。后续尽量由同一次解析产出链接 / 嵌入信息。 | 字数 / 嵌入路径不在状态栏渲染或按键回调中物化全文 / 扫描嵌入；连续编辑、重载、切路径、删除与重新加入嵌入时旧回复不覆盖新状态；字数、嵌入内容与现有语义一致。 | 已交付（2026-10-08）；合并到主解析产物留作后续 |
| O2 | P1（先做低风险部分） | 将新建笔记、日记、附件操作等剩余同步目录扫描移到后台；需要立即定位的路径独立记录并在扫描完成后应用。文件操作保持顺序、错误反馈、create-only 与关闭保存屏障。 | 文件树刷新不在前台调用全库枚举；连续文件事件最终收敛；新建后定位、附件刷新、空目录、忽略规则、失败与切库回归通过。附件引用改写和文件读写均在后台执行。 | 已交付（2026-10-10）；有序队列、保存 / 关闭屏障与附件改名回滚 |
| O3 | P0 | 大文档全量回退解析改为单任务后台管线。保留安全的局部段落解析，以 256 KiB 为初始大小阈值并据测量调整；快照解析期间沿用漂移后的 Anchor 和聚焦控件，完成后按文档版本及会话代次应用，合并连续请求。Comrak 的整篇解析不能照搬 Tree-sitter 的限时增量接口。 | 大文档普通输入与语法输入不等待全量解析；每文档最多一个后台解析，过期结果作废；结构操作使用有效版本，BlockId、滚动、选区、IME、撤销与保存保持稳定。 | 已交付（2026-10-10）；256 KiB 阈值、64 KiB 局部解析上限与版本 / 会话校验 |
| O4 | P1 | 解析产出变化范围 / 结构与元数据变化信息；编辑单位、ID 索引、视口高度与样式缓存按依赖更新。未变结构沿用布局，后续块的位置通过 Anchor 或位移解析，不能保留过期的绝对区间。 | 普通文字修改不重建全部单位与视口；结构变化正确更新受影响范围；长列表插入 / 删除 / 拖排后无焦点或滚动跳变；必要时保留全量回退。 | 已交付（2026-10-10）；编辑补丁漂移、局部复用与结构比较；结构变化保留全量回退 |
| O5 | P1 | 建立带版本的共享 Vault 目录快照，文件树、快速打开、Wikilink 补全和 Resolver 共用；按保存、文件事件与改名增量更新，监听溢出 / 忽略规则变化做全量校准；目录可在 FTS 未就绪时独立使用。 | 普通打开补全 / 点击链接不重复全库扫描；磁盘与目录快照最终一致；标题、aliases、同名歧义、附件、删除重建与索引不可用回归通过。 | 已交付（2026-10-10）；独立于 FTS 的版本快照、路径增量刷新与共享附件目录 |
| O6 | P1 | 统一快速打开与 Wikilink 的匹配数据和筛选机制。预处理标题 / 路径 / aliases、小写字符与最近打开排名；Top-K 保留候选，大目录后台计算并在循环中协作取消。是否分段并行由测量决定。 | 大目录补全不前台全排序；新查询停止旧计算并拒绝旧结果；排序、同名解析、最近优先、键盘选择、Enter 确认与一次撤销语义保持。 | 已交付（2026-10-10）；共享预处理、Top-K、后台匹配与协作取消 |

配套验证与上述各步同时进行：

- 固定长段落、千项列表、大表格、含链接 / 嵌入的 1 / 10 MiB 文档，以及 10k / 100k 笔记库；隔离测试库与应用数据，不操作用户实际笔记。
- 分别记录解析、嵌入扫描、单位刷新、布局和实际输入到呈现耗时；CPU 微基准与真实窗口指标分别报告。先建立当前机器基线，再引入稳定的 20% 回归检查，不把历史数字当作当前版本实测。
- 每步运行相关核心与 GPUI 回归，阶段交付跑正式 crate 和本地 GPUI 补丁测试；验收脚本补入 `aero-embedding`。真实模型测试继续明确标为需要预先下载模型。
- 编辑相关变更用真实 debug 窗口检查点击前后布局、中文输入、长文档滚动与立即关闭保存；原生 IME 人工清单和参考机性能预算只有实际完成后才能标为通过。
- 先交付 O1 和 O2 的低风险部分，再推进 O3；O4、O5、O6 依序实施，每项独立记录结果与剩余限制。

**首批交付（2026-10-08）**：完成 O1 和 O2 的文件树重扫 / 命名去重部分。正式四个 crate 共 909 项通过，本地 GPUI 补丁 924 项通过；2 项真实模型测试保持跳过。真实窗口完成中文输入、撤销 / 重做、连续新建定位、立即关闭保存、段落点击无位移和嵌入外部更新验收。新增可复跑的 1 / 10 MiB 字数基准，具体数据与范围见 [本轮验收记录](qa/performance-o1-o2-2026-10-08.md)。附件引用改写、Finder 导入和其他剩余文件 I/O 仍需后台化；大文档回退解析、增量单位刷新和共享目录等 O3–O6 尚未实施。

**剩余开发交付（2026-10-10）**：完成 O2 的新建 / 日记、Finder 导入、附件改名与删除的后台执行和有序队列；附件改名复用校验、恢复副本与回滚。完成 O3 的单任务快照解析与过期结果保护，O4 的编辑区间漂移、局部单位 / 布局复用与结构比较，O5 的共享目录及增量元数据更新，O6 的笔记 / 标题 / 块补全预处理、Top-K 与协作取消。正式四个 crate 924 项通过，2 项真实模型测试跳过；GPUI 补丁 924 项通过。真实窗口验证 1.2 MB 文档输入 / 语法转换、千项嵌套列表点击无位移、别名快速打开与立即关闭保存。CPU 微基准中，100k 候选筛选中位耗时从 615.57 ms 降到 37.16 ms。实现、数据与限制见 [本轮验收记录](qa/performance-o2-o6-2026-10-10.md)。

本轮完成上述开发范围，仍保留明确的回退与验收边界：结构命令可同步校准解析；后台解析的块树装回与结构变化仍有全量整理成本；嵌入扫描暂时独立于主解析产物。原生 IME 人工矩阵、10k / 100k 磁盘库的端到端呈现延迟、GUI p99 / 内存预算及 20% 持续性能门禁尚未关闭，CPU 微基准不能替代这些指标。

---

## 20. 测试策略

- **往返属性测试**：CommonMark spec 用例、真实 vault 语料、随机编辑序列（proptest）。
- **解析器 fuzz**：`cargo-fuzz` 针对预扫描与后处理层。
- **渲染快照**：Block 树 → 渲染树的结构快照；GPUI 层用截图对比关键页面；公式与 mermaid 各维护一组固定用例的位图快照，渲染器升级时对比。
- **IME 手工清单**：按 9.6 逐项验收，记录在 `docs/qa/ime.md`。
- **真实窗口验收**：debug 构建设 `AERO_UI_CHANNEL` 时，应用从命令文件读取鼠标键盘事件并在窗口内派发（`crates/aero-app/src/ui_channel.rs`），窗口停在屏幕外、不抢焦点，`AERO_DATA_DIR` 隔离应用状态。脚本、样例库与用法做成 agent skill `.agents/skills/aero-ui-test/`：启动、点击 / 拖动 / 按键、截窗口图、逐像素比对与位移测量。用于探索和验收；确认的行为再写成无头 GPUI 测试（如 `editor::layout_tests`）进 CI。
- **索引一致性**：随机文件事件序列后，SQLite 与文件系统状态一致。
- **基准回归**：第 19 节指标在 CI 中按 nightly 运行。
- `aero-core` 全部 headless 可测，覆盖率目标 80%。

---

## 21. 风险清单

- **Block Editor 工期**：项目最大单体风险。缓解：块模型与 gpui-base 文本引擎已经撑住 IME 与往返；默认观感改为在同一模型上隐藏标记（9.1），不另起一个文档模型，也不做第二种编辑器。
- **GPUI API 变动**：pre-1.0。缓解：`gpui-kit` 精确锁版本、季度升级、UI 与 core 严格分层。
- **CJK 相关**：IME、分词、字体回退。缓解：进入 M0/M1 验收项而非事后修补。
- **往返无损被破坏**：某次解析器升级或新语法引入改变未触及字节。缓解：属性测试作为合入门禁。
- **单人业余投入**：任何一个模块失控都会拖垮整体节奏；缺少第二双眼睛审视设计。缓解：18.2 的节奏纪律、每个里程碑的砍单清单、M1b 后开源引入外部反馈、复用优先于自研。
- ~~两套索引引擎的维护成本~~：已消除，zvec 是唯一引擎（ADR 0003）；trait 隔离保留。
- **zvec-rust 成熟度与 FFI 分发**：绑定处于 0.7.x、社区小、随上游 C API 变动；M0 已实测发现 `delete_by_filter` 僵尸主键、rpath 不传递下游两个 bug，以及 git 依赖递归拉 730 MB submodule 的问题。缓解：保存路径只用 `upsert` + `delete(pk)`；`aero-app` 自管 rpath；离线构建与 bundle 链路已跑通并脚本化；向上游提 issue（僵尸主键、rpath、crates.io 发版）；性能与内存有一个数量级余量，即使上游停更也可锁版本长期使用。
- **无移动端影响采用率**：缓解：产品层面坦白；文件是纯 Markdown，任何手机编辑器都能读写。
- **GPUI 缺少 Web 生态**：没有浏览器内核可借用。缓解：数学、mermaid、PDF 分别用纯 Rust 库（RaTeX / merman / hayro）原生渲染；网页能力收缩为链接卡片，不引入 WebView。
- **渲染库均为 0.x 或 alpha**：RaTeX 0.1.x 基本单人维护，merman 0.8 alpha，hayro 0.7 且性能未优化。缓解：各自藏在 trait 后面；渲染失败一律回退显示源码，不影响编辑与数据；位图快照测试守住升级；备选路线（typst+mitex、mermaid-rs-renderer、pdfium-render）已列出。

---

## 22. 决策记录与待决问题

### 已决策

产品与范围（2026-09-14 拍板）：

1. 格式路线：扩展 Markdown，分 L0/L1/L2 三层；L1 借鉴社区惯例但**不以 Obsidian 兼容为目标**；L2 必须可降级且整体不进 v1。
2. 编辑器形态：块级编辑，Markdown 为唯一存储；跨块选区为块级。默认观感见第 20 条（2026-09-22 修订，不再把标记隐藏留到 M4）。
3. vault 内配置目录命名 `.aero/`，只放配置；索引放应用数据目录。
4. 附件默认每篇笔记一个 `<note>.assets/` 子目录，随笔记移动。
5. front matter 只在新建时写 `created`，不自动维护 `updated`。
6. v1 AI 能力只经由 MCP 与 CLI，不做应用内 AI。
7. 零远程遥测；有上限的诊断日志与崩溃报告仅本地生成，用户手动导出 / 提交（16.1）。
8. Apache-2.0，M1b 结束后公开仓库。
9. macOS 首发，最低 macOS 15；Linux/Windows 视进度。
10. 单人业余投入，按 18 节排期，每个里程碑带砍单清单。

架构（同日）：

11. Block ID 与 Note ID 均按需生成，不批量注入。
12. 一份 Rope（ropey 2.0，与 gpui-base 同型）+ Block 树持有 range（marker 表 Anchor）；整篇重解析 + 树 diff，不做增量解析；> 256 KB 的文档解析走后台快照。
13. v1 不做同步引擎；外部修改三方合并，绝不静默覆盖。
14. GPUI executor 为主，Tokio 限定在 MCP/网络边界。
15. 不实现 toggle 语法，用可折叠 callout 承担。
16. 索引引擎 = zvec-rust（全文 + 向量合一，ADR 0003）；保存路径 `upsert` + `delete(pk)`，禁用 `delete_by_filter`；jieba 词典裁到 ~100k 词随应用分发；Embedding 由应用负责（fastembed）。
17. 不复用 zvec-grep 作为组件，只借鉴其检索管线与 MCP 设计；vault 允许被第三方工具并行索引。
18. 富内容全部原生渲染、不引入 WebView：数学用 RaTeX（M2），mermaid 用 merman（M3），PDF 页面预览用 hayro（M3）；网页能力只做链接卡片（M4），不做剪藏与实时网页视图。所有渲染器经 9.9 的统一渲染服务与 trait 隔离。
19. 文字/块颜色用 Pandoc 与 markdown-it-attrs 共有的属性列表 `{.red}` / `{.bg-blue}` 承载（6.3.10），9 色调色板与 Notion 对齐；`==` 保留为黄色背景的专用写法。不用内联 HTML `style`。M2 交付。
20. 默认编辑观感（2026-09-22）：一个编辑器、两种呈现。聚焦块隐藏语法标记；选区工具条只含能写回 Markdown 的格式，不含字体族、字号、字间距、段距。源码按块或按篇打开，另有「聚焦时显示源码」偏好，等于现行 Live Preview。不做应用级纯 Markdown 模式。数学仍编辑源码；表格按单元格编辑。操作都写回 Rope，模型不变。细则 9.1。字符级跨块选区与表格增删行列不在此列。2026-09-23 补充硬性要求：用户不感知 Markdown 语法，浏览与编辑同一副样子、点进任何一块零跳变（字号字重不变）、任何一块按下即可拖选、表格列宽可拖且点击打字不变宽、callout 与任务方框不跳变；做不到的内容保持预览 + 源码编辑，不半藏半露（9.1）。
21. 标签页（2026-09-23）：每篇文档一个标签页，打开已打开的笔记就切过去，不做 Obsidian 默认的「在当前页里换笔记」；只有搜索 ↑↓ 预览共用一个预览页，避免方向键刷出一排标签。没显示的页保留编辑器、照常保存，切走时立刻落盘；收藏与打开的标签页按 vault_id 记在应用数据目录，不写入 vault（与面板状态一致，避免同步冲突）。

### 待决（均由 spike 数据决定，不需要额外拍板）

1. ~~解析器 comrak vs markdown-rs —— M0，限时一周。~~ 已定 comrak（ADR 0002）。
2. ~~Rope：Zed `rope`/`sum_tree` vs `ropey` + marker —— M0。~~ 已定 `ropey 2.0` + marker 表（ADR 0004）；Zed `rope` 不在 crates.io 且 anchor 不在其中，不再考虑。
3. ~~zvec 是否达标（查询延迟、写后可见延迟、打包链路、静态链接）—— M0。~~ 已达标（ADR 0003）；静态链接放弃。
4. 快速档 Embedding（model2vec 静态模型）的中文检索质量 —— M3，仅评估。

---

## 23. 块交互体验技术方案（2026-10-10）

需求基线：[交互体验需求及已定决策](edit_experience/edit_experience_request.md)。本节覆盖该文档 A01–A37；实施状态单独记录，不把技术设计当作已交付。该需求补充／更新第 6、7、9、12、18、22 节中关于预设颜色、原子容器编辑、笔记身份与扩展语法范围的旧约束；内容真相仍为 Markdown／Rope。

### 23.1 分层与数据流

```text
手柄 / 菜单 / 键盘 / 拖拽 / 剪贴板
    → 稳定操作目标（块身份、源码范围、修订）
    → 纯操作计划（编辑、身份映射、选区、落点）
    → 单文档 Document 编辑组 / 工作区关联事务
    → Rope → 块树 → 可见正文编辑单位 → 共用字形布局
    → 保存及内存源修订通知 → 可重建身份目录 / 同步引用 / 文档树
```

- `aero-core` 负责格式、身份、块操作计划、事务持久化与恢复，不依赖 GPUI。操作计划以源码区间修改，必须保留未操作内容；整树仅用作定位与校验，不把 AST 全篇重新打印。
- `aero-index` 以文件内 `aero.id` 为正式 note ID；路径、全局块 ID、别名和父子关系均为可重建派生表。旧文件无正式 ID 时保持现有缓存身份，首次需要时才迁移。
- `aero-app` 保持当前编辑器和 Textarea presenter；增加容器视图树、统一块命令、工作区事务协调、内存源订阅、逻辑文档树，不为高亮内部维护另一份编辑文本。
- `forks/gpui-base` 只承载实际必要的字形排版、取色输入与 IME 命中改造；应用语法和文档关系不能进入底座。

### 23.2 文件格式、身份与无损写回

1. 新增 `identity` 模块：识别／生成 `d-ULID`、`b-ULID`；读取和局部更新 front matter 的 `aero.id/parent/order`，拒绝重复键和非映射命名空间，保留未知 YAML、注释、BOM、行尾。已有缓存 note ID 与正式 ID 通过一次索引重建衔接。
2. 扩展 `block_refs`：主 ID 与 `aero-aliases` 共同解析；旧短 ID 升级时携带原文档作用域。全库目录记录 `主ID/别名 → 文件+块`，重复定义返回歧义，不任取一条。新 `id:d-…` Wikilink 由 Resolver 识别；块移动后允许按唯一全局块身份定位。
3. 新增共用块属性写回原语，支持叶块尾标记、代码／容器外部元数据行及 `aero-owner=item` 的列表项元数据；修改某个键或颜色通道时保留其他 token。容器首行负责外观、结束后负责身份。
4. 扩展颜色读取／写回：预设类兼容；自定义 HEX／RGBA 独立解析，源表示保留精度。相同通道自定义优先；恢复默认清理该通道已知表示，不删身份、别名或未知键。
5. 解析后将项头元数据归入 ListItem 并隐藏该行。正文编辑单位递归到叶子；另用 `ContainerPath` 表达引用／directive／列表的源前缀、围栏和边界，避免把前缀暴露为普通可删除字符。

### 23.3 操作计划与剪贴板

- 命令统一携带 `Target { block, revision, range }`；执行前重新校验目标身份和结构。菜单与取色器保存打开时的目标及文本选区，失效则提示，不退回当前焦点。
- 新增纯函数块操作层：转换、包裹／解除、移动、复制、删除、样式、拆分／合并；每个计划包含有序 Edit、焦点位置和身份映射。所有编辑先在快照上验证解析树，非法落点、身份冲突或无损条件不满足时不执行。
- 块操作范围来自树节点，正文输入范围来自叶编辑单位，两者不能混用。选中祖先时归并后代；列表项和列表整体操作显式区分。
- 复制输出完整源码及 Aero 内部结构元数据；粘贴按源身份映射重建副本 ID，内部引用同步重指，历史别名不继承。剪切携带会话、库和单次移动令牌；系统剪贴板文本始终可在其他应用读取。
- 拆分身份跟随原正文前半；段首插入空块时原身份留在原正文。`inline_format::split_at` 按解析到的文字范围为跨光标的强调、代码、链接和颜色包裹分别闭合／重开，未涉及的源码原样保留。列表拆分保留祖先前缀，原有续段和子列表留在原项。合并先规范旧身份，再将后块主 ID 并入前块别名；同父且块样式一致才自动合并，否则移动光标并提示原因。有关身份和样式写回进入同一个 Document 撤销组。
- 尚无正文节点的空段落在编辑器会话的 `EmptySlot { range, attrs }` 中保存显式样式；范围随所有正文编辑移动，失焦后仍渲染可点击占位，可同时保留多个不同样式的空段落。首次提交将正文与属性写入同一个撤销组，组合输入期间不物化；首次多行粘贴只解析本次输入，分别给生成的文字块补样式，粘贴中显式属性按通道覆盖继承值，不继承身份／别名。普通单行首次提交保留原 Textarea 输入客户端及后续输入合并路径。保留物化后的范围以便逆向编辑恢复原空占位样式，不能被粘贴末段的覆盖样式污染；撤销创建后不在既有段落上生成幽灵空行。该状态仅在当前文档会话内保留，关闭／外部重新装载时清除，不为无正文的样式写入空 Markdown 语法。历史落点位于隐藏属性／ID 时定位回所属正文。
- 类型转换的菜单、快捷键与斜杠入口共用 `conversion_reason` 校验。列表项首段操作可定位整项，续段定位当前叶块；含多个子块或独立项元数据的目标，不能通过删标记强制降成单个正文块。不可执行的斜杠命令保留原输入。

### 23.4 工作区事务与恢复

- 新增核心 `transaction`：`FileChange { path, expected, before, after }` 与事务阶段（prepared / committing / committed / rolling_back）。输入为实际字节快照，创建、替换、删除使用相同校验入口；备份镜像以二进制文件保存，不把附件或整篇正文塞入大型 JSON 数组。
- 日志位于应用数据目录的库级 transactions 子目录，不能进入用户笔记正文或作为文件内容的唯一来源。先落盘 manifest 和镜像并 fsync，再逐文件验证及原子替换；完成后写提交标记。重启读日志，未提交事务回滚，已提交事务核验后清理。
- 每次写入与回滚均复核预期指纹，提交标记落盘前再核验全部已写参与者及只读观察者；外部版本不同则保存恢复镜像并报告冲突，不能用备份覆盖新文字。事务不宣称跨多个文件同时原子，依靠日志达到可恢复。
- 粘贴结果基于编辑器接受命令时的字节快照生成；后台全库扫描再次读到的目标版本必须与它相同。扫描期间出现新版本即拒绝粘贴，不能把新版本用作事务的预期值，却仍写入由旧编辑器快照生成的结果。扫描之后的变化继续由提交前及逐文件版本校验拦截。
- 应用层协调器收集所有打开文档缓冲区，冻结相关自动保存／结构命令，后台准备和提交，结束后统一装回修订、更新目录与引用并释放屏障。读取快照不能遗漏尚未保存的内容。
- 各参与文档的撤销入口引用同一个事务 ID；先验证所有相关历史是否允许撤销，再整体执行反向计划。存在后续编辑时引导先撤销对应文档的后续编辑，不静默撤销或选择性覆盖它们。
- 成功事务若删除了某个参与文件（如撤销新建子文档），该编辑器向所有订阅工作区请求关闭标签页，不保存已删除内容；父文档继续保留共同事务的重做入口。只有预期文件仍存在却无法装载，或真正的外部删除，才保留删除横幅供用户处理。
- 事务删除的参与者立即退出活动来源注册表，不能因后台任务暂时持有已关闭编辑器而阻塞后续撤销／文件操作。完成磁盘工作后，文件守卫与库级屏障延续到标签页关闭事件处理完毕，再启动已接受的下一操作；避免旧编辑器使队列被误取消，或在下一操作冻结界面时才关闭旧页而破坏活动标签索引。
- 剪切到首次粘贴前用锚点维护恢复位置；粘贴成功后剪切记录归入共同事务，不能单独恢复源副本。`Document::without_transaction` 将剪切后的不相交编辑重放到恢复版本并重建撤销历史，发生重叠则拒绝自动恢复。事务日志另存恢复镜像，因此首次磁盘写入前退出也能恢复。失败／主动取消时恢复源、取消剪切记录并废弃令牌。恢复冲突时冻结关联写入，保留所有版本。
- 文件资源不随块移动搬迁；沿用现有附件与 Markdown 路径扫描重定向地址。扫描与文件版本校验在后台完成；确认受影响文档不能读取或写入即失败，不跳过后宣告成功。

### 23.5 容器视图、手柄与拖拽

- 在叶编辑单位外建立轻量容器展示树，递归渲染高亮、引用与列表；每个叶子仍用当前 presenter。容器负责背景／边框／布局边界，叶子负责文本。视口高度缓存按子树修订失效，不因悬停重建整篇。
- 手柄使用固定预留的外缘操作区和绝对定位；按容器真实内边距／边框和列表标记列计算外伸宽度，结构变化时缓存，不把每个容器祖先误当作一个列表层级。文字类型标记与六点按钮共用稳定命中区；不活动的嵌套按钮隐藏且不参与命中，鼠标从容器移入外缘操作区时保持原目标，不被子段落的外伸区域抢占；离开操作区清除悬停，菜单打开期间保持打开时目标。活动高亮不参与布局。
- 列表整体在现有编辑器外缘使用额外 24 pt 的窄列，六点与类型按钮纵向排列；项与正文保留内侧操作区，分别从列表标记列和实际正文命中。叶块的外伸区域不能截走父级边框／标记列的悬停。长文档按列表项虚拟化时，仅在首项挂载列表整体入口，其余可见项共享整体高亮，不为手柄挂载完整列表或改正文位置；菜单计划仍使用完整列表节点。
- 拖动以平台阈值区分点击，落点记录 `Before/After/Into(container)`，从容器树命中层级；预览只改 UI 状态，松手才提交纯移动计划。拒绝自己及后代，Escape 丢弃计划，不增加撤销项。
- 容器空占位、末尾空段落 Enter 退出一层、首部 Backspace 选边界、跨块删除均进入统一结构编辑路径。字符选区不跨容器源码边界。
- 有 ID／别名的容器禁止真正解除；透明背景／无色边框仍可设置。无身份容器解除时提升子块，保留各自标识与样式。

### 23.6 样式、取色与四种对齐

- 取色状态持有目标快照、初始颜色和预览颜色。预览只进入渲染覆盖层；确认生成一次属性编辑，Escape／取消直接去覆盖层。自定义输入支持 HEX、RGBA 及透明度，非法输入显示原因并禁用确认。
- 相同样式计算同时供浏览与输入使用；容器不向子块继承文字样式，列表项仅为直接正文提供默认值，显式叶样式优先。
- 视觉缩进作为块布局属性，0–8 级、24 pt／级，最窄文字区 160 pt。列表层级命令与视觉缩进分开。
- 左／中／右对齐复用现有 Textarea 对齐能力；两端对齐新增行内扩展机会与 glyph offset 映射。末行、软换行结束行不扩展；绘制、鼠标 index、选区矩形、光标、链接和 IME 候选都读同一偏移表，不以外部容器拉伸假装完成。
- 底座布局缓存键包含正文修订、字体、宽度、对齐和可见标记配置；组合输入保留当前控件与标记区间。代码、表格整体、数学、图片及同步引用不开放正文对齐。

### 23.7 同步引用与逻辑文档树

- 同步插入复用笔记／块选择器，增加“复制同步引用”“更换来源”动作。目标插入位置保存为锚点；中途插入的拆分与引用形成一组撤销。跨篇补来源 ID 可独立留下，防止插入撤销破坏其他引用。
- 工作区维护内存源修订表，编辑／撤销／重做时通知依赖引用；已打开缓冲区优先于磁盘。相同源修订的解析共享，后台请求携带库代次、源版本和目标请求号，过期结果丢弃。
- 文件删除／恢复也是来源修订变化：使共享磁盘快照失效并通知订阅者。已删除编辑器保留恢复缓冲，但不作为有效的同步正文覆盖层；直接引用即时显示来源不可用，嵌套引用的后台快照同样排除该缓冲。恢复相同字节也必须通知订阅者，不能只依赖正文版本变化；恢复新字节按现有外部重载路径刷新。
- 磁盘源缓存与底座 Markdown AST 缓存各自采用 128 项／32 MiB 源字节的 LRU 上限，同修订并发请求用 `Arc<OnceLock<…>>` 共享读取／解析。文件事件使磁盘源失效。此上限限制缓存持有的源数据，不能当作 AST、布局或进程 RSS 的 32 MiB 上限；进程内存仍须按 P1–P3 对照实测。
- 引用展开上下文记录身份链与跳数，循环及 3 跳上限各显示占位；失效、歧义、读取失败有独立状态。引用外层只读来源框，不另存正文或允许样式覆盖。
- 逻辑树由 `aero.id/parent/order` 生成，环、重复 ID、缺父分别显示问题分组。侧栏文档／文件视图共享实际文档实体，拖动分别操作关系或路径；关系索引可彻底重建。
- 逻辑树扫描快照预先生成展示行及父级／下一同级映射。拖动只处理鼠标所在行，上／下边缘显示同级插入线，中间显示改挂框；预览绝对定位，不改变行高。离开目标或 Escape 清除预览，松手使用最后有效落点；找下一同级时跳过目标的全部后代，不逐次悬停重建／排序整棵树。
- 子文档创建复用工作区事务：create-only 文件、元数据、父文档链接共同提交，随后打开／定位。同级 order 采用 1024 间隔，重排没有间隙时共同重编号，不用磁盘路径表达父关系。
- 系统表情入口先删除命令并恢复插入点，经过一次完整绘制、安装新的 textarea 输入处理器后，在第二个帧边界调用既有字符面板；通过现有 Unicode 输入与撤销路径插入，不手工截断字素。原生面板实际弹出及选取仍是独立验收项。

### 23.8 实施与验收跟踪

| 阶段 | 代码范围 | 必要验证 | 状态 |
| --- | --- | --- | --- |
| E1 文件及身份基础 | core 属性／颜色、identity、block_refs、解析归属、Resolver、index | A20、A23、A33–A35；旧文件不主动迁移、别名与元数据往返、重复身份拒绝 | 已接入，自动回归通过 |
| E2 事务与剪贴板 | core transaction、workspace 协调、editor 剪贴板 | A14–A15、A25–A26；只读、外部修改、逐文件失败、崩溃恢复和重复粘贴 | 已接入；自动故障回归通过，真实窗口已补准备期间取消、只读目标、失败后重复粘贴与外部目标变化恢复；修复扫描期间旧粘贴结果覆盖新目标版本的竞态，以及撤销后的队列收尾。完整组合矩阵待复核 |
| E3 容器与块操作 | units、ops、presenter、block_menu、viewport | A01–A08、A22、A24、A27–A28、A36；无头 GPUI + 真实窗口坐标比较 | 已接入；格式拆分、空占位、容器拖拽、列表整体／单项／正文目标及删除撤销子集通过；修复虚拟列表整体入口和嵌套悬停争抢，完整交互矩阵待补齐 |
| E4 样式与排版 | colors_ui、样式布局、gpui-base 行映射 | A09–A13、A34、A37；RGBA 精度、预览取消、四种对齐命中及 IME | 已接入；取色、嵌套正文双主题零跳变及 WeType 部分原生场景通过，完整输入法／主题矩阵待补齐 |
| E5 引用与文档组织 | embeds、catalog、file_tree、workspace/files | A16–A19、A29–A32、A35；内存源订阅、循环、逻辑树重建和创建事务 | 已接入；子文档、同级排序子集及直接／嵌套同步源删除、同字节／新版本恢复的双主题子集通过；修复删除来源仍显示旧缓冲的问题。原生表情面板、完整组织矩阵与共享来源压力验收待完成 |
| E6 集成验收 | 正式 crate、gpui-base、aero-ui-test、性能夹具 | 全部 A01–A37 与 P1–P3；真实 IME 和 GUI 性能另按实测记录 | 进行中；见下方 QA 记录，未达到整项交付门槛 |

实施记录更新于 2026-10-13。最新完整自动回归为 **1,893 项通过、0 失败、2 项显式忽略**，源码与构建校验、真实窗口截图、零位移像素比较及原生输入法子集见 [本轮 QA](qa/edit-experience-2026-10-12.md)。P1–P3 的 release 窗口延迟、帧间隔、引用传播及内存基线尚未采样；本次 100 篇观察文档的准备／恢复夹具及现有 CPU 微基准均不能关闭这些门槛。

每个阶段的实现与未完成部分必须在本表及 QA 记录中同步。只有实际通过相应验证才能写“已交付”；不得用 core 纯函数存在代替 UI 接入，也不得用编译成功代替功能／真实窗口验收。
