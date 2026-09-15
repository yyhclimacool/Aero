//! 增量索引器（方案 10.1）：vault 文件 → 解析 → 块 → 全文索引；元数据进 SQLite。
//!
//! 保存路径按 ADR 0003：`upsert(新块)` + `delete(旧有而新无的 pk)`，旧块数来自 SQLite，不查引擎。
//! 全部方法同步、阻塞，调用方放到后台线程；一次 `sync_all` 大 vault 可能要几秒，通过回调报进度。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aero_core::index::blocks_for_note;
use aero_core::{
    Document, FileStamp, FtsQuery, FullTextIndex, IndexBlock, IndexError, Vault, VaultError, VaultPath,
};

use crate::meta::{Meta, NoteRow};

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("metadata db: {0}")]
    Meta(#[from] rusqlite::Error),
    #[error(transparent)]
    Vault(#[from] VaultError),
}

/// 一条搜索命中：定位到笔记里第 `block_ord` 个可索引块（`aero_core::index::indexable_leaves` 的下标）。
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub path: VaultPath,
    pub title: String,
    pub block_ord: u32,
    /// 块正文（去标记），供摘要与高亮
    pub body: String,
    pub score: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub errors: Vec<String>,
    /// 上次 flush 之后写入的块数（分批 flush 用）
    written: usize,
}

/// [`Indexer::plan_sync`] 的结果。
#[derive(Debug)]
pub struct SyncPlan {
    /// 磁盘上的每篇笔记与已知的元数据（`None` = 新文件）
    pub notes: Vec<(VaultPath, Option<NoteRow>)>,
    /// 元数据里有、磁盘上没有的
    pub removed: Vec<NoteRow>,
}

/// 全量重建时每写多少块 `flush()` 一次（ADR 0003：不分批峰值内存 900 MB，分批 ≈ 280 MB）。
const FLUSH_EVERY_BLOCKS: usize = 10_000;

pub struct Indexer {
    vault: Arc<Vault>,
    meta: Meta,
    fts: Box<dyn FullTextIndex>,
}

impl Indexer {
    /// 打开 `data_dir/<vault_id>/` 下的索引（`meta.sqlite` + `fts/`），没有则建。
    pub fn open(vault: Arc<Vault>, data_dir: &Path) -> Result<Self, IndexerError> {
        let dir = data_dir.join(vault.id());
        let meta = Meta::open(&dir.join("meta.sqlite"))?;
        let fts = crate::zvec_fts::ZvecFts::open(&dir.join("fts"))?;
        Ok(Self::with_parts(vault, meta, Box::new(fts)))
    }

    /// 用给定的元数据库与引擎组装（测试用内存引擎）。
    pub fn with_parts(vault: Arc<Vault>, meta: Meta, fts: Box<dyn FullTextIndex>) -> Self {
        Indexer { vault, meta, fts }
    }

    pub fn vault(&self) -> &Arc<Vault> {
        &self.vault
    }

    pub fn fts(&self) -> &dyn FullTextIndex {
        self.fts.as_ref()
    }

