//! `FullTextIndex` 的 zvec 实现（ADR 0003 / 方案 10.3）。
//!
//! 一个 vault 一个 Collection，字段：`note_id`、`block_ord`、`path`、`title`、`tags`、`tags_filter`、`body`（jieba + lowercase 全文索引）。
//! 写路径只用 `upsert` / `delete(pk)`；`delete_by_filter` 会留下僵尸主键（ADR 0003），这里不暴露。
//! zvec 是进程级全局状态，`initialize()` 只做一次；API 同步，调用方放到后台线程。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aero_core::{FtsQuery, FullTextIndex, Hit, IndexBlock, IndexError};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, Fts, FtsQueryParams,
    IndexParams, SearchQuery,
};

const COLLECTION_NAME: &str = "aero_fts";
const OUTPUT_FIELDS: &[&str] = &["note_id", "block_ord"];

fn engine(e: zvec_rust::Error) -> IndexError {
    IndexError::Engine(e.to_string())
}

/// 进程级初始化：`initialize()` 一次，并把 jieba 词典目录固定到裁剪词典。
/// zvec 默认会自动发现 dylib 旁的全词典（349k 词，常驻 ≈150 MB），这里显式覆盖成裁剪词典
/// （~100k 词，≈110 MB，10.3）。查找顺序（第一个含 `jieba.dict.utf8` 的目录胜出）：
/// `ZVEC_JIEBA_DICT_DIR` 环境变量 → 编译期 `ZVEC_LIB_DIR/data/jieba_dict_trimmed`
/// → 可执行文件旁的 `../Resources/jieba_dict`（.app 打包，只装裁剪词典）
/// → `ZVEC_LIB_DIR/data/jieba_dict`（全词典兜底）→ zvec 自动发现的目录。
pub fn ensure_initialized() -> Result<(), IndexError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        zvec_rust::initialize(None).map_err(|e| e.to_string())?;
        if let Some(dir) = jieba_candidates().into_iter().find(|d| d.join("jieba.dict.utf8").is_file()) {
            zvec_rust::set_default_jieba_dict_dir(&dir.to_string_lossy()).map_err(|e| e.to_string())?;
        }
        let dict = zvec_rust::get_default_jieba_dict_dir();
        if dict.is_empty() {
            return Err("jieba dictionary not found (set ZVEC_JIEBA_DICT_DIR)".to_owned());
        }
        log::info!("zvec initialized, jieba dict: {dict}");
        Ok(())
    })
    .clone()
    .map_err(IndexError::Engine)
}

fn jieba_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("ZVEC_JIEBA_DICT_DIR") {
        out.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("../Resources/jieba_dict"));
    }
    if let Some(lib) = option_env!("ZVEC_LIB_DIR") {
        out.push(Path::new(lib).join("data").join("jieba_dict_trimmed"));
        out.push(Path::new(lib).join("data").join("jieba_dict"));
    }
    out
}

pub struct ZvecFts {
    dir: PathBuf,
    col: Option<Collection>,
    recreated: bool,
}

