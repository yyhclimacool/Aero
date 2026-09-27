# aero-core

Aero 的 headless 核心。当前 GUI 与 CLI 共用它，MCP 客户端接入仍在规划中。不依赖 GPUI、Tokio 或具体索引引擎。模块清单按 2026-09-26 源码更新。

| 模块 | 内容 | 方案章节 |
| --- | --- | --- |
| `text` | `Rope`（ropey 2.0，字节索引）、`Edit`、文件表层特征（BOM / CRLF）的解码与还原 | 7.2、5.5 |
| `anchors` | `MarkerTable`：随编辑漂移的 `Anchor`，块区间 = Left 起点 + Right 终点 | 7.2，ADR 0004 |
| `block` | `Block` / `BlockKind` / `Attrs` / `BlockTree` | 7.1 |
| `parse` | comrak + 后处理 → 带字节区间的块树，附 `ParseReport` | 8.2，ADR 0002 |
| `document` | `Document`：编辑、undo / redo、快照与版本校验、段落局部解析及整篇回退、BlockId 身份匹配 | 7.3、7.4、8.1 |
| `index` | `FullTextIndex` / `VectorIndex` trait、内存实现、块正文抽取与标签查询 | 10.3、10.4，ADR 0003 |
| `vault` | `VaultPath`、`Vault`：枚举笔记、读取、原子写、`.aero/vault.toml` | 第 5 节 |
| `links`、`headings`、`block_refs` | 笔记 / 标题 / 块链接解析、别名、片段定位与持久块 ID | 6.3、11 |
| `attachments`、`rename` | 附件发现、导入与引用计数；移动 / 改名的引用改写、校验、备份与回滚 | 5.4、11 |
| `inline_format`、`inline_links`、`colors` | 行内格式、链接和颜色的源码编辑计划 | 6.3、9.1 |
| `highlight`、`markup` | 预览 / HTML 导出、标记隐藏与行内公式提取 | 9.1、13.1 |
| `tags`、`rich_content` | 正文标签、Mermaid 源码、PDF 嵌入与页码参数 | 6.3、9.9 |

上表只覆盖核心层：全文 / 向量存储在 `aero-index`，模型下载与推理在 `aero-embedding`，图片 / 公式 / Mermaid / PDF 渲染和界面交互在 `aero-app`。整体关系见 [技术方案](../../docs/project_plan.md#32-crate-分层)。

## 测试

```sh
cargo test -p aero-core                               # 单元 + 往返测试
AERO_CORPUS_DIR=~/notes cargo test -p aero-core --test roundtrip external -- --nocapture
PROPTEST_CASES=1000 cargo test -p aero-core --test roundtrip random
```

`tests/roundtrip.rs` 是 8.3 的往返属性测试：CommonMark spec 全部用例、`tests/corpus/*.md`、合成文档、随机编辑序列。

## 调试

```sh
cargo run -p aero-core --example dump -- note.md          # 打印块树与后处理报告
cargo run -p aero-core --example replay -- base.md d:0.5:1 "i:0.9:- "   # 回放 proptest 的最小失败输入
```