    /// 与磁盘对齐：新文件与指纹变了的文件重索引，消失的文件删掉。`progress(done, total)` 每处理一篇调一次。
    /// 一把锁里跑完；UI 层若想在文件之间放开锁，用 [`Self::plan_sync`] + [`Self::sync_step`] 自己驱动。
    pub fn sync_all(&mut self, mut progress: impl FnMut(usize, usize)) -> SyncReport {
        let mut report = SyncReport::default();
        let plan = match self.plan_sync() {
            Ok(p) => p,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        let total = plan.notes.len();
        for (i, (path, row)) in plan.notes.iter().enumerate() {
            self.sync_step(path, row.as_ref(), &mut report);
            progress(i + 1, total);
        }
        for row in &plan.removed {
            self.remove_step(row, &mut report);
        }
        if let Err(e) = self.fts.flush() {
            report.errors.push(e.to_string());
        }
        report
    }

    /// 全量同步的计划：磁盘上每篇笔记与其已知元数据，以及元数据里有、磁盘上没有的。
    pub fn plan_sync(&self) -> Result<SyncPlan, IndexerError> {
        let notes = self.vault.notes()?;
        let mut known: HashMap<String, NoteRow> =
            self.meta.all_notes()?.into_iter().map(|r| (r.path.clone(), r)).collect();
        let notes = notes
            .into_iter()
            .map(|p| {
                let row = known.remove(p.as_str());
                (p, row)
            })
            .collect();
        Ok(SyncPlan { notes, removed: known.into_values().collect() })
    }

    /// 同步一篇（`plan_sync` 的一项），结果计入 `report`。每写满 [`FLUSH_EVERY_BLOCKS`] 块 flush 一次。
    pub fn sync_step(&mut self, path: &VaultPath, row: Option<&NoteRow>, report: &mut SyncReport) {
        match self.sync_one(path, row) {
            Ok(Some(n)) => {
                report.indexed += 1;
                report.written += n;
                if report.written >= FLUSH_EVERY_BLOCKS {
                    report.written = 0;
                    if let Err(e) = self.fts.flush() {
                        report.errors.push(e.to_string());
                    }
                }
            }
            Ok(None) => report.unchanged += 1,
            Err(e) => report.errors.push(format!("{}: {e}", path.as_str())),
        }
    }

    /// 删掉磁盘上已不存在的一篇（`plan_sync().removed` 的一项）。
    pub fn remove_step(&mut self, row: &NoteRow, report: &mut SyncReport) {
        match self.remove_row(row) {
            Ok(()) => report.removed += 1,
            Err(e) => report.errors.push(format!("{}: {e}", row.path)),
        }
    }

    /// 返回写入的块数；指纹没变返回 `None`。
    fn sync_one(&mut self, path: &VaultPath, row: Option<&NoteRow>) -> Result<Option<usize>, IndexerError> {
        if let Some(row) = row {
            let (mtime, size) = self.vault.stat(path)?;
            if mtime == row.stamp.mtime && size == row.stamp.size {
                return Ok(None);
            }
        }
        let (doc, stamp) = self.vault.read(path)?;
        if let Some(row) = row
            && row.stamp.hash == stamp.hash
        {
            // 内容没变（比如只是 touch）：只更新指纹
            self.meta.upsert_note(&NoteRow { stamp, ..row.clone() })?;
            return Ok(None);
        }
        self.index_document(path, &doc, stamp).map(Some)
    }

    /// 从磁盘读一篇并索引（外部修改 / 新建后调用）。
    pub fn index_note(&mut self, path: &VaultPath) -> Result<usize, IndexerError> {
        let (doc, stamp) = self.vault.read(path)?;
        self.index_document(path, &doc, stamp)
    }

    /// 用刚落盘的源文本索引（编辑器保存后调用，省一次读盘；解析在这里做）。
    pub fn index_text(
        &mut self,
        path: &VaultPath,
        text: &str,
        stamp: FileStamp,
    ) -> Result<usize, IndexerError> {
        self.index_document(path, &Document::from_text(text), stamp)
    }

    /// 用内存里的文档索引。返回写入的块数。
    pub fn index_document(
        &mut self,
        path: &VaultPath,
        doc: &Document,
        stamp: FileStamp,
    ) -> Result<usize, IndexerError> {
        let existing = self.meta.note_by_path(path.as_str())?;
        let note_id =
            existing.as_ref().map_or_else(|| ulid::Ulid::generate().to_string(), |r| r.note_id.clone());
        let old_count = existing.as_ref().map_or(0, |r| r.block_count);
        let title = doc.title_or(path.stem());
        let blocks: Vec<IndexBlock> = blocks_for_note(doc, &note_id, path.as_str(), &title);
        self.fts.upsert(&blocks)?;
        let new_count = blocks.len() as u32;
        if old_count > new_count {
            let gone: Vec<String> =
                (new_count..old_count).map(|ord| IndexBlock::pk_of(&note_id, ord)).collect();
            self.fts.delete(&gone)?;
        }
        self.meta.upsert_note(&NoteRow {
            note_id,
            path: path.as_str().to_owned(),
            title,
            stamp,
            block_count: new_count,
        })?;
        Ok(blocks.len())
    }

    /// 文件被删（移到废纸篓）。
    pub fn remove_note(&mut self, path: &VaultPath) -> Result<(), IndexerError> {
        if let Some(row) = self.meta.note_by_path(path.as_str())? {
            self.remove_row(&row)?;
        }
        Ok(())
    }

    fn remove_row(&mut self, row: &NoteRow) -> Result<(), IndexerError> {
        let pks: Vec<String> = (0..row.block_count).map(|ord| IndexBlock::pk_of(&row.note_id, ord)).collect();
        self.fts.delete(&pks)?;
        self.meta.remove_note(&row.note_id)?;
        Ok(())
    }

    /// 重命名 / 移动：`note_id` 不变；块里存的 `path` / `title` 要跟着改，所以重写这些块。
    pub fn rename_note(&mut self, old: &VaultPath, new: &VaultPath) -> Result<(), IndexerError> {
        let Some(row) = self.meta.note_by_path(old.as_str())? else {
            // 之前没索引过：当新文件处理
            return self.index_note(new).map(|_| ());
        };
        let (doc, stamp) = self.vault.read(new)?;
        let title = doc.title_or(new.stem());
        self.meta.rename_note(&row.note_id, new.as_str(), &title)?;
        // 直接按新路径走一遍保存路径（note_id 已通过 rename 绑定到新路径）
        self.index_document(new, &doc, stamp)?;
        Ok(())
    }

    /// 全文搜索，结果带正文与标题，按分数降序。
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<SearchHit>, IndexerError> {
        let mut q = FtsQuery::new(text);
        q.limit = limit;
        let hits = self.fts.search(&q)?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let pks: Vec<String> = hits.iter().map(|h| h.pk.clone()).collect();
        let docs: HashMap<String, IndexBlock> =
            self.fts.fetch(&pks)?.into_iter().map(|b| (b.pk.clone(), b)).collect();
        let mut out: Vec<SearchHit> = hits
            .iter()
            .filter_map(|h| {
                let b = docs.get(&h.pk)?;
                Some(SearchHit {
                    path: VaultPath::new(&b.path).ok()?,
                    title: b.title.clone(),
                    block_ord: b.block_ord,
                    body: b.body.clone(),
                    score: h.score,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then(a.block_ord.cmp(&b.block_ord))
        });
        Ok(out)
    }

    pub fn flush(&mut self) -> Result<(), IndexerError> {
        Ok(self.fts.flush()?)
    }

    pub fn optimize(&mut self) -> Result<(), IndexerError> {
        Ok(self.fts.optimize()?)
    }

    /// 已索引笔记的 `(路径, 标题)`，给快速打开做候选（标题来自 front matter，没有则是文件名）。
    pub fn note_titles(&self) -> Result<Vec<(VaultPath, String)>, IndexerError> {
        Ok(self
            .meta
            .all_notes()?
            .into_iter()
            .filter_map(|r| VaultPath::new(&r.path).ok().map(|p| (p, r.title)))
            .collect())
    }

    /// 已索引的笔记数 / 块数。
    pub fn stats(&self) -> (u64, usize) {
        (self.meta.note_count().unwrap_or(0), self.fts.len())
    }

    /// 清空并全量重建。
    pub fn rebuild(&mut self, progress: impl FnMut(usize, usize)) -> Result<SyncReport, IndexerError> {
        self.fts.clear()?;
        self.meta.reset()?;
        Ok(self.sync_all(progress))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_core::index::MemoryFullTextIndex;

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Indexer) {
        let dir = tempfile::tempdir().unwrap();
        for (p, body) in files {
            let f = dir.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let ix = Indexer::with_parts(vault, Meta::in_memory().unwrap(), Box::new(MemoryFullTextIndex::new()));
        (dir, ix)
    }

    fn paths(hits: &[SearchHit]) -> Vec<(String, u32)> {
        hits.iter().map(|h| (h.path.as_str().to_owned(), h.block_ord)).collect()
    }

    #[test]
    fn sync_indexes_changes_and_removals() {
        let (dir, mut ix) = setup(&[
            ("a.md", "# Alpha\n\n第一段 anchor\n\n第二段\n"),
            ("sub/b.md", "---\ntitle: Bravo\n---\n\nanchor 在这里\n"),
        ]);
        let mut seen = vec![];
        let r = ix.sync_all(|d, t| seen.push((d, t)));
        assert_eq!((r.indexed, r.unchanged, r.removed), (2, 0, 0));
        assert_eq!(seen, [(1, 2), (2, 2)]);
        assert_eq!(ix.stats(), (2, 4));

        let hits = ix.search("anchor", 10).unwrap();
        assert_eq!(paths(&hits), [("a.md".into(), 1), ("sub/b.md".into(), 0)]);
        assert_eq!(hits[1].title, "Bravo");
        assert_eq!(hits[0].body, "第一段 anchor");

        // 没动：全部 unchanged
        let r = ix.sync_all(|_, _| {});
        assert_eq!((r.indexed, r.unchanged, r.removed), (0, 2, 0));

        // 改 a：块数从 3 变 1，多出来的旧块要删掉；b 删除
        std::fs::write(dir.path().join("a.md"), "只剩一段 anchor\n").unwrap();
        std::fs::remove_file(dir.path().join("sub/b.md")).unwrap();
        // mtime 精度问题：强制指纹不同
        let r = ix.sync_all(|_, _| {});
        assert_eq!((r.indexed, r.removed), (1, 1), "{r:?}");
        assert_eq!(ix.stats(), (1, 1));
        assert_eq!(paths(&ix.search("anchor", 10).unwrap()), [("a.md".into(), 0)]);
        assert!(ix.search("第二段", 10).unwrap().is_empty());
    }

    #[test]
    fn open_with_zvec_engine_and_reopen() {
        let (dir, _) = setup(&[("a.md", "# A\n\nzvec 引擎 roundtrip\n"), ("b.md", "别的\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        {
            let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
            let r = ix.sync_all(|_, _| {});
            assert_eq!((r.indexed, r.unchanged), (2, 0), "{r:?}");
            assert_eq!(paths(&ix.search("引擎", 5).unwrap()), [("a.md".into(), 1)]);
        }
        // 重新打开：元数据与索引都在，什么都不用重建
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        assert_eq!(ix.stats(), (2, 3));
        let r = ix.sync_all(|_, _| {});
        assert_eq!((r.indexed, r.unchanged), (0, 2), "{r:?}");
        assert_eq!(paths(&ix.search("roundtrip", 5).unwrap()), [("a.md".into(), 1)]);
        let r = ix.rebuild(|_, _| {}).unwrap();
        assert_eq!(r.indexed, 2);
        assert_eq!(ix.stats(), (2, 3));
    }

    #[test]
    fn save_rename_remove_paths() {
        let (dir, mut ix) = setup(&[("n.md", "old body\n")]);
        ix.sync_all(|_, _| {});
        // 编辑器保存：用内存文档
        let doc = Document::from_text("new body here\n\nsecond\n");
        std::fs::write(dir.path().join("n.md"), doc.to_string()).unwrap();
        let (_, stamp) = ix.vault().read(&VaultPath::new("n.md").unwrap()).unwrap();
        assert_eq!(ix.index_document(&VaultPath::new("n.md").unwrap(), &doc, stamp).unwrap(), 2);
        assert!(ix.search("old", 5).unwrap().is_empty());
        assert_eq!(ix.search("second", 5).unwrap().len(), 1);

        // 重命名：note_id 不变，路径 / 标题更新
        let (old, new) = (VaultPath::new("n.md").unwrap(), VaultPath::new("dir/m.md").unwrap());
        ix.vault().rename(&old, &new).unwrap();
        ix.rename_note(&old, &new).unwrap();
        let hits = ix.search("second", 5).unwrap();
        assert_eq!(paths(&hits), [("dir/m.md".into(), 1)]);
        assert_eq!(hits[0].title, "m");
        assert_eq!(ix.stats(), (1, 2));

        // 删除
        ix.remove_note(&new).unwrap();
        assert_eq!(ix.stats(), (0, 0));
        assert!(ix.search("second", 5).unwrap().is_empty());
    }
}
