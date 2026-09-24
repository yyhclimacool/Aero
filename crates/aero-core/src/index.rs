//! 索引引擎的 trait 抽象（方案 10.3 / 10.4）。
//!
//! `aero-core` 只依赖这里的 trait；zvec 实现放在 `aero-index`。索引文档 = Block，
//! 主键 `pk = "{note_id}#{block_ord}"`（zvec 的主键不接受 `:`）。保存路径按 ADR 0003 的约束：`upsert(新块)` + `delete(旧有而新无的 pk)`，
//! 引擎实现不得暴露 `delete_by_filter` 之类的按条件删除。
//!
//! 本模块还提供两个内存实现（[`MemoryFullTextIndex`] / [`MemoryVectorIndex`]），
//! 供 headless 测试与没有引擎时的 CLI 使用；它们不追求性能。

use std::collections::HashMap;
use std::sync::Arc;

use crate::block::{Block, BlockKind};
use crate::document::Document;

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
    pub aliases: Arc<[String]>,
    /// 块的正文（源文本去掉标记）
    pub body: String,
    /// 所属笔记的标签（front matter `tags` + 正文行内 `#tag`），笔记级：同一篇的每个块都带同一组
    pub tags: Vec<String>,
}

impl IndexBlock {
    pub fn pk_of(note_id: &str, block_ord: u32) -> String {
        format!("{note_id}#{block_ord}")
    }
}

/// 全文查询。`text` 的语法由引擎决定（zvec：`+term -term "phrase"`），内存实现按空白切词做 AND。
///
/// 标签是笔记级的（见 [`blocks_for_note`]），按层级匹配：查 `rust` 命中带 `rust` 或 `rust/...` 的笔记
/// （见 [`tag_matches`]）。`tags` 全部要命中，`exclude_tags` 命中任一即排除。
///
/// `text` 为空而 `tags` 非空是**纯标签查询**：引擎每篇笔记只返回 `block_ord == 0` 那一块，
/// 否则一篇长笔记的几百个块会把 `limit` 吃满、挤掉别的笔记。只有 `exclude_tags` 的查询不成立（返回空）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FtsQuery {
    pub text: String,
    pub limit: usize,
    /// 只在某篇笔记内搜
    pub note_id: Option<String>,
    /// 必须全部命中的标签（小写、不带 `#`）
    pub tags: Vec<String>,
    /// 命中任一就排除的标签（小写、不带 `#`）
    pub exclude_tags: Vec<String>,
}

impl FtsQuery {
    pub fn new(text: impl Into<String>) -> Self {
        FtsQuery { text: text.into(), limit: 50, note_id: None, tags: Vec::new(), exclude_tags: Vec::new() }
    }

    /// 从搜索框文本里拆出标签条件（`ui_design.md` 4.2）：
    /// `#tag` / `tag:tag` / `tag:#tag` 是要求，前面加 `-` 是排除；标签后面粘着的标点（`#rust,`）忽略；
    /// 其余原样交给引擎做全文匹配（引号内的空格不拆）。
    pub fn parse(text: &str) -> Self {
        let mut query = Self::new("");
        let mut terms = Vec::new();
        let mut quoted = false;
        let mut escaped = false;
        let mut start = 0;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            if c.is_whitespace() && !quoted && !escaped {
                let token = &text[start..i];
                if !token.is_empty() {
                    match tag_token(token) {
                        Some((tag, false)) => {
                            if !query.tags.contains(&tag) {
                                query.tags.push(tag);
                            }
                        }
                        Some((tag, true)) => {
                            if !query.exclude_tags.contains(&tag) {
                                query.exclude_tags.push(tag);
                            }
                        }
                        None => terms.push(token),
                    }
                }
                start = i + c.len_utf8();
            }
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = !quoted;
            }
        }
        if start < text.len() {
            terms.push(&text[start..]);
        }
        query.text = terms.join(" ");
        query
    }

    /// 有没有任何能产生结果的条件。
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.tags.is_empty()
    }
}

/// 查询里的一个词是不是标签条件：`(标签, 是否排除)`。
fn tag_token(token: &str) -> Option<(String, bool)> {
    let (token, exclude) = match token.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let tag = token
        .strip_prefix("tag:#")
        .or_else(|| token.strip_prefix("tag:"))
        .or_else(|| token.strip_prefix('#'))?;
    // 粘在后面的标点：`#rust,` `#rust。` `(#rust)`
    let tag = tag.trim_end_matches(|c: char| !tag_char(c) || c == '/');
    normalize_tag(tag).map(|tag| (tag, exclude))
}

pub(crate) fn tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '/')
}

fn normalize_tag(tag: &str) -> Option<String> {
    let first = tag.chars().next()?;
    (!first.is_numeric() && tag.chars().all(tag_char)).then(|| tag.to_lowercase())
}

/// 标签层级匹配：查 `rust` 命中 `rust` 与 `rust/cli`，但不命中 `rusty`；两边都应已小写。
pub fn tag_matches(query: &str, doc_tag: &str) -> bool {
    doc_tag == query
        || (doc_tag.len() > query.len()
            && doc_tag.starts_with(query)
            && doc_tag.as_bytes()[query.len()] == b'/')
}

