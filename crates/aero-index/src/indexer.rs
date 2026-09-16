//! 增量索引器（方案 10.1）：vault 文件 → 解析 → 块 → 全文索引；元数据进 SQLite。
//!
//! 保存路径按 ADR 0003：`upsert(新块)` + `delete(旧有而新无的 pk)`，旧块数来自 SQLite，不查引擎。
//! 全部方法同步、阻塞，调用方放到后台线程；一次 `sync_all` 大 vault 可能要几秒，通过回调报进度。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aero_core::index::blocks_and_links_for_note;
use aero_core::links::{Resolution, Resolver};
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
/// 块 / 过滤字段的布局版本；变了就整体重建。3：`tags_filter` 连祖先标签一起写（父标签查询）。
const FTS_VERSION: i64 = 3;

/// wikilink 解析用的笔记表：路径 / 文件名 / 标题 → 路径，以及路径 → `note_id`。
/// 从 `notes` 表建一次，之后随每次入库 / 删除 / 改名增量维护，单篇保存不再全表重读、不再走 vault。
///
/// 全量同步开始时（[`Indexer::plan_sync`]）磁盘上的全部路径就登记进来，新文件预分配 `note_id`：
/// 这样逐篇入库时按路径 / 文件名的解析已经是最终结果（不会把还没轮到的同名笔记漏成"唯一"），
/// 收尾只需补挂靠标题解析的少数行——而不是把上百万行读出来再逐行 UPDATE。
struct LinkUniverse {
    resolver: Resolver,
    id_by_path: HashMap<String, String>,
}

impl LinkUniverse {
    fn from_rows(rows: Vec<NoteRow>) -> Self {
        let mut u =
            LinkUniverse { resolver: Resolver::new(), id_by_path: HashMap::with_capacity(rows.len()) };
        for row in rows {
            if let Ok(path) = VaultPath::new(&row.path) {
                u.insert(path, &row.note_id, Some(&row.title));
            }
        }
        u
    }

    /// `title` 为 None：只登记路径（预分配阶段，标题要等读了文件才知道）。
    fn insert(&mut self, path: VaultPath, note_id: &str, title: Option<&str>) {
        self.id_by_path.insert(path.as_str().to_owned(), note_id.to_owned());
        self.resolver.insert(path, title);
    }

    fn remove(&mut self, path: &VaultPath) {
        self.id_by_path.remove(path.as_str());
        self.resolver.remove(path);
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
    /// `(路径, 改动前的标题)`
    notes: Vec<(VaultPath, Option<String>)>,
    /// 超过这么多篇就不逐篇算邻域了
    overflow: bool,
}

const TOUCHED_LIMIT: usize = 64;

impl Touched {
    fn push(&mut self, path: VaultPath, old_title: Option<String>) {
        if self.notes.len() >= TOUCHED_LIMIT {
            self.overflow = true;
        } else {
            self.notes.push((path, old_title));
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
        let notes = self.vault.notes()?;
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
                    universe.insert(path.clone(), id, None);
                }
            }
        }
        Ok(SyncPlan { notes, removed: known.into_values().collect() })
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
            self.touched.push(path.clone(), None);
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
        let note_id = match &existing {
            Some(r) => r.note_id.clone(),
            None => {
                self.preassigned.remove(path.as_str()).unwrap_or_else(|| ulid::Ulid::generate().to_string())
            }
        };
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
        self.meta.upsert_note(&NoteRow {
            note_id: note_id.clone(),
            path: path.as_str().to_owned(),
            title: title.clone(),
            stamp,
            block_count: new_count,
        })?;
        let old_title = existing.as_ref().map(|r| r.title.clone()).filter(|t| *t != title);
        if old_title.is_some() {
            // 靠旧标题解析到这篇的行不再成立；补挂时能按新标题 / 文件名解析上的会挂回来
            self.meta.unresolve_incoming(&note_id)?;
        }
        // 只有"这篇是新的"或"标题变了"才会影响别人指向它的行；正文改动不用重判入链
        // （被大量引用的笔记每次保存都重判几千行，保存后可搜索的延迟会跟着涨）
        if existing.is_none() || old_title.is_some() {
            self.touched.push(path.clone(), old_title);
        }
        self.universe()?.insert(path.clone(), &note_id, Some(&title));
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
            // 先记邻域再移除：它的标题组要从 title_of 里查
            self.touched.push(path.clone(), Some(row.title.clone()));
            if let Some(u) = self.links.as_mut() {
                u.remove(&path);
            }
        }
        Ok(())
    }

    /// 重命名 / 移动：`note_id` 不变；块里存的 `path` / `title` 要跟着改，所以重写这些块。
    /// 按旧名字解析到它的链接行退回未解析（与预览点击时的解析结果保持一致；M2 的改名同步会改写链接文本）。
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
        self.touched.push(old.clone(), Some(row.title.clone()));
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
                .flat_map(|(p, old_title)| universe.resolver.neighbors(p, old_title.as_deref()))
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
    pub fn link_resolver(&mut self) -> Result<Resolver, IndexerError> {
        let mut resolver = self.universe()?.resolver.clone();
        for path in self.vault.notes()? {
            if !resolver.contains(&path) {
                resolver.insert(path, None);
            }
        }
        Ok(resolver)
    }

    fn universe(&mut self) -> Result<&mut LinkUniverse, IndexerError> {
        if self.links.is_none() {
            self.links = Some(LinkUniverse::from_rows(self.meta.all_notes()?));
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
        assert_eq!(ix.touched.notes, [(b.clone(), Some("B".to_owned()))], "改标题要重判");
        ix.reconcile_links().unwrap();

        let c = VaultPath::new("c.md").unwrap();
        ix.index_document(&c, &Document::from_text("c\n"), stamp).unwrap();
        assert_eq!(ix.touched.notes, [(c, None)], "新笔记要重判");
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
        let mut meta = Meta::open(&index_dir.join("meta.sqlite")).unwrap();
        meta.upsert_note(&crate::meta::NoteRow {
            note_id: "legacy".into(),
            path: "a.md".into(),
            title: "a".into(),
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
