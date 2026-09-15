//! Aero 的 headless 核心：vault、Knowledge Markdown 解析与无损序列化、Block 模型、
//! 链接解析，以及索引引擎的 trait 抽象。
//!
//! GUI（`aero-app`）、CLI（`aero-cli`）与 MCP（`aero-mcp`）都是本 crate 的客户端，
//! 任何一方都没有特权路径。本 crate 不依赖 GPUI、Tokio 或任何具体索引引擎。

/// 应用层显示与 CLI `--version` 共用的版本号。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