/// 一个标签自身加上所有祖先：`a/b/c` → `[a/b/c, a/b, a]`。引擎按精确子串过滤时，把它们都写进过滤字段，
/// 父标签查询就不需要前缀匹配。
pub fn tag_with_ancestors(tag: &str) -> Vec<String> {
    let mut out = vec![tag.to_owned()];
    let mut rest = tag;
    while let Some((parent, _)) = rest.rsplit_once('/') {
        if !parent.is_empty() {
            out.push(parent.to_owned());
        }
        rest = parent;
    }
    out
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
    /// 按主键取回索引文档（搜索结果要显示正文与标题）；不存在的 pk 跳过，顺序不保证
    fn fetch(&self, pks: &[String]) -> Result<Vec<IndexBlock>, IndexError>;
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
// 文档 → 索引文档
// ---------------------------------------------------------------------------

/// 要进索引的叶子块：跳过 front matter、`^id` / 属性标记行、分割线，以及去掉标记后为空的块。
/// 返回顺序即 `block_ord`——搜索结果用 ord 回到这里定位块。
pub fn indexable_leaves(doc: &Document) -> Vec<&Block> {
    indexable_bodies(doc).map(|(block, _)| block).collect()
}

/// 过滤和编号共用同一条路径；入库时复用正文，不再为每块解析两遍。
pub fn indexable_bodies(doc: &Document) -> impl Iterator<Item = (&Block, String)> {
    let mut extractor = extraction::Extractor::default();
    doc.blocks().leaves().filter_map(move |block| {
        if matches!(block.kind, BlockKind::FrontMatter | BlockKind::Marker | BlockKind::ThematicBreak) {
            return None;
        }
        let body = extractor.plain_text(&doc.slice(doc.content_range(block)).to_string(), &block.kind);
        (!body.trim().is_empty()).then_some((block, body))
    })
}

/// 把一篇文档拆成索引文档（10.3：索引粒度到块）。`ord` 连续，删除旧块时只需知道旧的块数。
pub fn blocks_for_note(doc: &Document, note_id: &str, path: &str, title: &str) -> Vec<IndexBlock> {
    blocks_and_links_for_note(doc, note_id, path, title).0
}

/// [`blocks_for_note`] 外加这篇的全部 wikilink 区间（`links::scan` 的结果）——标签与链接共用一次行内解析。
pub fn blocks_and_links_for_note(
    doc: &Document,
    note_id: &str,
    path: &str,
    title: &str,
) -> (Vec<IndexBlock>, Vec<crate::links::LinkSpan>) {
    let text = doc.to_string();
    let (tag_sources, links) = crate::parse::inline_scan(&text);
    let tags = note_tags(doc, &tag_sources);
    let aliases: Arc<[String]> = doc.front_matter_aliases().into();
    let mut blocks: Vec<_> = indexable_bodies(doc)
        .enumerate()
        .map(|(ord, (_, body))| IndexBlock {
            pk: IndexBlock::pk_of(note_id, ord as u32),
            note_id: note_id.to_owned(),
            block_ord: ord as u32,
            path: path.to_owned(),
            title: title.to_owned(),
            aliases: aliases.clone(),
            body,
            tags: tags.clone(),
        })
        .collect();
    if blocks.is_empty() && !aliases.is_empty() {
        blocks.push(IndexBlock {
            pk: IndexBlock::pk_of(note_id, 0),
            note_id: note_id.to_owned(),
            block_ord: 0,
            path: path.to_owned(),
            title: title.to_owned(),
            aliases,
            body: String::new(),
            tags,
        });
    }
    (blocks, links)
}

/// front matter 里的 `tags:` / `tag:`（Obsidian 两个键都认）。
/// 值可以是一个标量、标量列表；字符串再按逗号 / 空白拆开（`tags: rust, cli` 是老写法）。
/// 数字、布尔照字面转成字符串再走 `normalize_tag`（`2024` 因首字符是数字被丢，`v2` 留下）；
/// 嵌套结构、null 单独跳过，**不能**因为一项不合规就丢掉整组。
fn front_matter_tags(yaml: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct FrontMatter {
        tags: Option<TagValues>,
        tag: Option<TagValues>,
    }

    let Ok(FrontMatter { tags, tag }) = serde_saphyr::from_str::<FrontMatter>(yaml) else {
        return Vec::new();
    };
    [tags, tag].into_iter().flatten().flat_map(|v| v.0).collect()
}

/// `tags:` 的值：一个标量、标量列表，或者别的什么（映射、嵌套列表——整体忽略）。
/// 手写 Visitor 走 `deserialize_any`，逐项容错；`#[serde(untagged)]` 会在任一项类型不符时让整组失败。
struct TagValues(Vec<String>);

/// 列表里的一项：只接受标量，嵌套结构丢弃。
struct TagItem(Vec<String>);

fn split_tag_text(text: &str) -> Vec<String> {
    text.split([',', ' ', '\t']).map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect()
}

macro_rules! scalar_visits {
    () => {
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Self::Value::from(split_tag_text(v)))
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Self::Value::from(vec![v.to_string()]))
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Self::Value::from(vec![v.to_string()]))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Self::Value::from(vec![v.to_string()]))
        }
        fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Self::Value::from(vec![v.to_string()]))
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Self::Value::from(Vec::new()))
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Self::Value::from(Vec::new()))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
            Ok(Self::Value::from(Vec::new()))
        }
    };
}

impl From<Vec<String>> for TagValues {
    fn from(v: Vec<String>) -> Self {
        TagValues(v)
    }
}

impl From<Vec<String>> for TagItem {
    fn from(v: Vec<String>) -> Self {
        TagItem(v)
    }
}

impl<'de> serde::Deserialize<'de> for TagValues {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = TagValues;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a tag or a list of tags")
            }
            scalar_visits!();
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<TagValues, A::Error> {
                let mut out = Vec::new();
                while let Some(TagItem(v)) = seq.next_element::<TagItem>()? {
                    out.extend(v);
                }
                Ok(TagValues(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> serde::Deserialize<'de> for TagItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = TagItem;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a tag")
            }
            scalar_visits!();
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<TagItem, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(TagItem(Vec::new()))
            }
        }
        d.deserialize_any(V)
    }
}

