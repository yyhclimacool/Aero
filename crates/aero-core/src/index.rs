//! 索引引擎的 trait 抽象（方案 10.3 / 10.4）。
//!
//! `aero-core` 只依赖这里的 trait；zvec 实现放在 `aero-index`。索引文档 = Block，
//! 主键 `pk = "{note_id}:{block_ord}"`。保存路径按 ADR 0003 的约束：`upsert(新块)` + `delete(旧有而新无的 pk)`，
//! 引擎实现不得暴露 `delete_by_filter` 之类的按条件删除。
//!
//! 本模块还提供两个内存实现（[`MemoryFullTextIndex`] / [`MemoryVectorIndex`]），
//! 供 headless 测试与没有引擎时的 CLI 使用；它们不追求性能。

use std::collections::HashMap;

/// 一个待索引的块。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexBlock {
    pub pk: String,
    pub note_id: String,
    pub block_ord: u32,
    /// vault 内相对路径
    pub path: String,
    /// 所属笔记标题
    pub title: String,
    /// 块的正文（源文本去掉标记）
    pub body: String,
    pub tags: Vec<String>,
}

impl IndexBlock {
    pub fn pk_of(note_id: &str, block_ord: u32) -> String {
        format!("{note_id}:{block_ord}")
    }
}

/// 全文查询。`text` 的语法由引擎决定（zvec：`+term -term "phrase"`），内存实现按空白切词做 AND。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FtsQuery {
    pub text: String,
    pub limit: usize,
    /// 只在某篇笔记内搜
    pub note_id: Option<String>,
    /// 必须全部命中的标签
    pub tags: Vec<String>,
}