impl ZvecFts {
    /// 打开 `dir` 下的索引；不存在则创建。打开失败（格式变了、损坏）时删掉重建——索引随时可以从文件重建（10.1）。
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        ensure_initialized()?;
        let (col, recreated) =
            if dir.is_dir() && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
                match Collection::open(&dir.to_string_lossy(), Some(&options()?)) {
                    Ok(c) => (c, false),
                    Err(e) => {
                        log::warn!("cannot open fts index at {}: {e}; rebuilding", dir.display());
                        std::fs::remove_dir_all(dir).map_err(|e| IndexError::Engine(e.to_string()))?;
                        (create(dir)?, true)
                    }
                }
            } else {
                (create(dir)?, true)
            };
        Ok(ZvecFts { dir: dir.to_path_buf(), col: Some(col), recreated })
    }

    pub(crate) fn was_recreated(&self) -> bool {
        self.recreated
    }

    fn col(&self) -> Result<&Collection, IndexError> {
        self.col.as_ref().ok_or(IndexError::Closed)
    }

    fn query(&self, q: &FtsQuery, output: &[&str]) -> Result<Vec<Doc>, IndexError> {
        let text = q.text.trim();
        if text.is_empty() && q.tags.is_empty() {
            return Ok(Vec::new());
        }
        let topk = q.limit.clamp(1, 1000) as i32;
        let mut sq = if text.is_empty() {
            scalar_query(topk)?
        } else {
            let mut fts = Fts::new().map_err(engine)?;
            // 带 `+term -term "phrase"` 的走布尔表达式，其他按自然语言匹配；相邻词默认 AND
            if is_boolean(text) {
                fts.set_query_string(text).map_err(engine)?;
            } else {
                fts.set_match_string(text).map_err(engine)?;
            }
            let mut sq = SearchQuery::fts("body", &fts, topk).map_err(engine)?;
            sq.set_fts_params(FtsQueryParams::new(Some("AND")).map_err(engine)?).map_err(engine)?;
            sq
        };
        sq.set_output_fields(output).map_err(engine)?;
        let mut filters: Vec<_> =
            q.tags.iter().map(|tag| format!("tags_filter LIKE '%{}%'", encode_tag(tag))).collect();
        if text.is_empty() {
            // 纯标签查询：标签是笔记级的，每篇只取第一块，否则长笔记会吃满 topk（FtsQuery 文档）
            filters.push("block_ord = 0".to_owned());
        }
        if let Some(n) = &q.note_id {
            filters.push(format!("note_id = {}", sql_literal(n)?));
        }
        if !filters.is_empty() {
            sq.set_filter(&filters.join(" AND ")).map_err(engine)?;
        }
        self.col()?.query(&sq).map_err(engine)
    }
}

fn scalar_query(topk: i32) -> Result<SearchQuery, IndexError> {
    use zvec_rust::sys;

    // SAFETY: The C constructor has no preconditions; its result is checked below.
    let handle = unsafe { sys::zvec_vector_query_create() };
    if handle.is_null() {
        return Err(IndexError::Engine("failed to create scalar query".into()));
    }
    // SAFETY: The valid C handle is transferred exactly once; Drop frees it even on errors.
    let query = unsafe { SearchQuery::from_raw(handle) };
    // SAFETY: The handle is still owned by the live query.
    let code = unsafe { sys::zvec_vector_query_set_topk(handle, topk) };
    if code != sys::ZVEC_OK {
        let mut message = std::ptr::null_mut();
        // SAFETY: The API writes an allocated error string to this valid out-pointer.
        unsafe { sys::zvec_get_last_error(&mut message) };
        let message = if message.is_null() {
            "unknown error".to_owned()
        } else {
            // SAFETY: The C error is NUL-terminated and copied before freeing it.
            let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned();
            // SAFETY: The error string was allocated by zvec and must be freed with zvec_free.
            unsafe { sys::zvec_free(message.cast()) };
            text
        };
        return Err(engine(zvec_rust::Error { code: code.into(), message }));
    }
    Ok(query)
}

fn encode_tag(tag: &str) -> String {
    let hex: String = tag.to_lowercase().bytes().map(|byte| format!("{byte:02x}")).collect();
    format!("|{hex}|")
}

fn sql_literal(value: &str) -> Result<String, IndexError> {
    // zvec decodes backslash-escaped quotes only; doubling other backslashes changes the value.
    'delimiter: for quote in ['\'', '"'] {
        let mut literal = String::from(quote);
        let mut backslashes = 0;
        for c in value.chars() {
            if c == quote && backslashes % 2 == 1 {
                continue 'delimiter;
            }
            if c == quote || ((c == '\'' || c == '"') && backslashes > 0) {
                literal.push('\\');
            }
            literal.push(c);
            backslashes = if c == '\\' { backslashes + 1 } else { 0 };
        }
        if backslashes % 2 == 0 {
            literal.push(quote);
            return Ok(literal);
        }
    }
    Err(IndexError::Engine("note_id cannot be represented by zvec SQL string escaping".into()))
}