/// `tag_sources` 是 [`crate::parse::inline_scan`] 给出的正文片段（调用方已解析过一次，这里不再解析）。
fn note_tags(doc: &Document, tag_sources: &[&str]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut add = |tag: &str| {
        if let Some(tag) = normalize_tag(tag)
            && !tags.contains(&tag)
        {
            tags.push(tag);
        }
    };
    if let Some(fm) = doc.blocks().roots.first().filter(|b| b.kind == BlockKind::FrontMatter) {
        let source = doc.block_text(fm);
        let yaml = source.lines().skip(1).take_while(|line| *line != "---").collect::<Vec<_>>().join("\n");
        for tag in front_matter_tags(&yaml) {
            add(tag.strip_prefix('#').unwrap_or(&tag));
        }
    }
    for source in tag_sources {
        let mut chars = source.char_indices().peekable();
        let mut previous = None;
        while let Some((i, c)) = chars.next() {
            if c == '\\' {
                previous = chars.next().map(|(_, c)| c);
                continue;
            }
            if c == '#' && previous.is_none_or(|p| !tag_char(p) && p != '#') {
                // comrak 的自动链接不会识别单标签主机名等 URL。
                let prefix = source[..i].rsplit(char::is_whitespace).next().unwrap_or("");
                if prefix.contains("://") {
                    previous = Some(c);
                    continue;
                }
                let start = i + 1;
                let mut end = start;
                while let Some(&(pos, next)) = chars.peek() {
                    if !tag_char(next) {
                        break;
                    }
                    end = pos + next.len_utf8();
                    chars.next();
                }
                add(&source[start..end]);
            }
            previous = Some(c);
        }
    }
    tags
}

/// 笔记级标签（小写、去重），包含仅有 front matter、没有可索引正文块的笔记。
pub fn tags_for_note(doc: &Document) -> Vec<String> {
    let text = doc.to_string();
    let (sources, _) = crate::parse::inline_scan(&text);
    note_tags(doc, &sources)
}

#[path = "index_text.rs"]
mod extraction;

