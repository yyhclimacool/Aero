# ADR 0002 · Markdown 解析器：comrak

- 状态：**已采纳**（2026-09-15，spike 半天完成，原限时一周）
- 关联：`project_plan.md` 8.2；`spikes/md-parser/`（可重跑：`cargo run -p spike-md-parser --release -- <笔记目录>`）

## 问题

Aero 的文档模型是"一份 Rope + Block 树持有字节区间"，每次编辑整篇重解析（8.1）。解析器必须满足：

1. 块级与行内级节点都有**字节精确**的源位置，切片即原文；
2. 覆盖 L0/L1 语法（front matter、表格、脚注、数学、wikilink、callout、高亮），能扩展 L2；
3. 整篇重解析在每次按键预算内（100 KB 级笔记 < 2 ms，1 MB < 20 ms）。

候选：comrak 0.55.0（2026-09-06）与 markdown-rs 1.0.0（2025-04-23）。

## 语料

- CommonMark spec 0.31.2 全部 655 个用例；
- `corpus/aero-sample.md`：覆盖 6.3 / 6.4 全部 L1/L2 语法的样本；
- 45 篇真实笔记（`~/code/wlog`，1.1 MB，含中文表格、公式、front matter）；
- 合成 100 KiB / 1 MiB 文档（样本重复拼接）。

## 结果（release，Apple M5 Pro）

### 位置精度

| 指标 | comrak | markdown-rs |
| --- | --- | --- |
| 节点有位置 | 100% | 100% |
| 位置落在 char 边界内（真实笔记 27k 节点） | 99.73% | 100% |
| Text/Code 切片 = literal（spec / 真实笔记） | 96.8% / 94.7% | 95.0% / 96.8% |
| 无法解释的不一致 | spec 8 例、真实笔记 197 例 | 0 |

comrak 的不一致全部归结为两个**具体缺陷**：

- **缩进的表格行**（`  | a | b |`）：单元格与其中文本的位置整体左移缩进宽度；`Table` / `TableRow` 区间正确。真实笔记里 197 例全部来自这一种情况。
- **链接引用定义之后的同段文本**（`[foo]: /url` 紧接正文）：定义被消费后，后续 Text 位置错位。spec 5 例。

其余差异是字面值归一化（代码段内换行→空格、实体解码），不是位置问题。

### 结构

| 现象 | comrak | markdown-rs |
| --- | --- | --- |
| 顶层块顺序 = 源顺序 | 否：`FootnoteDefinition` 被移到 AST 末尾 | 是 |
| 链接引用定义 | 无节点（71 个 spec 用例出现"未覆盖"内容） | `Definition` 节点 |
| `$$` 块公式 | `Paragraph` 内的 `Math{display_math: true}` | 独立 `Math` 块 |
| `::::` / `:::` 嵌套容器 | 按栏数区分，嵌套正确 | 不支持 |

### 扩展语法覆盖

| 语法 | comrak | markdown-rs |
| --- | --- | --- |
| front matter / 表格 / 任务 / 删除线 / 脚注 / `$` `$$` / 自动链接 | 原生 | 原生 |
| `[[wikilink]]`、`[[a\|b]]` | 原生（`WikiLink`） | 无 |
| `==高亮==` | 原生（`Highlight`） | 无 |
| `> [!note]` callout | 原生（`Alert`，GitHub 风格） | 无 |
| `:::name` 块级容器 | 原生（`BlockDirective`） | 无 |
| `{.class key=val}` 属性：标题、代码段、链接/图片、围栏代码 | 原生（`attributes` feature） | 无 |
| CJK 相邻的 `**强调**` | 原生（`cjk_friendly_emphasis`） | 无 |
| 段尾 `^id`、`#tag`、`%%注释%%`、`[文本]{.red}`、段尾 `{.bg-blue}`、`:inline[]{}` | 无 | 无 |

### 速度（每文档取最小值）

| 语料 | comrak | markdown-rs |
| --- | --- | --- |
| 655 个 spec 用例累计 | 0.44 ms | 2.91 ms |
| 45 篇真实笔记累计（1.1 MB） | 5.0 ms（223 MB/s） | 50 ms（22 MB/s） |
| 合成 100 KiB | 1.3 ms | 20.2 ms |
| 合成 1 MiB | 18.3 ms | **1.13 s** |

