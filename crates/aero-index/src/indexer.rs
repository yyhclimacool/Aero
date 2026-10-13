//! 增量索引器（方案 10.1）：vault 文件 → 解析 → 块 → 全文索引；元数据进 SQLite。
//!
//! 保存路径按 ADR 0003：`upsert(新块)` + `delete(旧有而新无的 pk)`，旧块数来自 SQLite，不查引擎。
//! 全部方法同步、阻塞，调用方放到后台线程；一次 `sync_all` 大 vault 可能要几秒，通过回调报进度。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aero_core::index::blocks_and_links_for_note;
use aero_core::links::{NoteSummary, Resolution, Resolver};
use aero_core::{
    Document, FileStamp, FtsQuery, FullTextIndex, IndexBlock, IndexError, Vault, VaultError, VaultPath,
};

use crate::meta::{Meta, NoteRow};

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error("文档身份冲突: {0}")]
    Identity(String),
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

/// 一次笔记间引用；同一块内多次引用分别列出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backlink {
    pub path: VaultPath,
    pub title: String,
    pub range: std::ops::Range<usize>,
    pub body: String,
    /// 来源块的 Markdown，反链列表按笔记预览渲染这一段。
    pub source: String,
    pub label: String,
    /// 点击前校验来源是否仍是查询时的版本，避免旧偏移定位到其他块。
    pub source_hash: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BacklinkPage {
    pub total: u64,
    pub matched: u64,
    pub hits: Vec<Backlink>,
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

/// [`Indexer::refresh_note`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refresh {
    /// 重新索引了这么多块
    Indexed(usize),
    /// 文件已不存在，索引里的块已删
    Removed,
    /// 指纹没变（或本来就没索引过也不存在）
    Unchanged,
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
/// 5: extract visible Markdown text while preserving code and escaped literals.
const FTS_VERSION: i64 = 5;

/// wikilink 解析用的笔记表：路径 / 文件名 / 标题 → 路径，以及路径 → `note_id`。
/// 从 `notes` 表建一次，之后随每次入库 / 删除 / 改名增量维护，单篇保存不再全表重读、不再走 vault。
///
/// 全量同步开始时（[`Indexer::plan_sync`]）磁盘上的全部路径就登记进来，新文件预分配 `note_id`：
/// 这样逐篇入库时按路径 / 文件名的解析已经是最终结果（不会把还没轮到的同名笔记漏成"唯一"），
/// 收尾只需补挂靠标题解析的少数行——而不是把上百万行读出来再逐行 UPDATE。
struct LinkUniverse {
    resolver: Resolver,
    id_by_path: HashMap<String, String>,
    identities: HashMap<VaultPath, Vec<String>>,
}

impl LinkUniverse {
    fn from_rows(rows: Vec<NoteRow>) -> Self {
        let mut u = LinkUniverse {
            resolver: Resolver::new(),
            id_by_path: HashMap::with_capacity(rows.len()),
            identities: HashMap::new(),
        };
        for row in rows {
            if let Ok(path) = VaultPath::new(&row.path) {
                u.insert(path, &row.note_id, Some(&row.title), &row.aliases);
            }
        }
        u
    }

    /// `title` 为 None：只登记路径（预分配阶段，标题要等读了文件才知道）。
    fn insert(&mut self, path: VaultPath, note_id: &str, title: Option<&str>, aliases: &[String]) {
        self.id_by_path.insert(path.as_str().to_owned(), note_id.to_owned());
        self.resolver.insert(path, title, aliases);
    }

    fn remove(&mut self, path: &VaultPath) {
        self.id_by_path.remove(path.as_str());
        self.identities.remove(path);
        self.resolver.remove(path);
    }

    fn register_identity(&mut self, path: &VaultPath, doc: &aero_core::Document) -> bool {
        let mut keys: Vec<_> = aero_core::identity::read(doc).ok().and_then(|m| m.id).into_iter().collect();
        for (_, block) in doc.blocks().iter() {
            keys.extend(block.persisted_id.iter().map(|id| id.to_string()));
            keys.extend(aero_core::block_refs::aliases(block).map(str::to_owned));
        }
        keys.sort();
        self.resolver.register_identity(path, doc);
        self.identities.insert(path.clone(), keys.clone()).as_ref() != Some(&keys)
    }

    /// 解析到的目标的 `note_id`；歧义 / 不存在都是 None。
    fn resolve_id(&self, target: &str, fragment: Option<&str>, note_dir: &str) -> Option<&str> {
        match self.resolver.resolve(target, fragment, note_dir) {
            Resolution::Resolved { path, .. } => self.id_by_path.get(path.as_str()).map(String::as_str),
            Resolution::Ambiguous { .. } | Resolution::Unresolved => None,
        }
    }

    fn ids_of(&self, paths: &[VaultPath]) -> Vec<String> {
        paths.iter().filter_map(|p| self.id_by_path.get(p.as_str()).cloned()).collect()
    }
}

/// 一次 [`Indexer::reconcile_links`] 之前改动过的笔记：只重判它们邻域里的行（见 [`Resolver::neighbors`]）；
/// 改动太多（全量同步）就退化为重判全部名字有竞争的行。
#[derive(Default)]
struct Touched {
    /// Previous title and aliases; current names are in the resolver.
    notes: Vec<NoteSummary>,
    /// 超过这么多篇就不逐篇算邻域了
    overflow: bool,
}

const TOUCHED_LIMIT: usize = 64;

impl Touched {
    fn push(&mut self, path: VaultPath, old_title: Option<String>, old_aliases: Vec<String>) {
        if self.notes.len() >= TOUCHED_LIMIT {
            self.overflow = true;
        } else {
            self.notes.push(NoteSummary { path, title: old_title.unwrap_or_default(), aliases: old_aliases });
        }
    }
}

pub struct Indexer {
    vault: Arc<Vault>,
    meta: Meta,
    fts: Box<dyn FullTextIndex>,
    /// 懒建；`rebuild` 清库时丢掉。
    links: Option<LinkUniverse>,
    /// 全量同步为还没入库的新文件预分配的 `note_id`（路径 → id），入库时领走。
    preassigned: HashMap<String, String>,
    touched: Touched,
    /// 全量同步进行中（`plan_sync` 到 `finish_sync`）：链接行攒批写。
    bulk: bool,
    /// 攒着还没写进 links 表的 `(note_id, 行)`；读表前必须先 [`Self::flush_link_batch`]。
    link_batch: Vec<(String, Vec<LinkRow>)>,
}

/// 一篇里的一条 wikilink 及其解析到的 `note_id`（`None` = 未解析）。
type LinkRow = (aero_core::links::LinkSpan, Option<String>);

/// 全量同步时每攒多少篇的链接行写一个事务。
const LINK_BATCH_NOTES: usize = 256;

impl Indexer {
    /// 打开 `data_dir/<vault_id>/` 下的索引（`meta.sqlite` + `fts/`），没有则建。
    pub fn open(vault: Arc<Vault>, data_dir: &Path) -> Result<Self, IndexerError> {
        let dir = data_dir.join(vault.id());
        let mut meta = Meta::open(&dir.join("meta.sqlite"))?;
        let version = meta.fts_version()?;
        // open 可能恢复为空集合；提前记为未完成，防止恢复中断后沿用旧指纹。
        meta.set_fts_version(0)?;
        let mut fts = crate::zvec_fts::ZvecFts::open(&dir.join("fts"))?;
        if version != FTS_VERSION || fts.was_recreated() {
            meta.reset()?;
            if !fts.was_recreated() {
                fts.clear()?;
            }
        }
        meta.set_fts_version(FTS_VERSION)?;
        Ok(Self::with_parts(vault, meta, Box::new(fts)))
    }

    /// 用给定的元数据库与引擎组装（测试用内存引擎）。
    pub fn with_parts(vault: Arc<Vault>, meta: Meta, fts: Box<dyn FullTextIndex>) -> Self {
        Indexer {
            vault,
            meta,
            fts,
            links: None,
            preassigned: HashMap::new(),
            touched: Touched::default(),
            bulk: false,
            link_batch: Vec::new(),
        }
    }

    pub fn vault(&self) -> &Arc<Vault> {
        &self.vault
    }

    pub fn fts(&self) -> &dyn FullTextIndex {
        self.fts.as_ref()
    }

    /// 只读取已解析入链；每篇来源最多读取、解析一次。不修改索引或笔记。
    pub fn backlinks(
        &self,
        path: &VaultPath,
        fragment: Option<&str>,
        limit: usize,
    ) -> Result<BacklinkPage, IndexerError> {
        let total = self.meta.backlink_count(path.as_str(), None)?;
        let matched =
            if fragment.is_some() { self.meta.backlink_count(path.as_str(), fragment)? } else { total };
        let rows = self.meta.backlinks(path.as_str(), fragment, limit)?;
        let mut source = None;
        let mut doc = None;
        let mut hits = Vec::new();
        for row in rows {
            let path = VaultPath::new(&row.source.path)?;
            if source.as_ref() != Some(&path) {
                doc = match self.vault.read(&path) {
                    Ok((doc, stamp)) if stamp.hash == row.source.stamp.hash => Some(doc),
                    // 文件监听尚未追上磁盘变化时，不展示位置已经失效的结果。
                    Ok(_) => None,
                    Err(VaultError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                        None
                    }
                    Err(e) => return Err(e.into()),
                };
                source = Some(path.clone());
            }
            let Some(doc) = &doc else { continue };
            let Some(block) = doc.block_at(row.range.start) else { continue };
            let text = doc.slice(row.range.clone()).to_string();
            let raw = doc.slice(doc.content_range(block)).to_string();
            hits.push(Backlink {
                path,
                title: row.source.title,
                range: row.range,
                body: aero_core::index::plain_text(&doc.block_text(block), &block.kind),
                source: snippet_source(&raw),
                label: aero_core::index::plain_text(&text, &aero_core::BlockKind::Paragraph),
                source_hash: row.source.stamp.hash,
            });
        }
        Ok(BacklinkPage { total, matched, hits })
    }
}

/// 反链片段最多保留大约一段的源码，按字符边界截断。
fn snippet_source(text: &str) -> String {
    const MAX: usize = 400;
    let text = text.trim();
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim_end())
}