/// 从 Markdown AST 抽取可见正文；代码、数学与转义字面不作标记清理。
/// 链接保留 label / alt / alias，HTML 换行保留，元数据与不可见内容不进正文。
pub fn plain_text(src: &str, kind: &BlockKind) -> String {
    extraction::plain_text(src, kind)
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
        if terms.is_empty() && q.tags.is_empty() {
            return Ok(Vec::new());
        }
        let tags: Vec<String> = q.tags.iter().map(|tag| tag.to_lowercase()).collect();
        let excluded: Vec<String> = q.exclude_tags.iter().map(|tag| tag.to_lowercase()).collect();
        // 纯标签查询：标签是笔记级的，每篇只回第一块（见 FtsQuery 文档）
        let tag_only = terms.is_empty();
        let has = |d: &IndexBlock, t: &str| d.tags.iter().any(|dt| tag_matches(t, &dt.to_lowercase()));
        let mut hits: Vec<Hit> = self
            .docs
            .values()
            .filter(|d| !tag_only || d.block_ord == 0)
            .filter(|d| q.note_id.as_ref().is_none_or(|n| *n == d.note_id))
            .filter(|d| tags.iter().all(|t| has(d, t)))
            .filter(|d| !excluded.iter().any(|t| has(d, t)))
            .filter_map(|d| {
                let hay = format!("{}\n{}\n{}\n{}", d.title, d.aliases.join("\n"), d.body, d.tags.join(" "))
                    .to_lowercase();
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

    fn fetch(&self, pks: &[String]) -> Result<Vec<IndexBlock>, IndexError> {
        Ok(pks.iter().filter_map(|pk| self.docs.get(pk).cloned()).collect())
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

    #[test]
    fn alias_metadata_is_shared_across_blocks() {
        let doc = Document::from_text(&format!(
            "---\naliases: {}\n---\n\n{}",
            "long-alias".repeat(32),
            "body\n\n".repeat(32)
        ));
        let blocks = blocks_for_note(&doc, "n", "n.md", "Display");
        assert_eq!(blocks.len(), 32);
        for block in &blocks[1..] {
            assert_eq!(block.aliases[0].as_ptr(), blocks[0].aliases[0].as_ptr());
        }
    }

    #[test]
    fn alias_only_note_is_searchable_without_front_matter_in_body() {
        let doc = Document::from_text("---\naliases: Quicksilver\nsecret: hidden\n---\n");
        let blocks = blocks_for_note(&doc, "n", "n.md", "Display");
        let mut idx = MemoryFullTextIndex::new();
        idx.upsert(&blocks).unwrap();
        let hits = idx.search(&FtsQuery::new("Quicksilver")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(idx.fetch(&[hits[0].pk.clone()]).unwrap()[0].body.is_empty());
        assert!(idx.search(&FtsQuery::new("hidden")).unwrap().is_empty());
    }

    #[test]
    fn aliases_are_searchable_without_changing_body_and_are_replaced() {
        let mut idx = MemoryFullTextIndex::new();
        let doc = Document::from_text(
            "---\naliases: [Quicksilver, '雪 山']\ntags: [rust/cli]\n---\n\nordinary body\n",
        );
        let blocks = blocks_for_note(&doc, "n", "n.md", "Display");
        idx.upsert(&blocks).unwrap();
        let mut q = FtsQuery::parse("quicksilver #rust");
        assert_eq!(idx.search(&q).unwrap().len(), 1);
        assert_eq!(idx.fetch(&["n#0".into()]).unwrap()[0].body, "ordinary body");
        q.note_id = Some("other".into());
        assert!(idx.search(&q).unwrap().is_empty());
        assert!(idx.search(&FtsQuery::parse("quicksilver -#rust")).unwrap().is_empty());
        idx.upsert(&blocks_for_note(&Document::from_text("ordinary body\n"), "n", "n.md", "Display"))
            .unwrap();
        assert!(idx.search(&FtsQuery::new("quicksilver")).unwrap().is_empty());
    }

    fn block(note: &str, ord: u32, body: &str, tags: &[&str]) -> IndexBlock {
        IndexBlock {
            pk: IndexBlock::pk_of(note, ord),
            note_id: note.into(),
            block_ord: ord,
            path: format!("{note}.md"),
            title: note.into(),
            aliases: Arc::default(),
            body: body.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn parses_tag_filters_without_changing_quoted_or_boolean_text() {
        let q = FtsQuery::parse("anchor #Rust tag:#CLI #rust/cli #RUST +must -omit \"literal #keep\"");
        assert_eq!(q.text, "anchor +must -omit \"literal #keep\"");
        assert_eq!(q.tags, ["rust", "cli", "rust/cli"]);
        assert_eq!(FtsQuery::parse("#中文标签").tags, ["中文标签"]);
        assert_eq!(FtsQuery::parse("tag:#rust #cli").text, "");
        let literal = r##""#rust" \#escaped #123 ##heading tag:# +#required -# -tag:"##;
        assert_eq!(FtsQuery::parse(literal).text, literal);
        assert!(FtsQuery::parse(literal).tags.is_empty());
        assert!(FtsQuery::parse(literal).exclude_tags.is_empty());
        let escaped = r##""a \" #literal" #filter"##;
        assert_eq!(FtsQuery::parse(escaped).tags, ["filter"]);
        assert_eq!(FtsQuery::parse(escaped).text, r##""a \" #literal""##);
        assert!(FtsQuery::parse("\"unclosed #literal").tags.is_empty());
    }

    /// 4.2 的标签语法补全：`-#tag` 排除、`tag:rust` 不带 `#`、尾标点忽略、层级匹配。
    #[test]
    fn parses_exclusions_bare_tag_prefix_and_trailing_punctuation() {
        let q = FtsQuery::parse("anchor -#Draft tag:rust -tag:wip -tag:#Old #cli, (#中文) #a/b/ -#draft");
        assert_eq!(q.text, "anchor (#中文)");
        assert_eq!(q.tags, ["rust", "cli", "a/b"]);
        assert_eq!(q.exclude_tags, ["draft", "wip", "old"]);
        assert_eq!(FtsQuery::parse("#rust。").tags, ["rust"]);
        assert_eq!(FtsQuery::parse("#rust:").tags, ["rust"]);
        // 只有排除条件：不成立
        assert!(FtsQuery::parse("-#draft").is_empty());
        assert!(!FtsQuery::parse("#draft").is_empty());
        assert!(!FtsQuery::parse("x -#draft").is_empty());

        assert!(tag_matches("rust", "rust"));
        assert!(tag_matches("rust", "rust/cli"));
        assert!(tag_matches("rust/cli", "rust/cli/x"));
        assert!(!tag_matches("rust", "rusty"));
        assert!(!tag_matches("rust/cli", "rust"));
        assert_eq!(tag_with_ancestors("a/b/c"), ["a/b/c", "a/b", "a"]);
        assert_eq!(tag_with_ancestors("solo"), ["solo"]);
    }

    /// 内存实现：层级匹配与排除。
    #[test]
    fn memory_index_matches_parent_tags_and_excludes() {
        let mut idx = MemoryFullTextIndex::new();
        idx.upsert(&[
            block("a", 0, "needle", &["rust/cli"]),
            block("b", 0, "needle", &["rust", "draft"]),
            block("c", 0, "needle", &["rusty"]),
            block("d", 0, "needle", &[]),
        ])
        .unwrap();
        let notes = |q: &FtsQuery| {
            let mut v: Vec<String> = idx.search(q).unwrap().into_iter().map(|h| h.note_id).collect();
            v.sort();
            v
        };
        assert_eq!(notes(&FtsQuery::parse("#rust")), ["a", "b"]);
        assert_eq!(notes(&FtsQuery::parse("needle #rust")), ["a", "b"]);
        assert_eq!(notes(&FtsQuery::parse("#rust/cli")), ["a"]);
        assert_eq!(notes(&FtsQuery::parse("needle -#rust")), ["c", "d"]);
        assert_eq!(notes(&FtsQuery::parse("#rust -#draft")), ["a"]);
        assert_eq!(notes(&FtsQuery::parse("needle -#rust/cli")), ["b", "c", "d"]);
        assert!(notes(&FtsQuery::parse("-#draft")).is_empty());
    }

    #[test]
    fn note_tags_merge_front_matter_and_inline_tags_for_every_block() {
        for yaml in ["tags: [Rust, '#cli', 中文标签]", "tags:\n  - Rust\n  - '#cli'\n  - 中文标签"] {
            let doc = Document::from_text(&format!(
                "---\n{yaml}\n---\n\n# Title\n\n第一段 #rust/cli #RUST\n\n没有标签的 anchor\n"
            ));
            let blocks = blocks_for_note(&doc, "n", "n.md", "N");
            assert_eq!(blocks.len(), 3);
            for block in blocks {
                assert_eq!(block.tags, ["rust", "cli", "中文标签", "rust/cli"]);
            }
        }
        let doc = Document::from_text("---\ntags: Rust\n---\n\nanchor\n");
        assert_eq!(blocks_for_note(&doc, "n", "n.md", "N")[0].tags, ["rust"]);
    }

    /// 一项不合规（数字、嵌套、null）只丢那一项，不能连累同组其他标签；
    /// 字符串按逗号 / 空白拆；`tag:` 单数键与 `tags:` 等价。
    #[test]
    fn front_matter_tags_survive_non_string_items_and_legacy_formats() {
        let tags_of = |yaml: &str| {
            let doc = Document::from_text(&format!("---\n{yaml}\n---\n\nanchor\n"));
            blocks_for_note(&doc, "n", "n.md", "N")[0].tags.clone()
        };
        assert_eq!(tags_of("tags: [2024, rust]"), ["rust"]);
        assert_eq!(tags_of("tags: [rust, 1.5, true, v2]"), ["rust", "true", "v2"]);
        assert_eq!(tags_of("tags:\n  - rust\n  - [nested]\n  - {k: v}\n  -\n  - cli"), ["rust", "cli"]);
        assert_eq!(tags_of("tags: rust, cli"), ["rust", "cli"]);
        assert_eq!(tags_of("tags: \"rust cli,  #中文\""), ["rust", "cli", "中文"]);
        assert_eq!(tags_of("tag: rust"), ["rust"]);
        assert_eq!(tags_of("tag: [a]\ntags: [b, a]"), ["b", "a"]);
        assert!(tags_of("tags:").is_empty());
        assert!(tags_of("tags: 2024").is_empty());
        assert!(tags_of("tags: {rust: 1}").is_empty());
        assert!(tags_of("title: only").is_empty());
    }

    /// wikilink 目标里的 `#` 是标题 / 块锚点，不是标签。
    #[test]
    fn note_tags_exclude_wikilink_anchors() {
        let doc = Document::from_text(
            "see [[#heading]] [[note#sec]] [[note#^blk]] [[note#sec|alias]] #real\n\n# heading\n",
        );
        assert_eq!(blocks_for_note(&doc, "n", "n.md", "N")[0].tags, ["real"]);
    }

    #[test]
    fn note_tags_follow_markdown_context_and_tag_boundaries() {
        let doc = Document::from_text(concat!(
            "# Heading #not-heading\n\n",
            "(#rust), **#bold** #中文标签。 #rust_cli #rust-cli #v2 #ÉCOLE\n\n",
            "`#inline-code` \\#escaped #123 ##double https://host/#fragment https://host/?q=#query\n\n",
            "[link](https://host/#url) ![image](#image) {.red #attribute}\n\n",
            "```sh\n#fenced\n```\n\n    #indented\n\n",
            "<div>\n#html\n</div>\n\n",
            "> #quoted\n\n- #listed\n\n| cell |\n| --- |\n| #table |\n"
        ));
        let blocks = blocks_for_note(&doc, "n", "n.md", "N");
        assert_eq!(
            blocks[0].tags,
            ["rust", "bold", "中文标签", "rust_cli", "rust-cli", "v2", "école", "quoted", "listed", "table"]
        );
        assert!(blocks.iter().all(|b| b.tags == blocks[0].tags));
        let invalid = Document::from_text("---\ntags: [broken\n---\n\n#inline\n");
        assert_eq!(blocks_for_note(&invalid, "n", "n.md", "N")[0].tags, ["inline"]);
    }

    #[test]
    fn note_tags_exclude_www_autolinks() {
        let doc = Document::from_text("anchor www.example.com/?q=#ghost\n");
        assert!(blocks_for_note(&doc, "n", "n.md", "N")[0].tags.is_empty());
    }

    #[test]
    fn note_tags_exclude_attributes_split_by_inline_markup() {
        let doc = Document::from_text("anchor {#private title=\"*value*\"}\n");
        assert!(blocks_for_note(&doc, "n", "n.md", "N")[0].tags.is_empty());
    }

    #[test]
    fn memory_tag_only_queries_are_exact_and_filtered_before_limit() {
        let mut idx = MemoryFullTextIndex::new();
        idx.upsert(&[
            block("a", 0, "anchor anchor anchor", &[]),
            block("b", 0, "anchor", &["Rust", "CLI", "École"]),
            block("b", 1, "anchor again", &["Rust", "CLI", "École"]),
            block("c", 0, "anchor anchor", &["rust/cli"]),
        ])
        .unwrap();
        let pks = |hits: Vec<Hit>| hits.into_iter().map(|h| h.pk).collect::<Vec<_>>();
        let mut q = FtsQuery::new("");
        q.tags = vec!["RUST".into(), "cli".into(), "école".into()];
        // 纯标签查询每篇只回第一块
        assert_eq!(pks(idx.search(&q).unwrap()), ["b#0"]);
        // 带文本就回所有命中的块
        q.text = "anchor".into();
        assert_eq!(pks(idx.search(&q).unwrap()), ["b#0", "b#1"]);
        q.limit = 1;
        assert_eq!(idx.search(&q).unwrap()[0].pk, "b#0");
        q.note_id = Some("c".into());
        assert!(idx.search(&q).unwrap().is_empty());
        assert!(idx.search(&FtsQuery::new("")).unwrap().is_empty());
    }

    /// 纯标签查询下，一篇长笔记不能把 `limit` 吃满、挤掉别的带标签笔记。
    #[test]
    fn memory_tag_only_queries_return_one_hit_per_note() {
        let mut idx = MemoryFullTextIndex::new();
        let mut blocks: Vec<IndexBlock> = (0..50).map(|i| block("long", i, "para", &["rust"])).collect();
        blocks.push(block("short", 0, "para", &["rust"]));
        blocks.push(block("untagged", 0, "para", &[]));
        idx.upsert(&blocks).unwrap();
        let mut q = FtsQuery::new("");
        q.tags = vec!["rust".into()];
        q.limit = 10;
        let mut pks: Vec<String> = idx.search(&q).unwrap().into_iter().map(|h| h.pk).collect();
        pks.sort();
        assert_eq!(pks, ["long#0", "short#0"]);
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
        assert_eq!(idx.search(&q).unwrap()[0].pk, "b#0");

        let mut q = FtsQuery::new("索引");
        q.tags = vec!["INDEX".into()];
        assert_eq!(idx.search(&q).unwrap().len(), 1);

        // upsert 覆盖同一 pk
        idx.upsert(&[block("a", 0, "改写后没有那个词了", &[])]).unwrap();
        assert_eq!(idx.search(&FtsQuery::new("anchor")).unwrap().len(), 1);

        idx.delete(&["b#0".into(), "not-exist".into()]).unwrap();
        assert!(idx.search(&FtsQuery::new("anchor")).unwrap().is_empty());
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn plain_text_strips_markup() {
        let p = BlockKind::Paragraph;
        assert_eq!(
            plain_text("**粗体** 与 *斜体*、`code`、==高亮==、~~删除~~", &p),
            "粗体 与 斜体、code、高亮、删除"
        );
        assert_eq!(
            plain_text("见 [[rust/ripgrep|rg]] 和 [[tokio#运行时]] 与 ![[img.png|400]]", &p),
            "见 rg 和 tokio#运行时 与 img.png"
        );
        assert_eq!(
            plain_text("[文档](https://x.y) ![图](a.png) <https://z.w> <b>x</b>", &p),
            "文档 图 https://z.w x"
        );
        assert_eq!(
            plain_text("段落 {.red} 结束 #tag snake_case 1 = 2 ~ 3", &p),
            "段落  结束 #tag snake_case 1 = 2 ~ 3"
        );
        assert_eq!(plain_text("\\*literal\\*", &p), "*literal*");
        assert_eq!(plain_text("## 标题 **x**", &BlockKind::Heading { level: 2 }), "标题 x");
        assert_eq!(
            plain_text("```rust\nlet a = 1;\n```", &BlockKind::CodeFence { info: String::new() }),
            "let a = 1;"
        );
        assert_eq!(plain_text("    x\n    y", &BlockKind::CodeBlock), "x\ny");
        assert_eq!(plain_text("| a | **b** |\n| --- | --- |\n| 1 | 2 |", &BlockKind::Table), "a  b\n1  2");
        assert_eq!(plain_text("> 引用\n> > 嵌套", &BlockKind::BlockQuote), "引用\n嵌套");
    }

    #[test]
    fn plain_text_keeps_chinese_table_cells_breaks_and_escaped_pipes() {
        let src = "| __中文表头__ | 说明 |\n| :--- | ---: |\n| 甲\\|乙 | **第一行**<br><br>第二行 |\n| `a\\|b` | [[目标\\|别名]] |";
        assert_eq!(plain_text(src, &BlockKind::Table), "中文表头  说明\n甲|乙  第一行\n\n第二行\na|b  别名");
    }

    #[test]
    fn plain_text_removes_only_paired_inline_markup() {
        assert_eq!(
            plain_text("__粗体__ _斜体_ ~~删除~~ ==高亮== 2 * 3 snake_case 未闭合`", &BlockKind::Paragraph),
            "粗体 斜体 删除 高亮 2 * 3 snake_case 未闭合`"
        );
    }

    #[test]
    fn plain_text_links_keep_labels_not_destinations() {
        assert_eq!(
            plain_text(
                "[__中文__](https://hidden.test/a_(b) \"secret title\") ![图 *说明*](image.png) [[目标#章节|显示别名]] ![[图片.png|400]]",
                &BlockKind::Paragraph
            ),
            "中文 图 说明 显示别名 图片.png"
        );
    }

    #[test]
    fn plain_text_attributes_do_not_eat_literal_brackets_or_json() {
        assert_eq!(
            plain_text(
                r#"[彩色]{.red} 正文 {#private title="*secret*"} [字面] [] {} {"a": [1, 2]} {1, 2} \{.literal} \[方括号\] ^block-id"#,
                &BlockKind::Paragraph
            ),
            r#"彩色 正文  [字面] [] {} {"a": [1, 2]} {1, 2} {.literal} [方括号]"#
        );
    }

    #[test]
    fn plain_text_nested_json_braces_are_not_attributes() {
        let src = r#"JSON {"pattern": "{.red}", "data": {"n": 1}} 尾部 {.blue}"#;
        assert_eq!(
            plain_text(src, &BlockKind::Paragraph),
            r#"JSON {"pattern": "{.red}", "data": {"n": 1}} 尾部 "#
        );
    }

    #[test]
    fn plain_text_entity_decoded_attributes_remain_visible() {
        for source in ["正文 &#123;.red}", "正文 &#x7b;.red}", "正文 &lbrace;.red}"] {
            assert_eq!(plain_text(source, &BlockKind::Paragraph), "正文 {.red}", "{source}");
            let doc = Document::from_text(source);
            assert_eq!(blocks_for_note(&doc, "n", "n.md", "N")[0].body, "正文 {.red}");
        }
        assert_eq!(
            plain_text(
                r"&#123;.red} **粗体** {.blue} `&#123;.code}` \{.escaped} &#123;#literal} &#94;literal",
                &BlockKind::Paragraph
            ),
            "{.red} 粗体  &#123;.code} {.escaped} {#literal} ^literal"
        );
        assert_eq!(plain_text("&#123;.red}", &BlockKind::Paragraph), "{.red}");
        assert_eq!(plain_text("[正文]&#123;.red}", &BlockKind::Paragraph), "[正文]{.red}");
        for source in ["正文 {&#46;red}", "正文 {.red&#125;", "正文 {.r&#101;d}"] {
            assert_eq!(plain_text(source, &BlockKind::Paragraph), "正文 {.red}", "{source}");
        }
        assert_eq!(plain_text("正文 &#123;.red} {.blue} 尾部", &BlockKind::Paragraph), "正文 {.red}  尾部");
        assert_eq!(plain_text("正文 &#123;.red} ^actual", &BlockKind::Paragraph), "正文 {.red}");
        assert_eq!(
            plain_text("| a\\|b &#123;.red} | {.blue} c |\n| --- | --- |", &BlockKind::Table),
            "a|b {.red}   c"
        );
    }

    #[test]
    fn plain_text_code_and_math_are_literal() {
        assert_eq!(
            plain_text(r"`a * b **raw** {.red} <br> [[x]] \*` $x_{i} * y + \alpha$", &BlockKind::Paragraph),
            r"a * b **raw** {.red} <br> [[x]] \* x_{i} * y + \alpha"
        );
        let code = "````rust\n```\nlet x = [1, 2]; // {.red} **raw**\n~~~~\n````";
        assert_eq!(
            plain_text(code, &BlockKind::CodeFence { info: "rust".into() }),
            "```\nlet x = [1, 2]; // {.red} **raw**\n~~~~"
        );
        assert_eq!(plain_text("    a * b\n    {} []", &BlockKind::CodeBlock), "a * b\n{} []");
    }

    #[test]
    fn plain_text_html_hides_comments_and_scripts_but_separates_breaks() {
        assert_eq!(
            plain_text(
                "中文<br><BR />English<!-- hidden --> <script>secret **code**</script>可见",
                &BlockKind::Paragraph
            ),
            "中文\n\nEnglish 可见"
        );
        assert_eq!(
            plain_text(
                "<div>甲<br>乙</div><style>.hidden { color: red }</style><p>丙 &amp; 丁</p>",
                &BlockKind::HtmlBlock
            ),
            "甲\n乙\n丙 & 丁"
        );
        assert_eq!(plain_text(r"\<br> `<br>` a < b > c", &BlockKind::Paragraph), "<br> <br> a < b > c");
    }

    #[test]
    fn plain_text_removes_block_structure() {
        assert_eq!(plain_text("标题 __粗体__\n===", &BlockKind::Heading { level: 1 }), "标题 粗体");
        assert_eq!(
            plain_text("- [x] __任务__\n- 下一项", &BlockKind::List { ordered: false, start: 1 }),
            "任务\n下一项"
        );
        let kind = BlockKind::Callout {
            kind: crate::block::CalloutKind::Tip,
            title: Some("提示".into()),
            fold: crate::block::Fold::Collapsed,
        };
        assert_eq!(plain_text("> [!tip]- **提示**\n> 正文", &kind), "提示\n正文");
        assert_eq!(plain_text("---\nsecret: hidden\n---", &BlockKind::FrontMatter), "");
    }

    #[test]
    fn plain_text_respects_original_kind_and_literal_extension_markers() {
        assert_eq!(plain_text("1. **标题**<br><br>2. **标题**", &BlockKind::Paragraph), "1. 标题\n\n2. 标题");
        assert_eq!(plain_text("a * b\n    {} [] ^literal", &BlockKind::CodeBlock), "a * b\n{} [] ^literal");
        assert_eq!(plain_text(r"> \[!tip] literal", &BlockKind::BlockQuote), "[!tip] literal");
        assert_eq!(
            plain_text(r"\![[image.png|400]] `![[image.png|400]]`", &BlockKind::Paragraph),
            "![[image.png|400]] ![[image.png|400]]"
        );
        assert_eq!(plain_text(r"正文 \^literal", &BlockKind::Paragraph), "正文 ^literal");
        assert_eq!(plain_text("[[note|{.literal}]]", &BlockKind::Paragraph), "{.literal}");
        let doc = Document::from_text("- [x] __已完成__\n- [ ] 待办\n");
        assert_eq!(
            blocks_for_note(&doc, "n", "n.md", "N").iter().map(|b| b.body.as_str()).collect::<Vec<_>>(),
            ["已完成", "待办"]
        );
    }

    #[test]
    fn plain_text_nested_invisible_html_stays_hidden() {
        assert_eq!(
            plain_text(
                "<template><template>inner</template>secret</template><p>visible</p>",
                &BlockKind::HtmlBlock
            ),
            "visible"
        );
    }

    #[test]
    fn plain_text_html_comparisons_do_not_consume_closing_tags() {
        assert_eq!(
            plain_text("<script>if (a < b) secret();</script><p>a < b > c</p>", &BlockKind::HtmlBlock),
            "a < b > c"
        );
    }

    #[test]
    fn plain_text_long_unpaired_syntax_is_preserved() {
        let src = format!("<div>{}</div>", "&".repeat(20_000));
        assert_eq!(plain_text(&src, &BlockKind::HtmlBlock), "&".repeat(20_000));
        let src = format!("正文 {}", "{".repeat(20_000));
        assert_eq!(plain_text(&src, &BlockKind::Paragraph), src);
    }

    #[test]
    fn invisible_blocks_do_not_change_ord_alignment_or_source() {
        let source = "---\ntitle: Display\naliases: [Alias]\n---\n\n<!-- hidden -->\n\n<script>secret</script>\n\n# __标题__\n\n{.red}\n\n正文 ^block\n\n[x]: https://hidden.test\n";
        let doc = Document::from_text(source);
        let (blocks, _) = blocks_and_links_for_note(&doc, "n", "n.md", "Display");
        assert_eq!(blocks.iter().map(|b| b.body.as_str()).collect::<Vec<_>>(), ["标题", "正文"]);
        let leaves = indexable_leaves(&doc);
        assert_eq!(blocks.len(), leaves.len());
        for (ord, (block, leaf)) in blocks.iter().zip(leaves).enumerate() {
            assert_eq!(block.block_ord, ord as u32);
            assert_eq!(block.pk, IndexBlock::pk_of("n", ord as u32));
            assert_eq!(block.body, plain_text(&doc.slice(doc.content_range(leaf)).to_string(), &leaf.kind));
            assert_eq!(block.title, "Display");
            assert_eq!(&*block.aliases, &["Alias"]);
        }
        assert_eq!(doc.to_string(), source);
    }

    /// Check both extraction entry points and the exact source leaf behind every ord.
    fn assert_visible_leaves(source: &str, expected: &[(&str, &str)]) {
        let doc = Document::from_text(source);
        let blocks = blocks_for_note(&doc, "n", "n.md", "N");
        let (with_links, _) = blocks_and_links_for_note(&doc, "n", "n.md", "N");
        assert_eq!(blocks, with_links);
        assert_eq!(
            blocks.iter().map(|b| b.body.as_str()).collect::<Vec<_>>(),
            expected.iter().map(|(_, body)| *body).collect::<Vec<_>>()
        );
        let leaves = indexable_leaves(&doc);
        assert_eq!(leaves.len(), blocks.len());
        for (ord, ((block, leaf), (prefix, _))) in blocks.iter().zip(leaves).zip(expected).enumerate() {
            assert_eq!(block.block_ord, ord as u32);
            assert_eq!(block.pk, IndexBlock::pk_of("n", ord as u32));
            assert_eq!(doc.content_range(leaf).start, source.find(prefix).unwrap());
        }
        let mut index = MemoryFullTextIndex::new();
        index.upsert(&blocks).unwrap();
        assert!(index.search(&FtsQuery::new("SECRET")).unwrap().is_empty());
        assert_eq!(doc.to_string(), source);
    }

    #[test]
    fn index_hides_script_across_leaf_boundaries() {
        assert_visible_leaves("正文<script>\n\nSECRET\n\n</script>", &[("正文", "正文")]);
        assert_visible_leaves(
            "正文<script>\n\nSECRET\n\n</script>\n\n结尾",
            &[("正文", "正文"), ("结尾", "结尾")],
        );
        assert_visible_leaves("正文<script>\n\nSECRET", &[("正文", "正文")]);
    }

    #[test]
    fn index_hides_style_across_leaf_boundaries() {
        assert_visible_leaves(
            "正文<StYlE>\r\n\r\nSECRET_STYLE\r\n\r\n</sTyLe>\r\n\r\n结尾",
            &[("正文", "正文"), ("结尾", "结尾")],
        );
        assert_visible_leaves("正文<style>\n\nSECRET_STYLE", &[("正文", "正文")]);
    }

    #[test]
    fn index_hidden_html_respects_code_and_escaped_tags() {
        let source = concat!(
            "行内 `<script>` 可见\n\n",
            "\\<script>转义\n\n",
            "&lt;style&gt;实体\n\n",
            "```html\n<script>代码</script>\n```\n\n",
            "    <style>缩进代码</style>\n\n",
            "正文<script>\n\nSECRET **hidden**\n\n",
            "`</script> SECRET_CODE`\n\n",
            "\\</script> SECRET_ESCAPED\n\n",
            "</script>\n\n结尾"
        );
        assert_visible_leaves(
            source,
            &[
                ("行内", "行内 <script> 可见"),
                ("\\<script>", "<script>转义"),
                ("&lt;style&gt;", "<style>实体"),
                ("```html", "<script>代码</script>"),
                ("<style>缩进", "<style>缩进代码</style>"),
                ("正文", "正文"),
                ("结尾", "结尾"),
            ],
        );
    }

    #[test]
    fn blocks_for_note_numbers_indexable_leaves() {
        let doc = Document::from_text(
            "---\ntitle: T\n---\n\n# 标题\n\n第一段 #rust #cli。\n\n---\n\n- 列表项一\n- 列表项二\n\n```\ncode\n```\n\n\n",
        );
        let blocks = blocks_for_note(&doc, "n1", "a.md", "T");
        let bodies: Vec<&str> = blocks.iter().map(|b| b.body.as_str()).collect();
        assert_eq!(bodies, ["标题", "第一段 #rust #cli。", "列表项一", "列表项二", "code"]);
        assert_eq!(blocks.iter().map(|b| b.block_ord).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        assert_eq!(blocks[1].pk, "n1#1");
        assert_eq!(blocks[1].tags, ["rust", "cli"]);
        assert_eq!(blocks[0].title, "T");
        assert_eq!(blocks[0].path, "a.md");
        // ord 与 indexable_leaves 下标一致，能回到块
        let leaves = indexable_leaves(&doc);
        assert_eq!(leaves.len(), 5);
        assert_eq!(doc.slice(doc.content_range(leaves[4])).to_string(), "```\ncode\n```");
    }

    #[test]
    fn vector_search_orders_by_cosine() {
        let mut idx = MemoryVectorIndex::new(2);
        idx.upsert(&[
            VectorItem { pk: "a#0".into(), note_id: "a".into(), block_ord: 0, vector: vec![1.0, 0.0] },
            VectorItem { pk: "a#1".into(), note_id: "a".into(), block_ord: 1, vector: vec![0.0, 1.0] },
            VectorItem { pk: "b#0".into(), note_id: "b".into(), block_ord: 0, vector: vec![0.7, 0.7] },
        ])
        .unwrap();
        let hits = idx.search(&[1.0, 0.1], 2, None).unwrap();
        assert_eq!(hits[0].pk, "a#0");
        assert_eq!(hits[1].pk, "b#0");
        let hits = idx.search(&[1.0, 0.1], 5, Some("b")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(matches!(idx.search(&[1.0], 1, None), Err(IndexError::Dimension { expected: 2, got: 1 })));
    }
}
