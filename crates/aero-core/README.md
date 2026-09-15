# aero-core

Aero 的 headless 核心。GUI / CLI / MCP 都是它的客户端，没有任何一方有特权路径。不依赖 GPUI、Tokio 或具体索引引擎。

| 模块 | 内容 | 方案章节 |
| --- | --- | --- |
| `text` | `Rope`（ropey 2.0，字节索引）、`Edit`、文件表层特征（BOM / CRLF）的解码与还原 | 7.2、5.5 |
| `anchors` | `MarkerTable`：随编辑漂移的 `Anchor`，块区间 = Left 起点 + Right 终点 | 7.2，ADR 0004 |
| `block` | `Block` / `BlockKind` / `Attrs` / `BlockTree` | 7.1 |
| `parse` | comrak + 后处理 → 带字节区间的块树，附 `ParseReport` | 8.2，ADR 0002 |
| `document` | `Document`：编辑（同步段）、快照与后台解析（异步段）、undo、BlockId 身份匹配 | 7.3、7.4 |
| `index` | `FullTextIndex` / `VectorIndex` trait 与内存实现 | 10.3、10.4，ADR 0003 |
| `vault` | `VaultPath`、`Vault`：枚举笔记、读取、原子写、`.aero/vault.toml` | 第 5 节 |

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