fn is_boolean(text: &str) -> bool {
    text.contains('"')
        || text.split_whitespace().any(|t| t.len() > 1 && (t.starts_with('+') || t.starts_with('-')))
}

fn options() -> Result<CollectionOptions, IndexError> {
    CollectionOptions::new().map_err(engine)
}

fn create(dir: &Path) -> Result<Collection, IndexError> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| IndexError::Engine(e.to_string()))?;
    }
    let fts = IndexParams::fts(Some("jieba"), Some(&["lowercase"]), None).map_err(engine)?;
    let schema = CollectionSchema::builder(COLLECTION_NAME)
        .add_field(FieldSchema::new("note_id", DataType::String, false, 0).map_err(engine)?)
        .add_field(FieldSchema::new("block_ord", DataType::Int32, false, 0).map_err(engine)?)
        .add_field(FieldSchema::new("path", DataType::String, false, 0).map_err(engine)?)
        .add_field(FieldSchema::new("title", DataType::String, false, 0).map_err(engine)?)
        .add_field(FieldSchema::new("tags", DataType::String, false, 0).map_err(engine)?)
        .add_field(FieldSchema::new("tags_filter", DataType::String, false, 0).map_err(engine)?)
        .add_indexed_field("body", DataType::String, fts)
        .build()
        .map_err(engine)?;
    Collection::create_and_open(&dir.to_string_lossy(), &schema, Some(&options()?)).map_err(engine)
}

fn to_doc(b: &IndexBlock) -> Result<Doc, IndexError> {
    let mut d = Doc::new().map_err(engine)?;
    d.set_pk(&b.pk);
    d.add_string("note_id", &b.note_id).map_err(engine)?;
    d.add_i32("block_ord", b.block_ord as i32).map_err(engine)?;
    d.add_string("path", &b.path).map_err(engine)?;
    d.add_string("title", &b.title).map_err(engine)?;
    d.add_string("tags", &b.tags.join(" ")).map_err(engine)?;
    let tags_filter: String = b.tags.iter().map(|tag| encode_tag(tag)).collect();
    d.add_string("tags_filter", &tags_filter).map_err(engine)?;
    d.add_string("body", &b.body).map_err(engine)?;
    Ok(d)
}

fn from_doc(d: &Doc) -> Result<IndexBlock, IndexError> {
    let s = |name: &str| d.get_string(name).map_err(engine).map(|v| v.unwrap_or_default());
    let tags: Vec<String> = s("tags")?.split_whitespace().map(str::to_owned).collect();
    Ok(IndexBlock {
        pk: d.get_pk().unwrap_or_default().to_owned(),
        note_id: s("note_id")?,
        block_ord: d.get_i32("block_ord").map_err(engine)?.unwrap_or_default() as u32,
        path: s("path")?,
        title: s("title")?,
        body: s("body")?,
        tags,
    })
}

impl FullTextIndex for ZvecFts {
    fn upsert(&mut self, blocks: &[IndexBlock]) -> Result<(), IndexError> {
        if blocks.is_empty() {
            return Ok(());
        }
        let docs: Vec<Doc> = blocks.iter().map(to_doc).collect::<Result<_, _>>()?;
        let refs: Vec<&Doc> = docs.iter().collect();
        let r = self.col()?.upsert(&refs).map_err(engine)?;
        if r.error_count > 0 {
            let first = r.results.iter().find(|x| !x.success).map(|x| x.message.clone()).unwrap_or_default();
            return Err(IndexError::Engine(format!(
                "upsert: {} of {} failed: {first}",
                r.error_count,
                blocks.len()
            )));
        }
        Ok(())
    }

    fn delete(&mut self, pks: &[String]) -> Result<(), IndexError> {
        if pks.is_empty() {
            return Ok(());
        }
        let refs: Vec<&str> = pks.iter().map(String::as_str).collect();
        // 不存在的 pk：整体调用可能直接报 NotFound，也可能在逐条结果里报；都当作已删除
        match self.col()?.delete(&refs) {
            Ok(r) => {
                let bad = r
                    .results
                    .iter()
                    .find(|x| !x.success && x.code != zvec_rust::ErrorCode::NotFound)
                    .map(|x| x.message.clone());
                match bad {
                    Some(msg) => Err(IndexError::Engine(format!("delete: {msg}"))),
                    None => Ok(()),
                }
            }
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(engine(e)),
        }
    }