impl Indexer {
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
        if let Err(e) = self.finish_sync() {
            report.errors.push(e.to_string());
        }
        if let Err(e) = self.fts.flush() {
            report.errors.push(e.to_string());
        }
        report
    }

    /// 全量同步收尾（`plan_sync` + 若干 `sync_step` / `remove_step` 之后调）：写完攒批的链接行，
    /// 补挂靠标题解析的行、重判名字有竞争的行（路径 / 文件名解析在 `plan_sync` 预登记后已是最终结果）。
    pub fn finish_sync(&mut self) -> Result<usize, IndexerError> {
        self.bulk = false;
        self.reconcile_links()
    }

    /// 全量同步的计划：磁盘上每篇笔记与其已知元数据，以及元数据里有、磁盘上没有的。
    /// 同时把磁盘上的全部路径登记进链接解析表（新文件预分配 `note_id`），见 [`LinkUniverse`]。
    pub fn plan_sync(&mut self) -> Result<SyncPlan, IndexerError> {
        Ok(self.plan_sync_cancellable(|| false)?.expect("uncancellable sync plan"))
    }

    /// Cancellation must discard the whole enumeration: a partial file list could
    /// otherwise turn the unvisited notes into deletions from the index.
    pub fn plan_sync_cancellable(
        &mut self,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Option<SyncPlan>, IndexerError> {
        let Some(notes) = self.vault.notes_cancellable(cancelled)? else { return Ok(None) };
        let mut known: HashMap<String, NoteRow> =
            self.meta.all_notes()?.into_iter().map(|r| (r.path.clone(), r)).collect();
        let notes: Vec<(VaultPath, Option<NoteRow>)> = notes
            .into_iter()
            .map(|p| {
                let row = known.remove(p.as_str());
                (p, row)
            })
            .collect();
        // 上次同步中途取消可能留下指向预分配 id 的行；先清掉，别让它们冒充已解析
        self.flush_link_batch()?;
        self.meta.clear_dangling_targets()?;
        self.bulk = true;
        self.universe()?;
        let universe = self.links.as_mut().expect("universe built above");
        for (path, row) in &notes {
            if row.is_none() {
                let id = self
                    .preassigned
                    .entry(path.as_str().to_owned())
                    .or_insert_with(|| ulid::Ulid::generate().to_string());
                if !universe.resolver.contains(path) {
                    universe.insert(path.clone(), id, None, &[]);
                }
            }
        }
        Ok(Some(SyncPlan { notes, removed: known.into_values().collect() }))
    }

    /// 同步一篇（`plan_sync` 的一项），结果计入 `report`。每写满 [`FLUSH_EVERY_BLOCKS`] 块 flush 一次。
    /// 全部步骤走完后调一次 [`Self::reconcile_links`]。
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
            Err(e) => {
                report.errors.push(format!("{}: {e}", path.as_str()));
                if row.is_none() {
                    // 新文件没入库成功：撤掉预登记，指向它的行退回未解析
                    if let Err(e) = self.forget_preassigned(path) {
                        report.errors.push(format!("{}: {e}", path.as_str()));
                    }
                }
            }
        }
    }

    fn forget_preassigned(&mut self, path: &VaultPath) -> Result<(), IndexerError> {
        if let Some(id) = self.preassigned.remove(path.as_str()) {
            self.touched.push(path.clone(), None, Vec::new());
            if let Some(u) = self.links.as_mut() {
                u.remove(path);
            }
            self.flush_link_batch()?;
            self.meta.unresolve_incoming(&id)?;
        }
        Ok(())
    }

    /// 把攒批的链接行写进表（一个事务）。
    fn flush_link_batch(&mut self) -> Result<(), IndexerError> {
        if self.link_batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.link_batch);
        self.meta.replace_links_many(batch.iter().map(|(id, rows)| (id.as_str(), rows.as_slice())))?;
        Ok(())
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
        let n = self.index_document(path, &doc, stamp)?;
        self.reconcile_links()?;
        Ok(n)
    }

    /// 文件监听报告某个路径有动静：与磁盘对齐这一篇——不存在则从索引里删掉，指纹没变则什么都不做
    /// （自己刚保存的文件也会触发监听，这里靠指纹把它筛掉），变了才重新索引。
    pub fn refresh_note(&mut self, path: &VaultPath) -> Result<Refresh, IndexerError> {
        let row = self.meta.note_by_path(path.as_str())?;
        if !self.vault.exists(path) {
            return Ok(match row {
                Some(row) => {
                    self.remove_row(&row)?;
                    // 它走了，之前因与它同名而歧义的行可能又能挂上了
                    self.reconcile_links()?;
                    Refresh::Removed
                }
                None => Refresh::Unchanged,
            });
        }
        Ok(match self.sync_one(path, row.as_ref())? {
            Some(blocks) => {
                // 这篇可能刚入库：它自己的链接行，以及别的笔记指向它的行，现在能挂上了
                self.reconcile_links()?;
                Refresh::Indexed(blocks)
            }
            None => Refresh::Unchanged,
        })
    }

    /// 用刚落盘的源文本索引（编辑器保存后调用，省一次读盘；解析在这里做）。
    pub fn index_text(
        &mut self,
        path: &VaultPath,
        text: &str,
        stamp: FileStamp,
    ) -> Result<usize, IndexerError> {
        let n = self.index_document(path, &Document::from_text(text), stamp)?;
        self.reconcile_links()?;
        Ok(n)
    }

    /// 用内存里的文档索引。返回写入的块数。
    ///
    /// 这篇的链接行整体重建并按当前解析表解析；标题变了则指向它的行退回未解析。
    /// 调用方随后用 [`Self::reconcile_links`] 补挂别的笔记里等着这篇的行、重判名字有竞争的行。
    pub fn index_document(
        &mut self,
        path: &VaultPath,
        doc: &Document,
        stamp: FileStamp,
    ) -> Result<usize, IndexerError> {
        let existing = self.meta.note_by_path(path.as_str())?;
        let formal = aero_core::identity::read(doc).map_err(IndexerError::Identity)?.id;
        if let Some(id) = &formal
            && let Some(owner) = self.meta.note_by_id(id)?
            && owner.path != path.as_str()
            && self.vault.exists(&VaultPath::new(&owner.path)?)
        {
            self.universe()?.resolver.insert(
                path.clone(),
                Some(&doc.title_or(path.stem())),
                &doc.front_matter_aliases(),
            );
            self.universe()?.register_identity(path, doc);
            self.flush_link_batch()?;
            self.meta.unresolve_identity_links()?;
            return Err(IndexerError::Identity(format!("{id} 同时出现在 {} 和 {path}", owner.path)));
        }
        let note_id = match (formal, &existing) {
            (Some(id), _) => id,
            (None, Some(r)) => r.note_id.clone(),
            (None, None) => {
                self.preassigned.remove(path.as_str()).unwrap_or_else(|| ulid::Ulid::generate().to_string())
            }
        };
        if let Some(old) = &existing
            && old.note_id != note_id
        {
            self.flush_link_batch()?;
            self.meta.unresolve_incoming(&old.note_id)?;
            self.fts.delete(
                &(0..old.block_count).map(|ord| IndexBlock::pk_of(&old.note_id, ord)).collect::<Vec<_>>(),
            )?;
            self.meta.remove_note(&old.note_id)?;
        }
        let old_count = existing.as_ref().map_or(0, |r| r.block_count);
        let title = doc.title_or(path.stem());
        let (blocks, spans) = blocks_and_links_for_note(doc, &note_id, path.as_str(), &title);
        self.fts.upsert(&blocks)?;
        let new_count = blocks.len() as u32;
        if old_count > new_count {
            let gone: Vec<String> =
                (new_count..old_count).map(|ord| IndexBlock::pk_of(&note_id, ord)).collect();
            self.fts.delete(&gone)?;
        }
        let tags =
            blocks.first().map(|b| b.tags.clone()).unwrap_or_else(|| aero_core::index::tags_for_note(doc));
        let aliases =
            blocks.first().map(|b| b.aliases.to_vec()).unwrap_or_else(|| doc.front_matter_aliases());
        self.meta.upsert_note_with_tags(
            &NoteRow {
                note_id: note_id.clone(),
                path: path.as_str().to_owned(),
                title: title.clone(),
                aliases: aliases.clone(),
                stamp,
                block_count: new_count,
            },
            &tags,
        )?;
        let names_changed = existing.as_ref().is_some_and(|r| r.title != title || r.aliases != aliases);
        if names_changed {
            // Flush first: queued rows may still point here through removed names.
            // Otherwise bulk flush can restore stale resolutions after invalidation.
            self.flush_link_batch()?;
            self.meta.unresolve_incoming(&note_id)?;
        }
        // Body-only saves do not re-evaluate incoming links.
        if existing.is_none() || names_changed {
            self.touched.push(
                path.clone(),
                existing.as_ref().map(|r| r.title.clone()),
                existing.as_ref().map(|r| r.aliases.clone()).unwrap_or_default(),
            );
        }
        self.universe()?.insert(path.clone(), &note_id, Some(&title), &aliases);
        if self.universe()?.register_identity(path, doc) {
            self.flush_link_batch()?;
            self.meta.unresolve_identity_links()?;
        }
        // 同篇锚点 `[[#h]]` 不是笔记间关系，不进表
        let universe = self.links.as_ref().expect("universe built above");
        let rows: Vec<LinkRow> = spans
            .into_iter()
            .filter(|s| !s.target.is_empty())
            .map(|s| {
                let id =
                    universe.resolve_id(&s.target, s.fragment.as_deref(), path.parent()).map(str::to_owned);
                (s, id)
            })
            .collect();
        let targets = aero_core::attachments::referenced_targets(&doc.to_string());
        self.meta.replace_attachment_targets(&note_id, &targets)?;
        if self.bulk {
            self.link_batch.push((note_id, rows));
            if self.link_batch.len() >= LINK_BATCH_NOTES {
                self.flush_link_batch()?;
            }
        } else {
            self.meta.replace_links_many([(note_id.as_str(), rows.as_slice())])?;
        }
        Ok(blocks.len())
    }

    /// 文件被删（移到废纸篓）。
    pub fn remove_note(&mut self, path: &VaultPath) -> Result<(), IndexerError> {
        if let Some(row) = self.meta.note_by_path(path.as_str())? {
            self.remove_row(&row)?;
            self.reconcile_links()?;
        }
        Ok(())
    }

    fn remove_row(&mut self, row: &NoteRow) -> Result<(), IndexerError> {
        let pks: Vec<String> = (0..row.block_count).map(|ord| IndexBlock::pk_of(&row.note_id, ord)).collect();
        self.fts.delete(&pks)?;
        // 攒批里可能还有这篇的行，先落表再删
        self.flush_link_batch()?;
        self.meta.remove_note(&row.note_id)?;
        self.meta.remove_links(&row.note_id)?;
        // 指向它的行退回未解析：目标重建后会重新挂上，而不是留着一个悬空 id
        self.meta.unresolve_incoming(&row.note_id)?;
        if let Ok(path) = VaultPath::new(&row.path) {
            // Keep old names for the removed note's reconciliation neighborhood.
            self.touched.push(path.clone(), Some(row.title.clone()), row.aliases.clone());
            if let Some(u) = self.links.as_mut() {
                u.remove(&path);
            }
        }
        Ok(())
    }

    /// 重命名 / 移动：`note_id` 不变；块里存的 `path` / `title` 要跟着改，所以重写这些块。
    /// 按旧名字解析到它的链接行退回未解析；调用方随后重索引 `RenamePlan` 已改写的来源笔记。
    pub fn rename_note(&mut self, old: &VaultPath, new: &VaultPath) -> Result<(), IndexerError> {
        let Some(row) = self.meta.note_by_path(old.as_str())? else {
            // 之前没索引过：当新文件处理
            return self.index_note(new).map(|_| ());
        };
        let (doc, stamp) = self.vault.read(new)?;
        let title = doc.title_or(new.stem());
        self.meta.rename_note(&row.note_id, new.as_str(), &title)?;
        self.flush_link_batch()?;
        self.meta.unresolve_incoming(&row.note_id)?;
        self.touched.push(old.clone(), Some(row.title.clone()), row.aliases.clone());
        self.touched.push(new.clone(), None, Vec::new());
        if let Some(u) = self.links.as_mut() {
            u.remove(old);
        }
        // 直接按新路径走一遍保存路径（note_id 已通过 rename 绑定到新路径）
        self.index_document(new, &doc, stamp)?;
        self.reconcile_links()?;
        Ok(())
    }

    /// 重判两类链接行（哈希查找，不遍历笔记）：还没解析到目标的（目标可能刚入库），
    /// 以及指向"判定可能受本次改动影响"的笔记的（唯一可能变歧义、标题可能让位给文件名）——
    /// 单篇场景是改动笔记的邻域，全量同步收尾是全部名字有竞争的笔记。返回改写的行数。
    pub fn reconcile_links(&mut self) -> Result<usize, IndexerError> {
        self.flush_link_batch()?;
        self.universe()?;
        let touched = std::mem::take(&mut self.touched);
        let universe = self.links.as_ref().expect("universe built above");
        // (当前 resolved_note_id, 行)：结果没变的行不写
        let mut rows: Vec<(Option<String>, crate::meta::PendingLink)> =
            self.meta.pending_links()?.into_iter().map(|row| (None, row)).collect();
        let affected: Vec<VaultPath> = if touched.overflow {
            universe.resolver.contested()
        } else {
            let mut v: Vec<VaultPath> = touched
                .notes
                .iter()
                .flat_map(|old| universe.resolver.neighbors(&old.path, Some(&old.title), &old.aliases))
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let affected = universe.ids_of(&affected);
        if !affected.is_empty() {
            rows.extend(self.meta.links_resolved_to(&affected)?.into_iter().map(|(id, row)| (Some(id), row)));
        }
        let mut updates = Vec::new();
        for (current, (note_id, path, target, fragment, byte_start)) in &rows {
            let Ok(path) = VaultPath::new(path) else { continue };
            let resolved = universe.resolve_id(target, fragment.as_deref(), path.parent());
            if resolved != current.as_deref() {
                updates.push((note_id.as_str(), *byte_start, resolved));
            }
        }
        if !updates.is_empty() {
            self.meta.set_links_resolved(updates.iter().copied())?;
        }
        Ok(updates.len())
    }

    /// 给预览点击用的解析表：已索引的笔记（带标题）加上磁盘上还没索引到的（刚新建、全量同步进行中）。
    /// 把命令行里的笔记参数解析成路径：内部 `note_id`、相对路径、唯一文件名、标题或别名。
    pub fn resolve_note(&mut self, target: &str) -> Result<Resolution, IndexerError> {
        let target = target.trim();
        if let Some(row) = self.meta.all_notes()?.into_iter().find(|row| row.note_id == target) {
            return Ok(Resolution::Resolved { path: VaultPath::new(&row.path)?, fragment: None });
        }
        Ok(self.link_resolver()?.resolve(target, None, ""))
    }

    pub fn link_resolver(&mut self) -> Result<Resolver, IndexerError> {
        let mut resolver = self.universe()?.resolver.clone();
        for path in self.vault.notes()? {
            if !resolver.contains(&path) || self.preassigned.contains_key(path.as_str()) {
                match self.vault.read(&path) {
                    Ok((doc, _)) => {
                        resolver.insert(
                            path.clone(),
                            Some(&doc.title_or(path.stem())),
                            &doc.front_matter_aliases(),
                        );
                        resolver.register_identity(&path, &doc);
                    }
                    Err(_) => resolver.insert(path, None, &[]),
                }
            }
        }
        Ok(resolver)
    }

    fn universe(&mut self) -> Result<&mut LinkUniverse, IndexerError> {
        if self.links.is_none() {
            let rows = self.meta.all_notes()?;
            let mut universe = LinkUniverse::from_rows(rows.clone());
            for row in rows {
                let path = VaultPath::new(&row.path)?;
                if let Ok((doc, _)) = self.vault.read(&path) {
                    universe.register_identity(&path, &doc);
                }
            }
            self.links = Some(universe);
        }
        Ok(self.links.as_mut().expect("just built"))
    }

    /// 全文搜索，结果带正文与标题，按分数降序。
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<SearchHit>, IndexerError> {
        let mut q = FtsQuery::parse(text);
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

    /// Indexed catalog with unchanged display titles and alternate lookup names.
    pub fn note_summaries(&self) -> Result<Vec<NoteSummary>, IndexerError> {
        Ok(self
            .meta
            .all_notes()?
            .into_iter()
            .filter_map(|r| {
                VaultPath::new(&r.path).ok().map(|path| NoteSummary {
                    path,
                    title: r.title,
                    aliases: r.aliases,
                })
            })
            .collect())
    }

    pub fn tag_counts(&self, exclude_path: Option<&VaultPath>) -> Result<Vec<crate::TagCount>, IndexerError> {
        Ok(self.meta.tag_counts(exclude_path.map(VaultPath::as_str))?)
    }

    /// 已索引的笔记数 / 块数。
    pub fn stats(&self) -> (u64, usize) {
        (self.meta.note_count().unwrap_or(0), self.fts.len())
    }

    /// 附件被引用的次数。目标存在索引里，解析只用一次附件目录扫描，不再读每篇笔记。
    pub fn attachment_references(&self) -> Result<aero_core::attachments::References, IndexerError> {
        let catalog = aero_core::attachments::Catalog::read(self.vault.as_ref())?;
        Ok(aero_core::attachments::References::from_targets(&catalog, self.attachment_targets()?))
    }

    /// A cheap owned snapshot; directory traversal and resolution can run after
    /// the caller releases the index mutex, including while the index is closing.
    pub fn attachment_targets(&self) -> Result<Vec<(VaultPath, String)>, IndexerError> {
        Ok(self
            .meta
            .attachment_targets()?
            .into_iter()
            .filter_map(|(note, target)| VaultPath::new(&note).ok().map(|note| (note, target)))
            .collect())
    }

    /// 外部把笔记改名或搬走：内容指纹唯一对上时，改写入链并让 note_id 跟着新路径走。
    pub fn adopt_external_moves(&mut self, paths: &[aero_core::VaultPath]) -> Result<(), IndexerError> {
        let mut removed = Vec::new();
        let mut appeared = Vec::new();
        for path in paths {
            if self.vault.exists(path) {
                if self.meta.note_by_path(path.as_str())?.is_none() {
                    appeared.push(path.clone());
                }
            } else if let Some(row) = self.meta.note_by_path(path.as_str())? {
                removed.push(row);
            }
        }
        for row in removed {
            let hash = row.stamp.hash;
            let matched: Vec<_> = appeared
                .iter()
                .filter(|path| self.vault.read(path).ok().is_some_and(|(_, stamp)| stamp.hash == hash))
                .cloned()
                .collect();
            let [to] = matched.as_slice() else { continue };
            let from = aero_core::VaultPath::new(&row.path)?;
            self.rewrite_incoming_links(&row.note_id, to)?;
            self.rename_note(&from, to)?;
        }
        Ok(())
    }

    fn rewrite_incoming_links(
        &mut self,
        note_id: &str,
        to: &aero_core::VaultPath,
    ) -> Result<(), IndexerError> {
        let rows = self.meta.incoming_link_locations(note_id)?;
        let mut by_path: std::collections::HashMap<String, Vec<(String, i64, i64)>> =
            std::collections::HashMap::new();
        for (path, target, start, end) in rows {
            by_path.entry(path).or_default().push((target, start, end));
        }
        let mut sources = Vec::new();
        for (path, spans) in by_path {
            let path = aero_core::VaultPath::new(&path)?;
            let (bytes, _) = self.vault.read_bytes(&path)?;
            let mut text = String::from_utf8(bytes).map_err(|_| {
                IndexerError::Vault(aero_core::VaultError::Utf8 { path: self.vault.fs_path(&path) })
            })?;
            let mut edits = Vec::new();
            for (target, start, end) in spans {
                let start = start as usize;
                let end = end as usize;
                if end > text.len() || start >= end {
                    continue;
                }
                let span = aero_core::links::LinkSpan {
                    range: start..end,
                    target: target.clone(),
                    alias: None,
                    fragment: None,
                    pointer: None,
                };
                let range = aero_core::links::target_range(&text, &span);
                let replacement = if target.contains('/') {
                    to.as_str().trim_end_matches(".md").to_owned()
                } else {
                    to.stem().to_owned()
                };
                let Some(escaped) = aero_core::links::escape_target(&replacement) else { continue };
                edits.push((range, escaped));
            }
            edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
            for (range, replacement) in edits {
                if range.end <= text.len() && range.start <= range.end {
                    text.replace_range(range, &replacement);
                }
            }
            self.vault.write_bytes(&path, text.as_bytes())?;
            sources.push(path);
        }
        for path in sources {
            self.refresh_note(&path)?;
        }
        Ok(())
    }

    /// 清空并全量重建。
    pub fn rebuild(&mut self, progress: impl FnMut(usize, usize)) -> Result<SyncReport, IndexerError> {
        self.meta.reset()?;
        self.links = None;
        self.preassigned.clear();
        self.touched = Touched::default();
        self.bulk = false;
        self.link_batch.clear();
        self.fts.clear()?;
        self.meta.set_fts_version(FTS_VERSION)?;
        Ok(self.sync_all(progress))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_core::index::MemoryFullTextIndex;

    #[test]
    fn identity_links_and_backlinks_follow_a_block_move_and_rebuild() {
        let a = aero_core::identity::new_document_id();
        let b = aero_core::identity::new_document_id();
        let block = aero_core::identity::new_block_id();
        let text = |id: &str, body: &str| format!("---\naero:\n  id: {id}\n---\n\n{body}\n");
        let (dir, mut ix) = setup(&[
            ("a.md", &text(&a, &format!("source ^{block}"))),
            ("b.md", &text(&b, "destination")),
            ("ref.md", &format!("[[id:{a}#^{block}|ref]]\n")),
        ]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        assert_eq!(ix.meta.links_rows().unwrap()[0].1.as_deref(), Some(a.as_str()));
        std::fs::write(dir.path().join("a.md"), text(&a, "remaining")).unwrap();
        std::fs::write(dir.path().join("b.md"), text(&b, &format!("moved ^{block}"))).unwrap();
        ix.refresh_note(&VaultPath::new("a.md").unwrap()).unwrap();
        ix.refresh_note(&VaultPath::new("b.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap()[0].1.as_deref(), Some(b.as_str()));
        assert!(ix.rebuild(|_, _| {}).unwrap().errors.is_empty());
        assert_eq!(ix.meta.links_rows().unwrap()[0].1.as_deref(), Some(b.as_str()));
        assert!(
            matches!(ix.link_resolver().unwrap().resolve(&format!("id:{a}"), Some(&format!("^{block}")), ""),
            Resolution::Resolved { path, .. } if path.as_str() == "b.md")
        );
    }

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
    fn cancelled_sync_plan_preserves_notes_links_and_later_writes() {
        let (_dir, mut ix) = setup(&[("a.md", "[[b]]\n"), ("b.md", "body\n"), ("c.md", "third\n")]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let before = ix.meta.links_rows().unwrap();
        let mut visited = 0;
        assert!(
            ix.plan_sync_cancellable(|| {
                visited += 1;
                visited == 3
            })
            .unwrap()
            .is_none()
        );
        assert_eq!(visited, 3);
        assert_eq!(ix.meta.links_rows().unwrap(), before);
        assert_eq!(ix.note_summaries().unwrap().len(), 3);
        assert!(!ix.bulk, "cancelled enumeration must not start a bulk update");
        ix.index_note(&VaultPath::new("b.md").unwrap()).unwrap();
        assert_eq!(ix.backlinks(&VaultPath::new("b.md").unwrap(), None, 10).unwrap().total, 1);
    }

    #[test]
    fn alias_links_reconcile_edits_collisions_removal_and_deletion() {
        for order in [["a.md", "b.md", "source.md"], ["source.md", "b.md", "a.md"]] {
            let (dir, mut ix) = setup(&[
                ("a.md", "---\naliases: [shared, old]\n---\nbody\n"),
                ("b.md", "body\n"),
                ("source.md", "[[shared]] [[old]] [[new]]\n"),
            ]);
            for name in order {
                ix.index_note(&VaultPath::new(name).unwrap()).unwrap();
            }
            let a = VaultPath::new("a.md").unwrap();
            let b = VaultPath::new("b.md").unwrap();
            assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits.len(), 2);
            assert!(
                ix.backlinks(&a, None, 20).unwrap().hits.iter().any(|hit| hit.source.contains("[[shared]]")),
                "反链片段保留来源块的 Markdown"
            );
            std::fs::write(dir.path().join("b.md"), "---\ntitle: shared\n---\nbody\n").unwrap();
            ix.index_note(&b).unwrap();
            assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits.len(), 1);
            assert!(ix.backlinks(&b, None, 20).unwrap().hits.is_empty());
            std::fs::write(dir.path().join("a.md"), "---\naliases: [new]\n---\nbody\n").unwrap();
            ix.index_note(&a).unwrap();
            assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits[0].label, "new");
            assert_eq!(ix.backlinks(&b, None, 20).unwrap().hits[0].label, "shared");
            std::fs::remove_file(dir.path().join("a.md")).unwrap();
            ix.remove_note(&a).unwrap();
            assert!(ix.backlinks(&a, None, 20).unwrap().hits.is_empty());
            assert_eq!(ix.meta.pending_links().unwrap().len(), 2);
        }
    }

    #[test]
    fn external_rename_follows_the_file_hash_and_rewrites_the_link() {
        let (dir, mut ix) = setup(&[("a.md", "见 [[b]]\n"), ("b.md", "目标\n")]);
        ix.sync_all(|_, _| {});
        std::fs::rename(dir.path().join("b.md"), dir.path().join("c.md")).unwrap();
        ix.adopt_external_moves(&[
            aero_core::VaultPath::new("b.md").unwrap(),
            aero_core::VaultPath::new("c.md").unwrap(),
        ])
        .unwrap();
        let text = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
        assert!(text.contains("[[c]]"), "{text}");
        assert!(ix.meta.note_by_path("c.md").unwrap().is_some());
        assert!(ix.meta.note_by_path("b.md").unwrap().is_none());
    }

    #[test]
    fn alias_batch_overflow_does_not_reintroduce_old_resolutions() {
        let (dir, mut ix) = setup(&[("target.md", "---\naliases: oldname\n---\nbody\n")]);
        ix.index_note(&VaultPath::new("target.md").unwrap()).unwrap();
        assert!(matches!(
            ix.link_resolver().unwrap().resolve("oldname", None, ""),
            Resolution::Resolved { .. }
        ));
        for i in 0..TOUCHED_LIMIT + 2 {
            std::fs::write(dir.path().join(format!("a{i:03}.md")), "[[oldname]]\n").unwrap();
        }
        std::fs::write(dir.path().join("target.md"), "---\naliases: newname\n---\nbody\n").unwrap();
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        assert!(ix.backlinks(&VaultPath::new("target.md").unwrap(), None, 200).unwrap().hits.is_empty());
        assert_eq!(ix.meta.pending_links().unwrap().len(), TOUCHED_LIMIT + 2);
    }

    #[test]
    fn alias_collisions_in_large_sync_resolve_after_removing_a_competitor() {
        let (dir, mut ix) =
            setup(&[("a.md", "---\naliases: shared\n---\nbody\n"), ("source.md", "[[shared]]\n")]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let a = VaultPath::new("a.md").unwrap();
        assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits.len(), 1);
        for i in 0..TOUCHED_LIMIT + 2 {
            std::fs::write(dir.path().join(format!("b{i:03}.md")), "body\n").unwrap();
        }
        std::fs::write(dir.path().join("z.md"), "---\naliases: shared\n---\nbody\n").unwrap();
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        assert!(ix.backlinks(&a, None, 20).unwrap().hits.is_empty());
        assert_eq!(ix.meta.pending_links().unwrap().len(), 1);
        std::fs::remove_file(dir.path().join("z.md")).unwrap();
        ix.remove_note(&VaultPath::new("z.md").unwrap()).unwrap();
        assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits.len(), 1);
        let new = VaultPath::new("renamed.md").unwrap();
        ix.vault.rename(&a, &new).unwrap();
        ix.rename_note(&a, &new).unwrap();
        assert_eq!(ix.backlinks(&new, None, 20).unwrap().hits.len(), 1);
        assert_eq!(
            ix.note_summaries().unwrap().into_iter().find(|n| n.path == new).unwrap().aliases,
            ["shared"]
        );
    }

    #[test]
    fn alias_path_names_yield_to_new_filename_matches_incrementally() {
        let (dir, mut ix) = setup(&[
            ("a.md", "---\naliases: ['nested/target', 'target.md']\n---\nbody\n"),
            ("source.md", "[[nested/target]] [[target.md]]\n"),
        ]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let a = VaultPath::new("a.md").unwrap();
        assert_eq!(ix.backlinks(&a, None, 20).unwrap().hits.len(), 2);
        std::fs::write(dir.path().join("target.md"), "body\n").unwrap();
        let target = VaultPath::new("target.md").unwrap();
        ix.index_note(&target).unwrap();
        assert!(ix.backlinks(&a, None, 20).unwrap().hits.is_empty());
        assert_eq!(ix.backlinks(&target, None, 20).unwrap().hits.len(), 2);
    }

    #[test]
    fn aliases_survive_index_reopen_and_replaced_aliases_leave_search() {
        let (dir, _) = setup(&[("n.md", "---\ntitle: Display\naliases: Quicksilver\n---\nbody\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        ix.flush().unwrap();
        drop(ix);
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        assert_eq!(ix.note_summaries().unwrap()[0].aliases, ["Quicksilver"]);
        assert_eq!(ix.search("quicksilver", 10).unwrap()[0].body, "body");
        assert!(matches!(
            ix.link_resolver().unwrap().resolve("Quicksilver", None, ""),
            Resolution::Resolved { .. }
        ));
        std::fs::write(dir.path().join("n.md"), "---\ntitle: Display\naliases: Replacement\n---\nbody\n")
            .unwrap();
        ix.index_note(&VaultPath::new("n.md").unwrap()).unwrap();
        assert!(ix.search("quicksilver", 10).unwrap().is_empty());
        assert_eq!(ix.search("replacement", 10).unwrap()[0].title, "Display");
    }

    #[test]
    fn unindexed_resolver_reads_front_matter_names() {
        let (_dir, mut ix) = setup(&[("a.md", "---\ntitle: Display\naliases: Quicksilver\n---\nbody\n")]);
        for preassigned in [false, true] {
            if preassigned {
                ix.plan_sync().unwrap();
            }
            let resolver = ix.link_resolver().unwrap();
            for name in ["Display", "Quicksilver"] {
                assert!(
                    matches!(resolver.resolve(name, None, ""), Resolution::Resolved { path, .. } if path.as_str() == "a.md")
                );
            }
        }
    }

    #[test]
    fn backlinks_group_sources_preserve_occurrences_and_skip_non_links() {
        let (dir, mut ix) = setup(&[
            ("target.md", "# Target\n\n[[target]] [[#Target]]\n"),
            ("z.md", "---\ntitle: 来源乙\n---\n\n> 引用 [[target#Target|别名]]\n"),
            (
                "a.md",
                "\u{feff}# 来源甲\r\n\r\n中文 [[TARGET]] 与 [[target|同一笔记]]\r\n\r\n`[[target]]` ![[target]] [[missing]]\r\n\r\n```\r\n[[target]]\r\n```\r\n",
            ),
        ]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let target = VaultPath::new("target.md").unwrap();
        let hits = ix.backlinks(&target, None, 20).unwrap().hits;
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["a.md", "a.md", "z.md"]);
        assert_eq!(hits.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(), ["TARGET", "同一笔记", "别名"]);
        assert_eq!(hits[2].title, "来源乙");
        assert!(hits[0].body.contains("中文 TARGET 与 同一笔记"));
        assert!(hits[2].body.contains("引用 别名"));
        for hit in &hits {
            let (doc, stamp) = ix.vault.read(&hit.path).unwrap();
            assert_eq!(hit.source_hash, stamp.hash);
            assert!(doc.slice(hit.range.clone()).to_string().starts_with("[["));
        }
        assert_eq!(ix.backlinks(&target, None, 1).unwrap().hits, hits[..1]);
        assert!(ix.backlinks(&target, None, 0).unwrap().hits.is_empty());
        assert!(ix.backlinks(&VaultPath::new("absent.md").unwrap(), None, 20).unwrap().hits.is_empty());

        // 监听尚未处理的改动 / 删除不能把旧偏移用于新内容。
        std::fs::write(dir.path().join("a.md"), "完全不同的内容\n").unwrap();
        std::fs::remove_file(dir.path().join("z.md")).unwrap();
        assert!(ix.backlinks(&target, None, 20).unwrap().hits.is_empty());
    }

    #[test]
    fn backlinks_follow_source_edits_renames_and_target_resolution() {
        let (dir, mut ix) = setup(&[("a.md", "见 [[target]] 与 [[目标标题]]\n")]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let target = VaultPath::new("target.md").unwrap();
        assert!(ix.backlinks(&target, None, 20).unwrap().hits.is_empty());
        std::fs::write(dir.path().join("target.md"), "---\ntitle: 目标标题\n---\n正文\n").unwrap();
        ix.index_note(&target).unwrap();
        assert_eq!(ix.backlinks(&target, None, 20).unwrap().hits.len(), 2);

        let a = VaultPath::new("a.md").unwrap();
        let renamed = VaultPath::new("renamed.md").unwrap();
        ix.vault.rename(&a, &renamed).unwrap();
        ix.rename_note(&a, &renamed).unwrap();
        assert!(ix.backlinks(&target, None, 20).unwrap().hits.iter().all(|h| h.path == renamed));
        std::fs::write(dir.path().join("renamed.md"), "只有 [[target]]\n").unwrap();
        ix.index_note(&renamed).unwrap();
        assert_eq!(ix.backlinks(&target, None, 20).unwrap().hits.len(), 1);

        std::fs::remove_file(dir.path().join("target.md")).unwrap();
        ix.remove_note(&target).unwrap();
        assert!(ix.backlinks(&target, None, 20).unwrap().hits.is_empty());
        std::fs::write(dir.path().join("target.md"), "重建\n").unwrap();
        ix.index_note(&target).unwrap();
        assert_eq!(ix.backlinks(&target, None, 20).unwrap().hits.len(), 1);
        std::fs::remove_file(dir.path().join("renamed.md")).unwrap();
        ix.remove_note(&renamed).unwrap();
        assert!(ix.backlinks(&target, None, 20).unwrap().hits.is_empty());
    }

    #[test]
    fn backlinks_exclude_ambiguous_targets() {
        let (_dir, mut ix) = setup(&[
            ("a.md", "[[target]] [[x/target]] [[Shared]]\n"),
            ("x/target.md", "---\ntitle: Shared\n---\nX\n"),
            ("y/target.md", "---\ntitle: Shared\n---\nY\n"),
        ]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let hits = ix.backlinks(&VaultPath::new("x/target.md").unwrap(), None, 20).unwrap().hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "x/target");
        assert!(ix.backlinks(&VaultPath::new("y/target.md").unwrap(), None, 20).unwrap().hits.is_empty());
    }

    #[test]
    fn indexing_records_links_and_removal_clears_them() {
        let (dir, mut ix) =
            setup(&[("a.md", "见 [[b]] 与 [[missing]]，还有 [[b#标题]]。\n"), ("b.md", "# B\n")]);
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");

        let b_id: String = ix.meta.note_by_path("b.md").unwrap().unwrap().note_id;
        let rows = ix.meta.links_rows().unwrap();
        assert_eq!(
            rows,
            [
                ("b".to_owned(), Some(b_id.clone()), None),
                ("missing".to_owned(), None, None),
                ("b".to_owned(), Some(b_id.clone()), Some("标题".to_owned())),
            ]
        );

        // 重新保存 a：行随内容更新；[[c]] 的目标还不存在，落库为未解析
        std::fs::write(dir.path().join("a.md"), "见 [[c]]\n").unwrap();
        ix.index_note(&VaultPath::new("a.md").unwrap()).unwrap();
        let rows = ix.meta.links_rows().unwrap();
        assert_eq!(rows, [("c".to_owned(), None, None)]);

        // c 被创建并索引：之前没解析上的行补挂到 c
        std::fs::write(dir.path().join("c.md"), "# C\n").unwrap();
        assert_eq!(ix.refresh_note(&VaultPath::new("c.md").unwrap()).unwrap(), Refresh::Indexed(1));
        let c_id = ix.meta.note_by_path("c.md").unwrap().unwrap().note_id;
        assert_eq!(ix.meta.links_rows().unwrap(), [("c".to_owned(), Some(c_id), None)]);

        // a 被删：它的链接行清掉
        std::fs::remove_file(dir.path().join("a.md")).unwrap();
        assert_eq!(ix.refresh_note(&VaultPath::new("a.md").unwrap()).unwrap(), Refresh::Removed);
        assert!(ix.meta.links_rows().unwrap().is_empty());
    }

    /// 目标被删 / 重建 / 改名 / 改标题时，指向它的行不能留着悬空的旧 id。
    #[test]
    fn incoming_links_follow_target_deletion_recreation_rename_and_retitle() {
        let (dir, mut ix) = setup(&[
            ("a.md", "[[b]] 与 [[乙标题]]\n"),
            ("b.md", "# 乙标题\n"),
            ("t.md", "---\ntitle: 乙标题\n---\n"),
        ]);
        // 标题 "乙标题" 有两篇（b 的一级标题不算 front matter 标题；t 的是）——先确认基本挂接
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");
        let b_id = ix.meta.note_by_path("b.md").unwrap().unwrap().note_id;
        let t_id = ix.meta.note_by_path("t.md").unwrap().unwrap().note_id;
        assert_eq!(
            ix.meta.links_rows().unwrap(),
            [("b".to_owned(), Some(b_id.clone()), None), ("乙标题".to_owned(), Some(t_id.clone()), None)]
        );

        // 删 b：[[b]] 退回未解析（不是悬空 id）
        std::fs::remove_file(dir.path().join("b.md")).unwrap();
        assert_eq!(ix.refresh_note(&VaultPath::new("b.md").unwrap()).unwrap(), Refresh::Removed);
        assert_eq!(ix.meta.links_rows().unwrap()[0], ("b".to_owned(), None, None));

        // 重建 b（新 note_id）：[[b]] 挂到新 id
        std::fs::write(dir.path().join("b.md"), "# B2\n").unwrap();
        ix.refresh_note(&VaultPath::new("b.md").unwrap()).unwrap();
        let b2_id = ix.meta.note_by_path("b.md").unwrap().unwrap().note_id;
        assert_ne!(b2_id, b_id);
        assert_eq!(ix.meta.links_rows().unwrap()[0], ("b".to_owned(), Some(b2_id.clone()), None));

        // b 改名为 c：[[b]] 不再解析（与预览点击的判定一致）
        std::fs::rename(dir.path().join("b.md"), dir.path().join("c.md")).unwrap();
        ix.rename_note(&VaultPath::new("b.md").unwrap(), &VaultPath::new("c.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap()[0], ("b".to_owned(), None, None));

        // t 改标题：靠标题解析的行退回未解析；改回来又挂上
        std::fs::write(dir.path().join("t.md"), "---\ntitle: 别的\n---\n").unwrap();
        ix.index_note(&VaultPath::new("t.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap()[1], ("乙标题".to_owned(), None, None));
        std::fs::write(dir.path().join("t.md"), "---\ntitle: 乙标题\n---\n").unwrap();
        ix.index_note(&VaultPath::new("t.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap()[1], ("乙标题".to_owned(), Some(t_id), None));
    }

    /// 名字有竞争的目标：另一篇拿了同样的标题 → 靠标题挂上的行变歧义；同名文件出现 → 标题让位给文件名；
    /// 竞争者消失 → 挂回来。
    #[test]
    fn contested_names_are_rejudged_after_each_save() {
        let (dir, mut ix) = setup(&[("src.md", "[[甲]]\n"), ("x.md", "---\ntitle: 甲\n---\n")]);
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");
        let x_id = ix.meta.note_by_path("x.md").unwrap().unwrap().note_id;
        assert_eq!(ix.meta.links_rows().unwrap(), [("甲".to_owned(), Some(x_id.clone()), None)]);

        // y 也叫"甲"：歧义，行退回未解析
        std::fs::write(dir.path().join("y.md"), "---\ntitle: 甲\n---\n").unwrap();
        ix.refresh_note(&VaultPath::new("y.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap(), [("甲".to_owned(), None, None)]);

        // y 删掉：又只剩 x，挂回去
        std::fs::remove_file(dir.path().join("y.md")).unwrap();
        ix.refresh_note(&VaultPath::new("y.md").unwrap()).unwrap();
        assert_eq!(ix.meta.links_rows().unwrap(), [("甲".to_owned(), Some(x_id.clone()), None)]);

        // 出现文件 甲.md：文件名优先于标题
        std::fs::write(dir.path().join("甲.md"), "z\n").unwrap();
        ix.refresh_note(&VaultPath::new("甲.md").unwrap()).unwrap();
        let file_id = ix.meta.note_by_path("甲.md").unwrap().unwrap().note_id;
        assert_ne!(file_id, x_id);
        assert_eq!(ix.meta.links_rows().unwrap(), [("甲".to_owned(), Some(file_id), None)]);
    }

    /// 只改正文的保存不重判指向这篇的行（被大量引用的笔记否则每次保存都重判几千行）；
    /// 新建、改标题、重命名、删除才算"动了名字"。
    #[test]
    fn content_only_saves_do_not_rejudge_incoming_links() {
        let (_dir, mut ix) = setup(&[("a.md", "[[b]]\n"), ("b.md", "---\ntitle: B\n---\nold\n")]);
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");
        let b = VaultPath::new("b.md").unwrap();
        let stamp = FileStamp { mtime: std::time::UNIX_EPOCH, size: 0, hash: 0 };

        ix.index_document(&b, &Document::from_text("---\ntitle: B\n---\nnew\n"), stamp).unwrap();
        assert!(ix.touched.notes.is_empty(), "正文改动：{:?}", ix.touched.notes);

        ix.index_document(&b, &Document::from_text("---\ntitle: B2\n---\nnew\n"), stamp).unwrap();
        assert_eq!(
            ix.touched.notes,
            [NoteSummary { path: b.clone(), title: "B".into(), aliases: vec![] }],
            "改标题要重判"
        );
        ix.reconcile_links().unwrap();

        let c = VaultPath::new("c.md").unwrap();
        ix.index_document(&c, &Document::from_text("c\n"), stamp).unwrap();
        assert_eq!(
            ix.touched.notes,
            [NoteSummary { path: c, title: String::new(), aliases: vec![] }],
            "新笔记要重判"
        );
    }

    /// 全量同步中途取消（新文件预分配了 id 却没入库）再重来：不留悬空 id，最终照样挂对。
    #[test]
    fn interrupted_sync_leaves_no_dangling_preassigned_ids() {
        let (_dir, mut ix) = setup(&[("a.md", "[[b]]\n"), ("b.md", "b\n")]);
        let plan = ix.plan_sync().unwrap();
        let mut report = SyncReport::default();
        // 只处理 a：它的 [[b]] 挂到 b 预分配的 id 上，然后"取消"
        let (a_path, a_row) = plan.notes.iter().find(|(p, _)| p.as_str() == "a.md").unwrap();
        ix.sync_step(a_path, a_row.as_ref(), &mut report);
        ix.flush_link_batch().unwrap();
        let rows = ix.meta.links_rows().unwrap();
        let ghost = rows[0].1.clone().expect("挂在预分配 id 上");
        assert!(ix.meta.note_by_path("b.md").unwrap().is_none());

        // 模拟重启：丢掉进程内状态，再来一次完整同步
        ix.links = None;
        ix.preassigned.clear();
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");
        let b_id = ix.meta.note_by_path("b.md").unwrap().unwrap().note_id;
        assert_ne!(b_id, ghost, "重启后是新 id");
        assert_eq!(ix.meta.links_rows().unwrap(), [("b".to_owned(), Some(b_id), None)]);
    }

    /// 全量同步顺序无关：同名两篇即使一篇先入库，也不会被当成"唯一"挂上；大小写不敏感。
    #[test]
    fn full_sync_resolves_links_order_independently_and_case_insensitively() {
        let (_dir, mut ix) = setup(&[
            ("0-first.md", "[[cargo]] [[Notes]]\n"),
            ("a/cargo.md", "x\n"),
            ("z/cargo.md", "y\n"),
            ("notes.md", "n\n"),
        ]);
        let r = ix.sync_all(|_, _| {});
        assert!(r.errors.is_empty(), "{r:?}");
        let notes_id = ix.meta.note_by_path("notes.md").unwrap().unwrap().note_id;
        assert_eq!(
            ix.meta.links_rows().unwrap(),
            [("cargo".to_owned(), None, None), ("Notes".to_owned(), Some(notes_id), None)]
        );
        // 点击用的解析表：歧义照样报出来，并带上磁盘上刚出现、还没索引的文件
        std::fs::write(_dir.path().join("fresh.md"), "").unwrap();
        let resolver = ix.link_resolver().unwrap();
        assert!(matches!(resolver.resolve("cargo", None, ""), Resolution::Ambiguous { .. }));
        assert!(matches!(resolver.resolve("FRESH", None, ""), Resolution::Resolved { .. }));
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

    /// 文件监听的入口：新文件索引、指纹没变跳过、内容变了重索引、文件消失删除。
    #[test]
    fn refresh_note_follows_disk() {
        let (dir, mut ix) = setup(&[("a.md", "anchor one\n")]);
        let a = VaultPath::new("a.md").unwrap();
        let b = VaultPath::new("b.md").unwrap();
        assert_eq!(ix.refresh_note(&a).unwrap(), Refresh::Indexed(1));
        assert_eq!(ix.refresh_note(&a).unwrap(), Refresh::Unchanged);
        // 不存在也没索引过：无事发生
        assert_eq!(ix.refresh_note(&b).unwrap(), Refresh::Unchanged);

        std::fs::write(dir.path().join("b.md"), "anchor two\n\nmore\n").unwrap();
        assert_eq!(ix.refresh_note(&b).unwrap(), Refresh::Indexed(2));
        assert_eq!(ix.stats(), (2, 3));

        // 内容变了（长度不同保证指纹不同）
        std::fs::write(dir.path().join("a.md"), "anchor one changed\n").unwrap();
        assert_eq!(ix.refresh_note(&a).unwrap(), Refresh::Indexed(1));
        assert_eq!(ix.search("changed", 10).unwrap().len(), 1);

        std::fs::remove_file(dir.path().join("b.md")).unwrap();
        assert_eq!(ix.refresh_note(&b).unwrap(), Refresh::Removed);
        assert_eq!(ix.stats(), (1, 1));
        assert_eq!(paths(&ix.search("anchor", 10).unwrap()), [("a.md".into(), 0)]);
    }

    #[test]
    fn tag_directory_tracks_content_front_matter_rename_and_deletion() {
        let (dir, mut ix) = setup(&[
            ("a.md", "---\ntags: [Rust, 中文]\n---\n\nbody #rust #rust/cli\n\nmore #rust\n"),
            ("b.md", "---\ntag: rust\n---\n"),
            ("c.md", "`#ghost` [[x#anchor]]\n"),
        ]);
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        let list = |ix: &Indexer| {
            ix.tag_counts(None).unwrap().into_iter().map(|t| (t.tag, t.note_count)).collect::<Vec<_>>()
        };
        assert_eq!(list(&ix), [("rust".into(), 2), ("rust/cli".into(), 1), ("中文".into(), 1)]);
        let a = VaultPath::new("a.md").unwrap();
        std::fs::write(dir.path().join("a.md"), "body #new\n").unwrap();
        ix.refresh_note(&a).unwrap();
        assert_eq!(list(&ix), [("new".into(), 1), ("rust".into(), 1)]);
        let renamed = VaultPath::new("renamed.md").unwrap();
        ix.vault.rename(&a, &renamed).unwrap();
        ix.rename_note(&a, &renamed).unwrap();
        assert_eq!(ix.tag_counts(Some(&renamed)).unwrap()[0].tag, "rust");
        std::fs::remove_file(dir.path().join("b.md")).unwrap();
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        assert_eq!(list(&ix), [("new".into(), 1)]);
    }

    #[test]
    fn tag_directory_rebuilds_from_schema_three_cache() {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = "---\ntags: [rust]\n---\n\nbody #中文\n";
        std::fs::write(dir.path().join("a.md"), source).unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
        assert!(ix.sync_all(|_, _| {}).errors.is_empty());
        drop(ix);
        let db = rusqlite::Connection::open(data.path().join(vault.id()).join("meta.sqlite")).unwrap();
        db.execute_batch("DROP TABLE note_tags; UPDATE index_state SET version = 3 WHERE name = 'meta';")
            .unwrap();
        drop(db);
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert!(report.errors.is_empty());
        assert_eq!(report.indexed, 1);
        assert_eq!(ix.tag_counts(None).unwrap().len(), 2);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.md")).unwrap(), source);
    }

    #[test]
    fn search_parses_note_wide_tag_filters() {
        let (_dir, mut ix) = setup(&[
            ("a.md", "---\ntags: [rust, cli]\n---\n\nanchor\n\nsecond\n"),
            ("b.md", "anchor #rust/cli\n"),
        ]);
        let report = ix.sync_all(|_, _| {});
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(paths(&ix.search("anchor #RUST tag:#cli", 1).unwrap()), [("a.md".into(), 0)]);
        // 父标签 `rust` 也命中 b.md 的 `rust/cli`；`cli` 不命中 `rust/cli`（层级只向下）
        assert_eq!(paths(&ix.search("anchor #rust", 10).unwrap()), [("a.md".into(), 0), ("b.md".into(), 0)]);
        assert_eq!(paths(&ix.search("anchor #cli", 10).unwrap()), [("a.md".into(), 0)]);
        assert_eq!(ix.search("second #rust", 10).unwrap().len(), 1);
        // 纯标签：每篇只回第一块
        assert_eq!(paths(&ix.search("#rust", 10).unwrap()), [("a.md".into(), 0), ("b.md".into(), 0)]);
        assert_eq!(paths(&ix.search("#rust/cli", 10).unwrap()), [("b.md".into(), 0)]);
        assert!(ix.search("#rust #missing", 10).unwrap().is_empty());
    }

    /// 纯标签查询：一篇长笔记不能挤掉别的带标签笔记（limit 小于长笔记的块数）。
    #[test]
    fn tag_only_search_lists_every_tagged_note_once() {
        let long = format!(
            "---\ntags: [rust]\n---\n\n{}",
            (0..30).map(|i| format!("para {i}\n\n")).collect::<String>()
        );
        let (_dir, mut ix) =
            setup(&[("long.md", &long), ("short.md", "#rust one\n"), ("plain.md", "rust\n")]);
        let report = ix.sync_all(|_, _| {});
        assert!(report.errors.is_empty(), "{report:?}");
        let hits = ix.search("#rust", 5).unwrap();
        assert_eq!(paths(&hits), [("long.md".into(), 0), ("short.md".into(), 0)]);
        // 带文本仍按块返回
        assert_eq!(ix.search("para #rust", 100).unwrap().len(), 30);
    }

    #[test]
    fn legacy_index_rebuilds_unchanged_notes_and_removes_stale_blocks() {
        use zvec_rust::{Collection, CollectionSchema, DataType, Doc, FieldSchema, IndexParams};

        let (dir, _) = setup(&[("a.md", "---\ntags: [rust]\n---\n\nanchor\n\nsecond\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let index_dir = data.path().join(vault.id());
        let path = VaultPath::new("a.md").unwrap();
        let (_, stamp) = vault.read(&path).unwrap();
        let meta = Meta::open(&index_dir.join("meta.sqlite")).unwrap();
        meta.upsert_note(&crate::meta::NoteRow {
            note_id: "legacy".into(),
            path: "a.md".into(),
            title: "a".into(),
            aliases: Vec::new(),
            stamp,
            block_count: 1,
        })
        .unwrap();
        drop(meta);
        crate::zvec_fts::ensure_initialized().unwrap();
        let mut schema = CollectionSchema::builder("aero_fts");
        for name in ["note_id", "path", "title", "tags"] {
            schema = schema.add_field(FieldSchema::new(name, DataType::String, false, 0).unwrap());
        }
        let schema = schema
            .add_field(FieldSchema::new("block_ord", DataType::Int32, false, 0).unwrap())
            .add_indexed_field(
                "body",
                DataType::String,
                IndexParams::fts(Some("jieba"), Some(&["lowercase"]), None).unwrap(),
            )
            .build()
            .unwrap();
        let col =
            Collection::create_and_open(&index_dir.join("fts").to_string_lossy(), &schema, None).unwrap();
        for pk in ["legacy#0", "ghost#0"] {
            let mut doc = Doc::new().unwrap();
            doc.set_pk(pk);
            for (name, value) in
                [("note_id", "legacy"), ("path", "a.md"), ("title", "a"), ("tags", ""), ("body", "outdated")]
            {
                doc.add_string(name, value).unwrap();
            }
            doc.add_i32("block_ord", 0).unwrap();
            assert_eq!(col.upsert(&[&doc]).unwrap().error_count, 0);
        }
        col.flush().unwrap();
        drop(col);

        let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert_eq!((report.indexed, report.unchanged), (1, 0), "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(ix.stats(), (1, 2));
        assert_eq!(paths(&ix.search("#rust", 10).unwrap()), [("a.md".into(), 0)]);
        assert_eq!(ix.search("second #rust", 10).unwrap().len(), 1);
        assert!(ix.search("outdated", 10).unwrap().is_empty());
        assert!(ix.fts().fetch(&["legacy#0".into(), "ghost#0".into()]).unwrap().is_empty());
        drop(ix);
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert_eq!((report.indexed, report.unchanged), (0, 1));
        assert_eq!(ix.search("anchor #rust", 1).unwrap().len(), 1);
    }

    #[test]
    fn recreated_fts_reindexes_unchanged_notes() {
        let (dir, _) = setup(&[("a.md", "anchor #rust\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
        assert_eq!(ix.sync_all(|_, _| {}).indexed, 1);
        drop(ix);
        std::fs::remove_dir_all(data.path().join(vault.id()).join("fts")).unwrap();
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert_eq!((report.indexed, report.unchanged), (1, 0), "{report:?}");
        assert_eq!(ix.search("anchor #rust", 1).unwrap().len(), 1);
    }

    #[test]
    fn failed_fts_open_keeps_rebuild_pending() {
        let (dir, _) = setup(&[("a.md", "anchor #rust\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault.clone(), data.path()).unwrap();
        assert_eq!(ix.sync_all(|_, _| {}).indexed, 1);
        drop(ix);
        let index_dir = data.path().join(vault.id());
        let fts_dir = index_dir.join("fts");
        std::fs::remove_dir_all(&fts_dir).unwrap();
        std::fs::write(&fts_dir, "not a directory").unwrap();
        assert!(Indexer::open(vault.clone(), data.path()).is_err());
        assert_eq!(Meta::open(&index_dir.join("meta.sqlite")).unwrap().fts_version().unwrap(), 0);
        std::fs::remove_file(&fts_dir).unwrap();
        drop(crate::zvec_fts::ZvecFts::open(&fts_dir).unwrap());
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert_eq!((report.indexed, report.unchanged), (1, 0), "{report:?}");
        assert_eq!(ix.search("#rust", 10).unwrap().len(), 1);
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

    /// zvec 引擎：父标签命中子标签（过滤字段里带祖先）、`-#tag` 排除（取回后筛）、尾标点与 `tag:x`。
    #[test]
    fn zvec_matches_parent_tags_and_excludes() {
        let (dir, _) = setup(&[
            ("a.md", "needle #rust/cli\n"),
            ("b.md", "---\ntags: [rust, draft]\n---\n\nneedle\n"),
            ("c.md", "needle #rusty\n"),
            ("d.md", "needle\n"),
        ]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        let report = ix.sync_all(|_, _| {});
        assert!(report.errors.is_empty(), "{report:?}");
        let notes = |ix: &Indexer, q: &str| {
            let mut v: Vec<String> =
                ix.search(q, 10).unwrap().into_iter().map(|h| h.path.as_str().to_owned()).collect();
            v.sort();
            v
        };
        assert_eq!(notes(&ix, "#rust"), ["a.md", "b.md"]);
        assert_eq!(notes(&ix, "needle tag:rust"), ["a.md", "b.md"]);
        assert_eq!(notes(&ix, "needle #rust,"), ["a.md", "b.md"]);
        assert_eq!(notes(&ix, "#rust/cli"), ["a.md"]);
        assert_eq!(notes(&ix, "needle -#rust"), ["c.md", "d.md"]);
        assert_eq!(notes(&ix, "#rust -#draft"), ["a.md"]);
        assert_eq!(notes(&ix, "needle -#rust/cli -tag:rusty"), ["b.md", "d.md"]);
        assert!(notes(&ix, "-#draft").is_empty());
        // 排除也按层级：`-#rust` 连 `rust/cli` 一起排除；`rusty` 不受影响
        assert_eq!(notes(&ix, "needle -#rust -#rusty"), ["d.md"]);
    }

    #[test]
    fn tag_filters_follow_save_rename_and_remove() {
        let (dir, _) = setup(&[("a.md", "---\ntags: [rust]\n---\n\nanchor #cli\n\nsecond\n")]);
        let data = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(dir.path()).unwrap());
        let mut ix = Indexer::open(vault, data.path()).unwrap();
        assert_eq!(ix.sync_all(|_, _| {}).indexed, 1);
        assert_eq!(paths(&ix.search("#rust #cli", 10).unwrap()), [("a.md".into(), 0)]);
        assert_eq!(ix.search("second #rust #cli", 10).unwrap().len(), 1);
        let old = VaultPath::new("a.md").unwrap();
        let text = "---\ntags: [new]\n---\n\nanchor\n\nsecond\n";
        std::fs::write(dir.path().join("a.md"), text).unwrap();
        let (_, stamp) = ix.vault().read(&old).unwrap();
        ix.index_text(&old, text, stamp).unwrap();
        assert!(ix.search("#rust", 10).unwrap().is_empty());
        assert!(ix.search("#cli", 10).unwrap().is_empty());
        assert_eq!(paths(&ix.search("#new", 10).unwrap()), [("a.md".into(), 0)]);
        assert_eq!(ix.search("second #new", 10).unwrap().len(), 1);
        let new = VaultPath::new("moved.md").unwrap();
        ix.vault().rename(&old, &new).unwrap();
        ix.rename_note(&old, &new).unwrap();
        assert_eq!(paths(&ix.search("anchor #new", 10).unwrap()), [("moved.md".into(), 0)]);
        ix.remove_note(&new).unwrap();
        assert!(ix.search("#new", 10).unwrap().is_empty());
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
