//! `FullTextIndex` 的 zvec 实现（ADR 0003 / 方案 10.3）。
//!
//! 一个 vault 一个 Collection，字段：`note_id`、`block_ord`、`path`、`title`、`tags`、`body`（jieba + lowercase 全文索引）。
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
}

impl ZvecFts {
    /// 打开 `dir` 下的索引；不存在则创建。打开失败（格式变了、损坏）时删掉重建——索引随时可以从文件重建（10.1）。
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        ensure_initialized()?;
        let col = if dir.is_dir() && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
            match Collection::open(&dir.to_string_lossy(), Some(&options()?)) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("cannot open fts index at {}: {e}; rebuilding", dir.display());
                    std::fs::remove_dir_all(dir).map_err(|e| IndexError::Engine(e.to_string()))?;
                    create(dir)?
                }
            }
        } else {
            create(dir)?
        };
        Ok(ZvecFts { dir: dir.to_path_buf(), col: Some(col) })
    }

    fn col(&self) -> Result<&Collection, IndexError> {
        self.col.as_ref().ok_or(IndexError::Closed)
    }

    fn query(&self, q: &FtsQuery, output: &[&str]) -> Result<Vec<Doc>, IndexError> {
        let text = q.text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let mut fts = Fts::new().map_err(engine)?;
        // 带 `+term -term "phrase"` 的走布尔表达式，其他按自然语言匹配；相邻词默认 AND
        if is_boolean(text) {
            fts.set_query_string(text).map_err(engine)?;
        } else {
            fts.set_match_string(text).map_err(engine)?;
        }
        let mut sq = SearchQuery::fts("body", &fts, q.limit.clamp(1, 1000) as i32).map_err(engine)?;
        sq.set_fts_params(FtsQueryParams::new(Some("AND")).map_err(engine)?).map_err(engine)?;
        sq.set_output_fields(output).map_err(engine)?;
        if let Some(n) = &q.note_id {
            sq.set_filter(&format!("note_id = '{}'", n.replace('\'', "''"))).map_err(engine)?;
        }
        self.col()?.query(&sq).map_err(engine)
    }
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