markdown-rs 在 1 MiB 上呈超线性（100 KiB → 1 MiB 耗时增长 56 倍），无法满足整篇重解析的预算；comrak 线性且余量充足。

## 决定

**采用 comrak**（`default-features = false, features = ["attributes"]`）。理由按权重：

1. 速度差 10–60 倍，且 markdown-rs 在大文件上超线性，会迫使我们放弃"整篇重解析"、转向增量解析（8.1 明确不想做）。
2. wikilink / 高亮 / callout / `:::` 容器 / 属性列表 / CJK 强调全部原生，L1 与 L2 的自研解析量大幅缩小。
3. 上游活跃（本月有版本），markdown-rs 最近发布是 17 个月前。

markdown-rs 位置精度更好，但差距只体现在两个可绕过的缺陷上，不足以抵消 1、2。

## aero-core 需要承担的补偿（写入 8.2 的后处理层）

- **顶层块按区间排序**，把 `FootnoteDefinition` 放回源位置。
- **位置校验**：对每个节点校验 `start <= end <= len` 且落在 char 边界；不通过的节点（目前只有缩进表格的单元格）标记为"位置不可信"，由父节点区间内的二次扫描重建。v1 表格只读渲染，只需 `Table` 区间正确。
- **链接引用定义**：预扫描识别 `[label]: url` 行，作为 `Raw` 块保留区间；不依赖 comrak。
- **块公式**：仅含一个 `display_math` 节点的段落提升为数学块。
- **仍由后处理实现**：段尾 `^id`、`#tag`、`%%`、`[文本]{.red}`、段尾 `{.bg-blue}`、行内 directive。
- 向 comrak 提两个 issue（缩进表格行位置、引用定义后文本位置），附最小复现。

### 实现 aero-core 骨架时追加发现的块级怪癖（2026-09-15，均已在 `aero-core::parse` 补偿）

- `NodeValue::FrontMatter` 的 `block()` 返回 `false`，按块节点过滤时要单独放行。
- 列表最后一项的 sourcepos 终点是 `N:0`，N 是列表之后被吞掉的**最后一个**空行（多个空行时不是第一个），意思是"到前面最后一个非空行的行尾"。处理：回退到 N 行之前并去掉尾部空白。
- `alerts` 扩展只认 GitHub 的 5 种类型，标题会把折叠标记 `-`/`+` 一起吞进去。已关闭该扩展，callout 由 aero-core 在 BlockQuote 首行统一识别，Aero 的 8 种类型与折叠标记一并处理。
- `block_directive` 与列表项相遇（如 `- :::`）时，列表的终点会算到后面 directive 的开启行，兄弟区间重叠。处理：兄弟按起点排序后，前者终点 > 后者起点即截断前者（起点比终点可靠）。
- 列表项内段落末尾的空格计入段落终点、不计入列表项终点，产生 1 字节的父子越界（真实笔记里出现 2 例）。夹回即可，无害。
- 结论不变：这些都是可在后处理层用常数代价修正的位置问题，不影响选型。

### 往返测试框架的覆盖（`crates/aero-core/tests/roundtrip.rs`）

CommonMark spec 655 例 + 样本 + 合成文档（4 KB–512 KB）上：`Document(x).to_string() == x`、块区间结构不变量（越界 / char 边界 / 父子包含 / 兄弟不重叠 / 顶层间隙为空白）、块级零夹回。随机编辑序列（proptest，每次 1–25 步）上：与 `String` 参考实现一致、未触碰块文本不变、重解析后不变量成立、未触碰且仍在原位的块 id 不变、撤销后仍不变。

## 对方案的修正

- 8.2：解析器定为 comrak；预扫描不再需要处理 `:::` 容器（comrak 原生）；后处理清单按上文更新。
- 6.3.10：标题 / 代码段 / 链接的 `{...}` 属性由 comrak 原生解析；`[文本]{.red}` 与段尾属性仍由 aero-core 处理。
- 6.4.1 的示例中外层容器必须用更多冒号（`::::columns` 包 `:::column`），与 comrak 的栏数匹配规则一致——spike 里用同长度冒号嵌套确认会解析错误。