impl FtsQuery {
    pub fn new(text: impl Into<String>) -> Self {
        FtsQuery { text: text.into(), limit: 50, note_id: None, tags: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub pk: String,
    pub note_id: String,
    pub block_ord: u32,
    pub score: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index engine error: {0}")]
    Engine(String),
    #[error("vector dimension mismatch: index is {expected}, got {got}")]
    Dimension { expected: usize, got: usize },
    #[error("index is closed")]
    Closed,
}

/// 全文索引。写操作 `&mut self`，调用方自行放进锁里；搜索 `&self`。
pub trait FullTextIndex: Send + Sync {
    fn upsert(&mut self, blocks: &[IndexBlock]) -> Result<(), IndexError>;
    /// 按主键删除；不存在的 pk 静默忽略
    fn delete(&mut self, pks: &[String]) -> Result<(), IndexError>;
    fn search(&self, query: &FtsQuery) -> Result<Vec<Hit>, IndexError>;
    /// 持久化缓冲（空闲时调用；保存路径上不需要）
    fn flush(&mut self) -> Result<(), IndexError>;
    /// 段合并等整理
    fn optimize(&mut self) -> Result<(), IndexError>;
    /// 清空，用于全量重建
    fn clear(&mut self) -> Result<(), IndexError>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 一条向量。
#[derive(Clone, Debug, PartialEq)]
pub struct VectorItem {
    pub pk: String,
    pub note_id: String,
    pub block_ord: u32,
    pub vector: Vec<f32>,
}

/// 向量索引（Phase 3）。Embedding 由应用负责，这里只存取向量。
pub trait VectorIndex: Send + Sync {
    fn dim(&self) -> usize;
    fn upsert(&mut self, items: &[VectorItem]) -> Result<(), IndexError>;
    fn delete(&mut self, pks: &[String]) -> Result<(), IndexError>;
    /// 相似度降序的前 `k` 条；`note_id` 限定范围
    fn search(&self, query: &[f32], k: usize, note_id: Option<&str>) -> Result<Vec<Hit>, IndexError>;
    fn flush(&mut self) -> Result<(), IndexError>;
    fn clear(&mut self) -> Result<(), IndexError>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// 内存实现
// ---------------------------------------------------------------------------

/// 子串匹配的内存全文索引：查询按空白切词，全部词都出现（不区分大小写）才命中，
/// 分数 = 命中次数。够测试与小 vault 的 CLI 用。
#[derive(Debug, Default)]
pub struct MemoryFullTextIndex {
    docs: HashMap<String, IndexBlock>,
}

impl MemoryFullTextIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

impl FullTextIndex for MemoryFullTextIndex {
    fn upsert(&mut self, blocks: &[IndexBlock]) -> Result<(), IndexError> {
        for b in blocks {
            self.docs.insert(b.pk.clone(), b.clone());
        }
        Ok(())
    }

    fn delete(&mut self, pks: &[String]) -> Result<(), IndexError> {
        for pk in pks {
            self.docs.remove(pk);
        }
        Ok(())
    }

    fn search(&self, q: &FtsQuery) -> Result<Vec<Hit>, IndexError> {
        let terms: Vec<String> = q.text.split_whitespace().map(|t| t.to_lowercase()).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits: Vec<Hit> = self
            .docs
            .values()
            .filter(|d| q.note_id.as_ref().is_none_or(|n| *n == d.note_id))
            .filter(|d| q.tags.iter().all(|t| d.tags.iter().any(|dt| dt.eq_ignore_ascii_case(t))))
            .filter_map(|d| {
                let hay = format!("{}\n{}\n{}", d.title, d.body, d.tags.join(" ")).to_lowercase();
                let mut score = 0usize;
                for t in &terms {
                    let n = hay.matches(t.as_str()).count();
                    if n == 0 {
                        return None;
                    }
                    score += n;
                }
                Some(Hit {
                    pk: d.pk.clone(),
                    note_id: d.note_id.clone(),
                    block_ord: d.block_ord,
                    score: score as f32,
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.pk.cmp(&b.pk)));
        hits.truncate(q.limit.max(1));
        Ok(hits)
    }

    fn flush(&mut self) -> Result<(), IndexError> {
        Ok(())
    }

    fn optimize(&mut self) -> Result<(), IndexError> {
        Ok(())
    }

    fn clear(&mut self) -> Result<(), IndexError> {
        self.docs.clear();
        Ok(())
    }

    fn len(&self) -> usize {
        self.docs.len()
    }
}

/// 暴力余弦相似度的内存向量索引。
#[derive(Debug)]
pub struct MemoryVectorIndex {
    dim: usize,
    items: HashMap<String, VectorItem>,
}

impl MemoryVectorIndex {
    pub fn new(dim: usize) -> Self {
        MemoryVectorIndex { dim, items: HashMap::new() }
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

impl VectorIndex for MemoryVectorIndex {
    fn dim(&self) -> usize {
        self.dim
    }

    fn upsert(&mut self, items: &[VectorItem]) -> Result<(), IndexError> {
        for it in items {
            if it.vector.len() != self.dim {
                return Err(IndexError::Dimension { expected: self.dim, got: it.vector.len() });
            }
        }
        for it in items {
            self.items.insert(it.pk.clone(), it.clone());
        }
        Ok(())
    }

    fn delete(&mut self, pks: &[String]) -> Result<(), IndexError> {
        for pk in pks {
            self.items.remove(pk);
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize, note_id: Option<&str>) -> Result<Vec<Hit>, IndexError> {
        if query.len() != self.dim {
            return Err(IndexError::Dimension { expected: self.dim, got: query.len() });
        }
        let mut hits: Vec<Hit> = self
            .items
            .values()
            .filter(|it| note_id.is_none_or(|n| n == it.note_id))
            .map(|it| Hit {
                pk: it.pk.clone(),
                note_id: it.note_id.clone(),
                block_ord: it.block_ord,
                score: cosine(query, &it.vector),
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.pk.cmp(&b.pk)));
        hits.truncate(k);
        Ok(hits)
    }

    fn flush(&mut self) -> Result<(), IndexError> {
        Ok(())
    }

    fn clear(&mut self) -> Result<(), IndexError> {
        self.items.clear();
        Ok(())
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(note: &str, ord: u32, body: &str, tags: &[&str]) -> IndexBlock {
        IndexBlock {
            pk: IndexBlock::pk_of(note, ord),
            note_id: note.into(),
            block_ord: ord,
            path: format!("{note}.md"),
            title: note.into(),
            body: body.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn fts_upsert_search_delete() {
        let mut idx = MemoryFullTextIndex::new();
        idx.upsert(&[
            block("a", 0, "Rope 与 anchor 的实现", &["rust"]),
            block("a", 1, "zvec 索引引擎", &["rust", "index"]),
            block("b", 0, "anchor 漂移测试", &[]),
        ])
        .unwrap();
        assert_eq!(idx.len(), 3);

        let hits = idx.search(&FtsQuery::new("anchor")).unwrap();
        assert_eq!(hits.len(), 2);

        let mut q = FtsQuery::new("anchor");
        q.note_id = Some("b".into());
        assert_eq!(idx.search(&q).unwrap()[0].pk, "b:0");

        let mut q = FtsQuery::new("索引");
        q.tags = vec!["INDEX".into()];
        assert_eq!(idx.search(&q).unwrap().len(), 1);

        // upsert 覆盖同一 pk
        idx.upsert(&[block("a", 0, "改写后没有那个词了", &[])]).unwrap();
        assert_eq!(idx.search(&FtsQuery::new("anchor")).unwrap().len(), 1);

        idx.delete(&["b:0".into(), "not-exist".into()]).unwrap();
        assert!(idx.search(&FtsQuery::new("anchor")).unwrap().is_empty());
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn vector_search_orders_by_cosine() {
        let mut idx = MemoryVectorIndex::new(2);
        idx.upsert(&[
            VectorItem { pk: "a:0".into(), note_id: "a".into(), block_ord: 0, vector: vec![1.0, 0.0] },
            VectorItem { pk: "a:1".into(), note_id: "a".into(), block_ord: 1, vector: vec![0.0, 1.0] },
            VectorItem { pk: "b:0".into(), note_id: "b".into(), block_ord: 0, vector: vec![0.7, 0.7] },
        ])
        .unwrap();
        let hits = idx.search(&[1.0, 0.1], 2, None).unwrap();
        assert_eq!(hits[0].pk, "a:0");
        assert_eq!(hits[1].pk, "b:0");
        let hits = idx.search(&[1.0, 0.1], 5, Some("b")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(idx.search(&[1.0], 1, None), Err(IndexError::Dimension { expected: 2, got: 1 })));
    }
}