    fn search(&self, q: &FtsQuery) -> Result<Vec<Hit>, IndexError> {
        let docs = self.query(q, OUTPUT_FIELDS)?;
        docs.iter()
            .map(|d| {
                Ok(Hit {
                    pk: d.get_pk().unwrap_or_default().to_owned(),
                    note_id: d.get_string("note_id").map_err(engine)?.unwrap_or_default(),
                    block_ord: d.get_i32("block_ord").map_err(engine)?.unwrap_or_default() as u32,
                    score: d.get_score(),
                })
            })
            .collect()
    }

    fn fetch(&self, pks: &[String]) -> Result<Vec<IndexBlock>, IndexError> {
        if pks.is_empty() {
            return Ok(Vec::new());
        }
        let refs: Vec<&str> = pks.iter().map(String::as_str).collect();
        let docs = self.col()?.fetch(&refs).map_err(engine)?;
        docs.iter().map(from_doc).collect()
    }

    fn flush(&mut self) -> Result<(), IndexError> {
        self.col()?.flush().map_err(engine)
    }

    fn optimize(&mut self) -> Result<(), IndexError> {
        self.col()?.optimize().map_err(engine)
    }

    /// 全量重建：关掉、删目录、重建空 Collection。
    fn clear(&mut self) -> Result<(), IndexError> {
        if let Some(col) = self.col.take() {
            col.close().map_err(engine)?;
        }
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir).map_err(|e| IndexError::Engine(e.to_string()))?;
        }
        self.col = Some(create(&self.dir)?);
        Ok(())
    }

    fn len(&self) -> usize {
        self.col.as_ref().and_then(|c| c.stats().ok()).map_or(0, |s| s.doc_count as usize)
    }
}

