//! 索引层（方案第 10 节）：`aero-core` 只定义 trait，这里放引擎实现与管线。
//!
//! - [`zvec_fts::ZvecFts`]：`FullTextIndex` 的 zvec 实现（ADR 0003）
//! - [`meta::Meta`]：SQLite 元数据（10.2 的 M1b 子集）
//! - [`indexer::Indexer`]：vault → 块 → 索引的增量管线，含搜索入口
//!
//! 全部 API 同步阻塞；UI 层放到后台线程。

pub mod indexer;
pub mod meta;
pub mod zvec_fts;

pub use indexer::{Indexer, IndexerError, Refresh, SearchHit, SyncPlan, SyncReport};
pub use meta::{Meta, NoteRow};
pub use zvec_fts::{ZvecFts, ensure_initialized};
