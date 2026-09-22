//! Aero 的 headless 核心：vault、Knowledge Markdown 解析、Block 模型、
//! 随编辑漂移的 Anchor，以及索引引擎的 trait 抽象。
//!
//! GUI（`aero-app`）、CLI（`aero-cli`）与 MCP（`aero-mcp`）都是本 crate 的客户端，
//! 任何一方都没有特权路径。本 crate 不依赖 GPUI、Tokio 或任何具体索引引擎。
//!
//! 模块对应方案章节：
//! - [`text`] / [`anchors`]：Rope 与 Anchor（7.2，ADR 0004）
//! - [`block`]：Block 模型（7.1–7.3）
//! - [`parse`]：整篇解析为带区间的块树（8.2，ADR 0002）
//! - [`document`]：Document，编辑管线的同步段与异步段（7.4）
//! - [`index`]：`FullTextIndex` / `VectorIndex` trait（10.3 / 10.4，ADR 0003）
//! - [`vault`]：vault 路径、枚举、原子读写（第 5 节）

pub mod anchors;
pub mod attachments;
pub mod block;
pub mod block_refs;
pub mod document;
pub mod headings;
pub mod highlight;
pub mod index;
pub mod inline_format;
pub mod inline_links;
pub mod links;
mod markdown_links;
pub mod parse;
pub mod rename;
pub mod tags;
pub mod text;
pub mod vault;

pub use anchors::{Anchor, Bias, MarkerTable};
pub use block::{Attrs, Block, BlockId, BlockKind, BlockTree, CalloutKind, Fold};
pub use document::{Document, Snapshot};
pub use index::{FtsQuery, FullTextIndex, Hit, IndexBlock, IndexError, VectorIndex, VectorItem};
pub use parse::{ParseOutput, ParseReport, ParsedBlock, parse};
pub use text::{Edit, EditError, LineEnding, Rope, RopeSlice, SourceText};
pub use vault::{FileStamp, Vault, VaultError, VaultPath};

/// 应用层显示与 CLI `--version` 共用的版本号。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