impl std::fmt::Debug for ZvecFts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZvecFts").field("dir", &self.dir).field("len", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(note: &str, ord: u32, body: &str) -> IndexBlock {
        IndexBlock {
            pk: IndexBlock::pk_of(note, ord),
            note_id: note.into(),
            block_ord: ord,
            path: format!("{note}.md"),
            title: note.to_uppercase(),
            body: body.into(),
            tags: vec![],
        }
    }

    fn tagged_block(note: &str, body: &str, tags: &[&str]) -> IndexBlock {
        let mut b = block(note, 0, body);
        b.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        b
    }

    fn tag_query(text: &str, tags: &[&str]) -> FtsQuery {
        let mut q = FtsQuery::new(text);
        q.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        q
    }

    fn hit_pks(idx: &ZvecFts, q: &FtsQuery) -> Vec<String> {
        let mut pks: Vec<_> = idx.search(q).unwrap().into_iter().map(|hit| hit.pk).collect();
        pks.sort();
        pks
    }

    #[test]
    fn tag_only_requires_all_complete_case_insensitive_tags() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        idx.upsert(&[
            tagged_block("both", "needle", &["RuSt", "CLI", "ÉTÉ/学习"]),
            tagged_block("rust", "needle", &["rust"]),
            tagged_block("nested", "needle", &["rust/web", "cli"]),
            tagged_block("prefix", "needle", &["rusty", "cli"]),
            tagged_block("suffix", "needle", &["trust", "cli"]),
            tagged_block("plain", "rust cli", &[]),
        ])
        .unwrap();

        for text in ["", "  \t", "needle"] {
            assert_eq!(hit_pks(&idx, &tag_query(text, &["RUST"])), ["both#0", "rust#0"]);
            assert_eq!(hit_pks(&idx, &tag_query(text, &["rust", "cLi"])), ["both#0"]);
            assert_eq!(hit_pks(&idx, &tag_query(text, &["RUST/WEB"])), ["nested#0"]);
            assert_eq!(hit_pks(&idx, &tag_query(text, &["été/学习", "CLI"])), ["both#0"]);
        }
        let mut q = tag_query("", &["rust"]);
        q.limit = 1;
        assert_eq!(idx.search(&q).unwrap().len(), 1);
    }

    /// 纯标签查询每篇只回 `block_ord == 0`：一篇 300 块的长笔记不能把 topk 吃满、挤掉别的带标签笔记。
    #[test]
    fn tag_only_returns_one_hit_per_note_before_topk() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        let mut blocks: Vec<IndexBlock> = (0..300)
            .map(|i| {
                let mut b = block("long", i, "needle");
                b.tags = vec!["rust".into()];
                b
            })
            .collect();
        blocks.push(tagged_block("short", "needle", &["rust"]));
        blocks.push(tagged_block("untagged", "needle", &[]));
        idx.upsert(&blocks).unwrap();

        let mut q = tag_query("", &["rust"]);
        q.limit = 10;
        assert_eq!(hit_pks(&idx, &q), ["long#0", "short#0"]);
        // 带文本仍是块级：长笔记的每一块都命中
        q.text = "needle".into();
        q.limit = 1000;
        assert_eq!(idx.search(&q).unwrap().len(), 301);
        // 限定笔记 + 纯标签：只回那一篇的第一块
        q.text.clear();
        q.note_id = Some("long".into());
        assert_eq!(hit_pks(&idx, &q), ["long#0"]);
    }

    #[test]
    fn absent_tags_do_not_match_text_or_untagged_documents() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        idx.upsert(&[
            tagged_block("tagged", "needle missing", &["rust"]),
            tagged_block("plain", "needle missing rust", &[]),
        ])
        .unwrap();
        for text in ["needle", ""] {
            assert!(idx.search(&tag_query(text, &["missing"])).unwrap().is_empty());
            assert!(idx.search(&tag_query(text, &["rust", "missing"])).unwrap().is_empty());
        }
    }

    #[test]
    fn tags_are_literals_not_sql_wildcards_or_injection() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        let tags = [
            "r_st",
            "rust",
            "r%st",
            "_",
            "%",
            "a|b",
            "a",
            "b",
            "x' OR note_id = 'plain' --",
            r"x\' OR 1=1 --",
            "雪",
        ];
        let blocks: Vec<_> = tags
            .iter()
            .enumerate()
            .map(|(i, tag)| tagged_block(&format!("literal{i}"), "needle", &[tag]))
            .collect();
        idx.upsert(&blocks).unwrap();
        idx.upsert(&[block("plain", 0, "needle")]).unwrap();
        for text in ["needle", ""] {
            for (i, tag) in tags.iter().enumerate() {
                assert_eq!(hit_pks(&idx, &tag_query(text, &[tag])), [format!("literal{i}#0")], "{tag:?}");
            }
            assert!(idx.search(&tag_query(text, &["%' OR 1=1 --"])).unwrap().is_empty());
            assert!(idx.search(&tag_query(text, &["a", "b"])).unwrap().is_empty());
        }
    }

    #[test]
    fn tags_compose_with_literal_note_id_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        let notes = ["plain", "o'reilly", r"folder\note", r"x\' OR 1=1 --", "double\"quote"];
        let blocks: Vec<_> = notes
            .iter()
            .enumerate()
            .map(|(i, note)| {
                let mut b = tagged_block(&format!("note{i}"), "needle", &["rust", "cli"]);
                b.note_id = (*note).to_owned();
                b
            })
            .collect();
        idx.upsert(&blocks).unwrap();
        // Same note and text, but missing one required tag.
        let mut other = tagged_block("other", "needle", &["rust"]);
        other.note_id = "plain".into();
        idx.upsert(&[other]).unwrap();
        for text in ["needle", ""] {
            for (i, note) in notes.iter().enumerate() {
                let mut q = tag_query(text, &["RUST", "CLI"]);
                q.note_id = Some((*note).to_owned());
                assert_eq!(hit_pks(&idx, &q), [format!("note{i}#0")]);
                q.text = "absent".into();
                assert!(idx.search(&q).unwrap().is_empty());
            }
            let mut q = tag_query(text, &["rust"]);
            q.note_id = Some("missing' OR 1=1 --".into());
            assert!(idx.search(&q).unwrap().is_empty());
        }
        // Escaping also applies to the existing text-only path.
        let mut q = FtsQuery::new("needle");
        q.note_id = Some(notes[1].into());
        assert_eq!(hit_pks(&idx, &q), ["note1#0"]);
    }

    #[test]
    fn unrepresentable_note_ids_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        idx.upsert(&[tagged_block("plain", "needle", &["rust"])]).unwrap();
        // zvec cannot roundtrip an odd trailing backslash or both quote types after odd backslashes.
        for note in ["trailing\\", "both\\'and\\\""] {
            for text in ["", "needle"] {
                let mut q = tag_query(text, &["rust"]);
                q.note_id = Some(note.into());
                assert!(matches!(idx.search(&q), Err(IndexError::Engine(_))));
            }
        }
    }

    #[test]
    fn text_tag_filter_is_applied_before_topk() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        let mut blocks: Vec<_> = (0..64).map(|i| block(&format!("distractor{i}"), 0, "needle")).collect();
        blocks.push(tagged_block("target", &format!("needle {}", "padding ".repeat(300)), &["rust", "cli"]));
        blocks.push(tagged_block("unrelated", "haystack", &["rust", "cli"]));
        idx.upsert(&blocks).unwrap();

        let mut q = FtsQuery::new("needle");
        q.limit = 1000;
        let unfiltered = idx.search(&q).unwrap();
        let target = unfiltered.iter().find(|hit| hit.pk == "target#0").unwrap();
        assert_eq!(unfiltered.iter().filter(|hit| hit.score > target.score).count(), 64);
        q.limit = 1;
        assert_ne!(idx.search(&q).unwrap()[0].pk, "target#0");
        q.tags = vec!["RUST".into(), "cLi".into()];
        for text in ["needle", "+needle -haystack", "\"needle\""] {
            q.text = text.into();
            assert_eq!(hit_pks(&idx, &q), ["target#0"]);
        }
    }

    #[test]
    fn tag_filter_survives_fetch_upsert_delete_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts");
        let mut idx = ZvecFts::open(&path).unwrap();
        let original = tagged_block("tagged", "needle", &["RuSt", "CLI"]);
        idx.upsert(std::slice::from_ref(&original)).unwrap();
        assert_eq!(idx.fetch(std::slice::from_ref(&original.pk)).unwrap(), std::slice::from_ref(&original));
        assert_eq!(hit_pks(&idx, &tag_query("", &["rust", "cli"])), ["tagged#0"]);
        let docs = idx.col().unwrap().fetch(&["tagged#0"]).unwrap();
        assert_eq!(docs[0].get_string("tags_filter").unwrap().as_deref(), Some("|72757374||636c69|"));

        idx.flush().unwrap();
        drop(idx);
        let mut idx = ZvecFts::open(&path).unwrap();
        assert_eq!(hit_pks(&idx, &tag_query("", &["RUST"])), ["tagged#0"]);
        assert_eq!(idx.fetch(std::slice::from_ref(&original.pk)).unwrap(), std::slice::from_ref(&original));
        idx.upsert(&[tagged_block("tagged", "needle", &[])]).unwrap();
        for text in ["", "needle"] {
            assert!(idx.search(&tag_query(text, &["rust"])).unwrap().is_empty());
        }
        assert!(idx.fetch(&["tagged#0".into()]).unwrap()[0].tags.is_empty());
        assert_eq!(hit_pks(&idx, &FtsQuery::new("needle")), ["tagged#0"]);
        idx.upsert(&[original]).unwrap();
        idx.delete(&["tagged#0".into()]).unwrap();
        for text in ["", "needle"] {
            assert!(idx.search(&tag_query(text, &["rust"])).unwrap().is_empty());
        }
        idx.flush().unwrap();
        drop(idx);
        let idx = ZvecFts::open(&path).unwrap();
        assert!(idx.search(&tag_query("", &["rust"])).unwrap().is_empty());
        assert!(idx.fetch(&["tagged#0".into()]).unwrap().is_empty());
    }

    #[test]
    fn recreation_flag_distinguishes_new_and_reopened_collections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts");
        let mut idx = ZvecFts::open(&path).unwrap();
        assert!(idx.was_recreated());
        idx.upsert(&[tagged_block("tagged", "needle", &["rust"])]).unwrap();
        idx.flush().unwrap();
        drop(idx);
        let idx = ZvecFts::open(&path).unwrap();
        assert!(!idx.was_recreated());
        assert_eq!(hit_pks(&idx, &tag_query("", &["rust"])), ["tagged#0"]);
    }

    #[test]
    fn recreation_flag_is_set_after_recovering_invalid_collection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("invalid"), b"not a collection").unwrap();
        let mut idx = ZvecFts::open(&path).unwrap();
        assert!(idx.was_recreated());
        assert!(!path.join("invalid").exists());
        idx.flush().unwrap();
        drop(idx);
        assert!(!ZvecFts::open(&path).unwrap().was_recreated());
    }

    #[test]
    fn empty_text_without_tags_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        idx.upsert(&[tagged_block("tagged", "needle", &["rust"])]).unwrap();
        for text in ["", "  \n\t"] {
            let mut q = FtsQuery::new(text);
            assert!(idx.search(&q).unwrap().is_empty());
            q.note_id = Some("tagged".into());
            assert!(idx.search(&q).unwrap().is_empty());
        }
    }

    #[test]
    fn upsert_search_fetch_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        assert_eq!(idx.len(), 0);
        idx.upsert(&[
            block("a", 0, "GPUI 的锚点漂移与 Markdown 解析"),
            block("a", 1, "第二段讲 ropey 文本模型"),
            block("b", 0, "另一篇也提到 markdown 与 zvec 索引引擎"),
        ])
        .unwrap();
        assert_eq!(idx.len(), 3);

        // 中文分词 + 大小写不敏感；写入即可见
        let hits = idx.search(&FtsQuery::new("markdown")).unwrap();
        let pks: Vec<&str> = hits.iter().map(|h| h.pk.as_str()).collect();
        assert_eq!(pks.len(), 2);
        assert!(pks.contains(&"a#0") && pks.contains(&"b#0"));
        assert_eq!(idx.search(&FtsQuery::new("索引引擎")).unwrap()[0].pk, "b#0");
        // 相邻词 AND
        assert_eq!(idx.search(&FtsQuery::new("markdown zvec")).unwrap().len(), 1);
        // 限定笔记
        let mut q = FtsQuery::new("markdown");
        q.note_id = Some("a".into());
        assert_eq!(idx.search(&q).unwrap().len(), 1);
        // 布尔：排除
        assert_eq!(idx.search(&FtsQuery::new("markdown -zvec")).unwrap().len(), 1);

        // fetch 拿回正文
        let got = idx.fetch(&["a#1".to_owned(), "missing#9".to_owned()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "第二段讲 ropey 文本模型");
        assert_eq!(got[0].title, "A");
        assert_eq!(got[0].path, "a.md");

        // 覆盖同 pk；删除（含不存在的 pk）
        idx.upsert(&[block("a", 0, "改写后没有那个词")]).unwrap();
        assert_eq!(idx.search(&FtsQuery::new("markdown")).unwrap().len(), 1);
        idx.delete(&["b#0".to_owned(), "nope#0".to_owned()]).unwrap();
        assert!(idx.search(&FtsQuery::new("markdown")).unwrap().is_empty());
        assert_eq!(idx.len(), 2);

        // 重新打开还在；clear 后为空
        idx.flush().unwrap();
        drop(idx);
        let mut idx = ZvecFts::open(&dir.path().join("fts")).unwrap();
        assert_eq!(idx.len(), 2);
        idx.clear().unwrap();
        assert_eq!(idx.len(), 0);
        idx.upsert(&[block("c", 0, "clear 之后还能写")]).unwrap();
        assert_eq!(idx.search(&FtsQuery::new("clear")).unwrap().len(), 1);
    }
}
