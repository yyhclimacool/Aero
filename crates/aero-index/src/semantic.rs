//! Worker-owned, vector-only semantic cache; no model execution or FTS locks.
//! `note_id` in a semantic query means the exact vault-relative path, NOT the FTS ULID.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use aero_core::{Document, FtsQuery, Vault, VaultPath, hash_bytes};
use rusqlite::{Connection, OptionalExtension, params};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, IndexParams, MetricType,
    SearchQuery,
};

use crate::SearchHit;

pub const SEMANTIC_DIMENSION: usize = 384;
pub const MAX_NOTE_BYTES: usize = 2 * 1024 * 1024;
const SCHEMA_VERSION: i64 = 1;
const MAX_CHUNKS: usize = 8192;
const WRITE_BATCH: usize = 64;
// Bounded ANN overfetch, never a vault or metadata-table scan during search.
const MAX_CANDIDATES: usize = 4096;
const MAX_SEARCH_BYTES: usize = 32 * 1024 * 1024;
const READ_BATCH_BYTES: usize = 64 * 1024;

#[derive(Debug)]
enum NoteRead {
    Bytes(Vec<u8>),
    Missing,
    Cancelled,
    BudgetExhausted,
}

#[derive(Clone, Debug)]
pub struct SemanticChunk {
    /// Budgeted title prefix + plain body, ready for the caller's embedding model.
    pub text: String,
    pub block_ord: u32,
    pub body: String,
    title: Arc<str>,
    tags: Arc<[String]>,
}

#[derive(Clone, Debug)]
pub struct SemanticPlan {
    pub path: VaultPath,
    pub fingerprint: u64,
    pub chunks: Vec<SemanticChunk>,
}

/// Own on one dedicated worker. SQLite deliberately makes this Send, not Sync.
/// Drop closes only this collection/connection; global zvec initialization stays alive.
pub struct SemanticIndex {
    vault: Arc<Vault>,
    col: Collection,
    db: Connection,
    committed: (usize, usize),
    live_paths: Option<HashSet<VaultPath>>,
}

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl SemanticIndex {
    pub fn open(vault: Arc<Vault>, data_dir: &Path, model_id: &str) -> Result<Self, String> {
        Self::open_interruptible(vault, data_dir, model_id, true, || false)?
            .ok_or_else(|| "semantic open unexpectedly cancelled".into())
    }

    /// Disable recovery for query-only opens; flush_interruptible cleans the journal when idle.
    pub fn open_interruptible(
        vault: Arc<Vault>,
        data_dir: &Path,
        model_id: &str,
        recover: bool,
        cancel: impl Fn() -> bool,
    ) -> Result<Option<Self>, String> {
        if cancel() {
            return Ok(None);
        }
        crate::ensure_initialized().map_err(error)?;
        let dir = data_dir.join(vault.id()).join("semantic");
        fs::create_dir_all(&dir).map_err(error)?;
        let mut db = Connection::open(dir.join("metadata.sqlite")).map_err(error)?;
        db.busy_timeout(std::time::Duration::ZERO).map_err(error)?;
        // Retain an exclusive lock for this connection's lifetime, including between commits.
        // Acquire BEFORE inspecting versions: a second worker/model must never reset live files.
        db.execute_batch("PRAGMA locking_mode=EXCLUSIVE;
            PRAGMA synchronous=FULL;
            BEGIN EXCLUSIVE;
            CREATE TABLE IF NOT EXISTS cache_config (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL, model TEXT NOT NULL);
            COMMIT;").map_err(|e| format!("semantic metadata unavailable (possibly open elsewhere): {e}"))?;
        let config: Option<(i64, String)> = db
            .query_row("SELECT version, model FROM cache_config WHERE singleton=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(error)?;
        let reset = config.as_ref().is_none_or(|(v, m)| *v != SCHEMA_VERSION || m != model_id);
        if cancel() {
            return Ok(None);
        }
        let vector_dir = dir.join("vectors");
        let mut options = CollectionOptions::new().map_err(error)?;
        options.set_enable_mmap(true).map_err(error)?;
        options.set_max_buffer_size(8 * 1024 * 1024).map_err(error)?;
        // Open errors (including engine locks/corruption) are propagated, NEVER treated as reset permission.
        let existing = if vector_dir.exists() {
            Some(Collection::open(&vector_dir.to_string_lossy(), Some(&options)).map_err(error)?)
        } else {
            None
        };
        if cancel() {
            return Ok(None);
        }
        // Finish an initiated schema reset before observing cancellation again.
        let col = if reset {
            if let Some(col) = existing {
                col.close().map_err(error)?;
                fs::remove_dir_all(&vector_dir).map_err(error)?;
            }
            create_collection(&vector_dir, &options)?
        } else {
            existing.ok_or_else(|| {
                "semantic vector cache is missing; refusing to trust existing metadata".to_owned()
            })?
        };
        if reset {
            let tx = db.transaction().map_err(error)?;
            tx.execute_batch(
                "DROP TABLE IF EXISTS notes;
                DROP TABLE IF EXISTS vector_keys;
                DROP TABLE IF EXISTS tag_ids;
                CREATE TABLE notes(path TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, chunks INTEGER NOT NULL);
                CREATE TABLE vector_keys(pk TEXT PRIMARY KEY, path TEXT NOT NULL, version TEXT NOT NULL);
                CREATE INDEX vector_keys_path ON vector_keys(path);
                CREATE TABLE tag_ids(id INTEGER PRIMARY KEY, tag TEXT NOT NULL UNIQUE);",
            )
            .map_err(error)?;
            tx.execute(
                "INSERT OR REPLACE INTO cache_config VALUES(1, ?1, ?2)",
                params![SCHEMA_VERSION, model_id],
            )
            .map_err(error)?;
            tx.commit().map_err(error)?;
        }
        let committed = db
            .query_row("SELECT count(*), coalesce(sum(chunks),0) FROM notes", [], |r| {
                Ok((r.get::<_, usize>(0)?, r.get::<_, usize>(1)?))
            })
            .map_err(error)?;
        let mut index = Self { vault, col, db, committed, live_paths: None };
        if cancel() || (recover && !index.clean_obsolete_interruptible(None, &cancel)?) {
            return Ok(None);
        }
        Ok(Some(index))
    }

    /// Install the latest successful `Vault::notes()` rescan, including ignore rules.
    /// Workers MUST set this before serving queries/preparing notes, and replace it
    /// on every rescan (including ignore changes). Excluded paths are immediately
    /// rejected; prepare/remove unpublishes their metadata with cancellable cleanup.
    /// Core has no per-path ignore matcher: no implicit vault walk occurs here or
    /// in search. Without a snapshot, legacy direct callers only get path safety checks.
    pub fn set_live_paths(&mut self, paths: HashSet<VaultPath>) {
        self.live_paths = Some(paths);
    }

    pub fn known_paths(&self) -> Result<Vec<VaultPath>, String> {
        let mut statement = self.db.prepare("SELECT path FROM notes ORDER BY path").map_err(error)?;
        statement
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(error)?
            .map(|p| VaultPath::new(&p.map_err(error)?).map_err(error))
            .collect()
    }

    /// Counts only fully committed notes/chunks, not pending vectors.
    pub fn stats(&self) -> (usize, usize) {
        self.committed
    }

    pub fn prepare_note(&mut self, path: &VaultPath) -> Result<Option<SemanticPlan>, String> {
        self.prepare_note_interruptible(path, || false)
    }

    /// None means unchanged, removed, or cancelled (no plan was published).
    /// Workers should latch a true callback result and requeue on cancellation;
    /// a missing/excluded note may already be unpublished, with cleanup deferred.
    pub fn prepare_note_interruptible(
        &mut self,
        path: &VaultPath,
        cancel: impl Fn() -> bool,
    ) -> Result<Option<SemanticPlan>, String> {
        if cancel() {
            return Ok(None);
        }
        let bytes = match self.read_note_bounded(path, &mut (MAX_NOTE_BYTES + 1), &cancel)? {
            NoteRead::Bytes(bytes) => bytes,
            NoteRead::Missing => {
                self.remove_note_interruptible(path, &cancel)?;
                return Ok(None);
            }
            NoteRead::Cancelled | NoteRead::BudgetExhausted => return Ok(None),
        };
        if cancel() {
            return Ok(None);
        }
        let fingerprint = hash_bytes(&bytes);
        if self.note_state(path)?.is_some_and(|(v, _)| v == version(fingerprint)) {
            return Ok(None);
        }
        let doc = Document::from_bytes(&bytes).map_err(|e| format!("{path}: {e}"))?;
        let title: Arc<str> = doc.title_or(path.stem()).into();
        let prefix = &title[..budget_end(&title, 80, 24)];
        let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}\n") };
        let chars = 700 - prefix.chars().count();
        let units = 200 - estimated_units(&prefix);
        let tags: Arc<[String]> = aero_core::index::tags_for_note(&doc).into();
        if title.len() + tags.iter().map(String::len).sum::<usize>() > 16 * 1024 {
            return Err(format!("{path}: skipped; title and tags exceed 16 KiB"));
        }
        let mut chunks = Vec::new();
        // Stream exactly the core's visible leaves/ord, without IndexBlock's per-leaf
        // title/tags copies. Alias-only notes still have no semantic body/chunks.
        for (ord, (_, body)) in aero_core::index::indexable_bodies(&doc).enumerate() {
            let mut rest = body.trim();
            while !rest.is_empty() {
                if cancel() {
                    return Ok(None);
                }
                if chunks.len() == MAX_CHUNKS {
                    return Err(format!("{path}: skipped; more than {MAX_CHUNKS} semantic chunks"));
                }
                let end = budget_end(rest, chars, units);
                let body = rest[..end].trim();
                if !body.is_empty() {
                    chunks.push(SemanticChunk {
                        text: format!("{prefix}{body}"),
                        body: body.to_owned(),
                        block_ord: ord as u32,
                        title: title.clone(),
                        tags: tags.clone(),
                    });
                }
                if end == rest.len() {
                    break;
                }
                // At most 30 estimated units / 100 Unicode scalars of overlap; always make progress.
                let reversed: String = rest[..end].chars().rev().collect();
                let overlap_end = budget_end(&reversed, 100, 30);
                let overlap_bytes = reversed[..overlap_end].len();
                let advance = if overlap_bytes < end { end - overlap_bytes } else { end };
                rest = rest[advance..].trim_start();
            }
        }
        Ok(Some(SemanticPlan { path: path.clone(), fingerprint, chunks }))
    }

    pub fn commit_note(&mut self, plan: &SemanticPlan, vectors: &[Vec<f32>]) -> Result<bool, String> {
        self.commit_note_interruptible(plan, vectors, || false)
    }

    /// Returns false for stale/canceled plans without publishing them. Pending keys stay
    /// journaled: retry ALL chunks with the same plan/vectors, or let reopen/cleanup discard
    /// them. Cancellation never removes the previous committed version or copies vectors.
    /// Once metadata is published, cancellation only defers cleanup and the result is true.
    pub fn commit_note_interruptible(
        &mut self,
        plan: &SemanticPlan,
        vectors: &[Vec<f32>],
        cancel: impl Fn() -> bool,
    ) -> Result<bool, String> {
        if cancel() {
            return Ok(false);
        }
        if vectors.len() != plan.chunks.len() || plan.chunks.len() > MAX_CHUNKS {
            return Err("semantic vector count must equal the bounded chunk count".into());
        }
        for batch in vectors.chunks(WRITE_BATCH) {
            if cancel() {
                return Ok(false);
            }
            for vector in batch {
                validate_vector(vector)?;
            }
        }
        let Some(bytes) = self.read_note(&plan.path)? else {
            // Missing is stale too; prepare_note/remove_note own unpublishing deletions.
            return Ok(false);
        };
        if hash_bytes(&bytes) != plan.fingerprint {
            return Ok(false);
        }
        let ver = version(plan.fingerprint);
        let old = self.note_state(&plan.path)?;
        if old.as_ref().is_some_and(|(v, _)| *v == ver) {
            // Idempotent retries must not overwrite vectors of an already published version.
            self.clean_obsolete_interruptible(Some(&plan.path), &cancel)?;
            return Ok(true);
        }
        // Journal intended keys BEFORE touching zvec. Failed writes/transactions remain retryable
        // and recoverable on reopen. This is NOT the committed fingerprint/keys in `notes`.
        // zvec PKs are restricted ASCII and at most 64 bytes; paths remain lossless scalar fields.
        let path_hash = hash_bytes(plan.path.as_str().as_bytes());
        let keys: Vec<String> =
            (0..plan.chunks.len()).map(|i| format!("s{path_hash:016x}_{ver}_{i}")).collect();
        let tags: Vec<String> = plan
            .chunks
            .iter()
            .flat_map(|c| c.tags.iter())
            .flat_map(|t| aero_core::index::tag_with_ancestors(&t.to_lowercase()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut tag_ids = HashMap::new();
        for batch in tags.chunks(WRITE_BATCH) {
            if cancel() {
                return Ok(false);
            }
            let tx = self.db.transaction().map_err(error)?;
            for tag in batch {
                tx.execute("INSERT OR IGNORE INTO tag_ids(tag) VALUES(?1)", [tag]).map_err(error)?;
                let id: u64 = tx
                    .query_row("SELECT id FROM tag_ids WHERE tag=?1", [tag], |r| r.get(0))
                    .map_err(error)?;
                tag_ids.insert(tag.clone(), id);
            }
            tx.commit().map_err(error)?;
        }
        for batch in keys.chunks(WRITE_BATCH) {
            if cancel() {
                return Ok(false);
            }
            let tx = self.db.transaction().map_err(error)?;
            for key in batch {
                tx.execute(
                    "INSERT OR IGNORE INTO vector_keys VALUES(?1,?2,?3)",
                    params![key, plan.path.as_str(), ver],
                )
                .map_err(error)?;
            }
            tx.commit().map_err(error)?;
        }
        for start in (0..keys.len()).step_by(WRITE_BATCH) {
            if cancel() {
                return Ok(false);
            }
            let end = (start + WRITE_BATCH).min(keys.len());
            let docs = (start..end)
                .map(|i| {
                    let chunk = &plan.chunks[i];
                    let mut d = Doc::new().map_err(error)?;
                    d.set_pk(&keys[i]);
                    d.add_string("path", plan.path.as_str()).map_err(error)?;
                    d.add_string("version", &ver).map_err(error)?;
                    d.add_string("title", &chunk.title).map_err(error)?;
                    d.add_string("body", &chunk.body).map_err(error)?;
                    d.add_u32("block_ord", chunk.block_ord).map_err(error)?;
                    let mut ids: BTreeSet<u64> = chunk
                        .tags
                        .iter()
                        .flat_map(|t| aero_core::index::tag_with_ancestors(&t.to_lowercase()))
                        .filter_map(|t| tag_ids.get(&t).copied())
                        .collect();
                    ids.insert(0); // non-null empty-tag representation; real SQLite rowids start at 1.
                    d.add_array_u64("tags", &ids.into_iter().collect::<Vec<_>>()).map_err(error)?;
                    d.add_vector_f32("embedding", &vectors[i]).map_err(error)?;
                    Ok(d)
                })
                .collect::<Result<Vec<_>, String>>()?;
            let result = self.col.upsert(&docs.iter().collect::<Vec<_>>()).map_err(error)?;
            if result.error_count > 0 {
                return Err(format!(
                    "semantic upsert: {}",
                    result
                        .results
                        .iter()
                        .find(|r| !r.success)
                        .map_or("unknown error", |r| r.message.as_str())
                ));
            }
        }
        // Durability before publication: a crash must not claim fingerprints for unwritten vectors.
        if cancel() {
            return Ok(false);
        }
        self.col.flush().map_err(error)?;
        // Also detect edits occurring during the batched write, not just during inference.
        if cancel() || !self.read_note(&plan.path)?.is_some_and(|b| hash_bytes(&b) == plan.fingerprint) {
            return Ok(false);
        }
        let tx = self.db.transaction().map_err(error)?;
        tx.execute(
            "INSERT OR REPLACE INTO notes VALUES(?1,?2,?3)",
            params![plan.path.as_str(), ver, keys.len()],
        )
        .map_err(error)?;
        tx.commit().map_err(error)?;
        self.committed.0 += usize::from(old.is_none());
        self.committed.1 = self.committed.1 - old.map_or(0, |(_, n)| n) + keys.len();
        // Publication is the commit point. A canceled cleanup leaves recoverable obsolete
        // keys, but must not tell the worker to treat this fully committed note as pending.
        self.clean_obsolete_interruptible(Some(&plan.path), &cancel)?;
        Ok(true)
    }

    pub fn remove_note(&mut self, path: &VaultPath) -> Result<(), String> {
        self.remove_note_interruptible(path, || false).map(|_| ())
    }

    /// False means cancelled BEFORE unpublication. True means no committed note
    /// remains, even if cancellation deferred physical cleanup. Retry is idempotent.
    pub fn remove_note_interruptible(
        &mut self,
        path: &VaultPath,
        cancel: impl Fn() -> bool,
    ) -> Result<bool, String> {
        if cancel() {
            return Ok(false);
        }
        let old = self.note_state(path)?;
        // Unpublish first, so partial/failed physical deletion cannot leak hits.
        self.db.execute("DELETE FROM notes WHERE path=?1", [path.as_str()]).map_err(error)?;
        if let Some((_, n)) = old {
            self.committed.0 -= 1;
            self.committed.1 -= n;
        }
        self.clean_obsolete_interruptible(Some(path), &cancel)?;
        Ok(true)
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.flush_interruptible(|| false).map(|_| ())
    }

    /// False means cancelled; remaining cleanup stays journaled for retry/reopen.
    pub fn flush_interruptible(&mut self, cancel: impl Fn() -> bool) -> Result<bool, String> {
        if !self.clean_obsolete_interruptible(None, &cancel)? || cancel() {
            return Ok(false);
        }
        self.col.flush().map_err(error)?;
        Ok(true)
    }

    /// HNSW cosine candidates; tags are filtered in-engine BEFORE top-k. `query.text`
    /// is already represented by `vector`; `query.note_id` is an exact relative path.
    /// Invalid/deleted/changed notes are suppressed with one disk hash per candidate note.
    /// Bounded overfetch may return fewer than limit when many stale/duplicate chunks rank first.
    pub fn search(&self, vector: &[f32], query: &FtsQuery) -> Result<Vec<SearchHit>, String> {
        self.search_interruptible(vector, query, || false)
    }

    /// Cancellation returns an empty result, not an error or stale partial results.
    /// At most 32 MiB of file contents are read across ALL candidates/overfetch rounds;
    /// budget exhaustion returns the validated partial results. Individual engine
    /// queries are not interruptible; checks bracket them and each candidate/read.
    pub fn search_interruptible(
        &self,
        vector: &[f32],
        query: &FtsQuery,
        cancel: impl Fn() -> bool,
    ) -> Result<Vec<SearchHit>, String> {
        validate_vector(vector)?;
        if cancel() || query.limit == 0 || self.committed.1 == 0 {
            return Ok(Vec::new());
        }
        let limit = query.limit.min(1000);
        let mut filters = Vec::new();
        for (tags, op) in [(&query.tags, "CONTAIN_ALL"), (&query.exclude_tags, "NOT CONTAIN_ANY")] {
            let mut ids = Vec::new();
            for tag in tags {
                if cancel() {
                    return Ok(Vec::new());
                }
                match self.tag_id(&tag.to_lowercase())? {
                    Some(id) => ids.push(id.to_string()),
                    None if op == "CONTAIN_ALL" => return Ok(Vec::new()),
                    None => {}
                }
            }
            if !ids.is_empty() {
                filters.push(format!("tags {op} ({})", ids.join(",")));
            }
        }
        if let Some(path) = &query.note_id {
            filters.push(format!("path = {}", crate::zvec_fts::sql_literal(path).map_err(error)?));
        }
        let mut topk = (limit * 4 + 32).min(MAX_CANDIDATES);
        let mut versions: HashMap<VaultPath, Option<String>> = HashMap::new();
        let mut remaining_bytes = MAX_SEARCH_BYTES;
        loop {
            if cancel() {
                return Ok(Vec::new());
            }
            let mut q = SearchQuery::new("embedding", vector, topk as i32).map_err(error)?;
            if !filters.is_empty() {
                q.set_filter(&filters.join(" AND ")).map_err(error)?;
            }
            q.set_output_fields(&["path", "version", "title", "body", "block_ord"]).map_err(error)?;
            let docs = self.col.query(&q).map_err(error)?;
            if cancel() {
                return Ok(Vec::new());
            }
            let mut exhausted = docs.len() < topk || topk == MAX_CANDIDATES;
            let mut hits = Vec::new();
            for doc in docs {
                if cancel() {
                    return Ok(Vec::new());
                }
                let path = VaultPath::new(&doc.get_string("path").map_err(error)?.unwrap_or_default())
                    .map_err(error)?;
                if !versions.contains_key(&path) {
                    let committed = self.note_state(&path)?.map(|(v, _)| v);
                    let valid = if let Some(v) = committed {
                        match self.read_note_bounded(&path, &mut remaining_bytes, &cancel) {
                            Ok(NoteRead::Bytes(bytes)) if version(hash_bytes(&bytes)) == v => Some(v),
                            Ok(NoteRead::Cancelled) => return Ok(Vec::new()),
                            Ok(NoteRead::BudgetExhausted) => {
                                exhausted = true;
                                break;
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    versions.insert(path.clone(), valid);
                }
                let Some(Some(current)) = versions.get(&path) else { continue };
                if doc.get_string("version").map_err(error)?.as_ref() != Some(current) {
                    continue;
                }
                // zvec's cosine score is a distance (0 = identical); SearchHit is higher-is-better.
                let score = 1.0 - doc.get_score();
                if !score.is_finite() {
                    continue;
                }
                hits.push(SearchHit {
                    path,
                    title: doc.get_string("title").map_err(error)?.unwrap_or_default(),
                    body: doc.get_string("body").map_err(error)?.unwrap_or_default(),
                    block_ord: doc.get_u32("block_ord").map_err(error)?.unwrap_or_default(),
                    score,
                });
            }
            hits.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| a.path.cmp(&b.path))
                    .then_with(|| a.block_ord.cmp(&b.block_ord))
                    .then_with(|| a.body.cmp(&b.body))
            });
            let mut seen = HashSet::new();
            hits.retain(|h| seen.insert((h.path.clone(), h.block_ord)));
            if cancel() {
                return Ok(Vec::new());
            }
            if hits.len() >= limit || exhausted {
                hits.truncate(limit);
                return Ok(hits);
            }
            topk = (topk * 2).min(MAX_CANDIDATES);
        }
    }

    fn note_state(&self, path: &VaultPath) -> Result<Option<(String, usize)>, String> {
        self.db
            .query_row("SELECT fingerprint,chunks FROM notes WHERE path=?1", [path.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(error)
    }

    fn tag_id(&self, tag: &str) -> Result<Option<u64>, String> {
        self.db
            .query_row("SELECT id FROM tag_ids WHERE tag=?1", [tag], |r| r.get(0))
            .optional()
            .map_err(error)
    }

    fn clean_obsolete_interruptible(
        &mut self,
        path: Option<&VaultPath>,
        cancel: &impl Fn() -> bool,
    ) -> Result<bool, String> {
        loop {
            if cancel() {
                return Ok(false);
            }
            let keys = {
                let mut stmt = self.db.prepare("SELECT k.pk FROM vector_keys k LEFT JOIN notes n ON n.path=k.path
                    WHERE (n.path IS NULL OR n.fingerprint != k.version) AND (?1 IS NULL OR k.path=?1) LIMIT ?2").map_err(error)?;
                stmt.query_map(params![path.map(VaultPath::as_str), WRITE_BATCH], |r| r.get::<_, String>(0))
                    .map_err(error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(error)?
            };
            if keys.is_empty() {
                return Ok(true);
            }
            if cancel() {
                return Ok(false);
            }
            match self.col.delete(&keys.iter().map(String::as_str).collect::<Vec<_>>()) {
                Ok(result) => {
                    if let Some(bad) =
                        result.results.iter().find(|r| !r.success && r.code != zvec_rust::ErrorCode::NotFound)
                    {
                        return Err(format!("semantic delete: {}", bad.message));
                    }
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(error(e)),
            }
            // Retain the journal until physical deletes are durable. Cancellation at
            // either checkpoint is safe even if zvec has already applied the deletes.
            if cancel() {
                return Ok(false);
            }
            self.col.flush().map_err(error)?;
            if cancel() {
                return Ok(false);
            }
            let tx = self.db.transaction().map_err(error)?;
            for key in keys {
                tx.execute("DELETE FROM vector_keys WHERE pk=?1", [key]).map_err(error)?;
            }
            tx.commit().map_err(error)?;
        }
    }

    fn read_note(&self, path: &VaultPath) -> Result<Option<Vec<u8>>, String> {
        match self.read_note_bounded(path, &mut (MAX_NOTE_BYTES + 1), &|| false)? {
            NoteRead::Bytes(bytes) => Ok(Some(bytes)),
            NoteRead::Missing => Ok(None),
            _ => Err("semantic read unexpectedly interrupted".into()),
        }
    }

    fn read_note_bounded(
        &self,
        path: &VaultPath,
        remaining: &mut usize,
        cancel: &impl Fn() -> bool,
    ) -> Result<NoteRead, String> {
        if cancel() {
            return Ok(NoteRead::Cancelled);
        }
        if !path.is_note() || path.as_str().split('/').any(|c| c.starts_with('.') || c.ends_with(".assets")) {
            return Err(format!("{path}: skipped; not a visible Markdown note"));
        }
        if self.live_paths.as_ref().is_some_and(|paths| !paths.contains(path)) {
            return Ok(NoteRead::Missing);
        }
        let check = || -> Result<Option<fs::Metadata>, String> {
            let mut disk = self.vault.root().to_owned();
            let mut last = None;
            for part in path.as_str().split('/') {
                disk.push(part);
                match fs::symlink_metadata(&disk) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(format!("{path}: skipped; symbolic link"));
                    }
                    Ok(meta) => last = Some(meta),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(format!("{path}: {e}")),
                }
            }
            Ok(last)
        };
        let Some(before) = check()? else { return Ok(NoteRead::Missing) };
        if !before.is_file() {
            return Err(format!("{path}: skipped; not a regular file"));
        }
        if before.len() > MAX_NOTE_BYTES as u64 {
            return Err(format!("{path}: skipped; exceeds 2 MiB semantic note limit"));
        }
        if cancel() {
            return Ok(NoteRead::Cancelled);
        }
        if before.len() > *remaining as u64 {
            return Ok(NoteRead::BudgetExhausted);
        }
        let mut file = match fs::File::open(path.to_fs(self.vault.root())) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NoteRead::Missing),
            Err(e) => return Err(format!("{path}: {e}")),
        };
        let mut bytes = Vec::with_capacity(before.len() as usize);
        let mut buffer = [0u8; READ_BATCH_BYTES];
        // Read only the checked size: growth/shrinkage is rejected by the metadata
        // and length checks below. Every actual byte, even from an invalid file,
        // is charged to the query's shared budget.
        while bytes.len() < before.len() as usize {
            if cancel() {
                return Ok(NoteRead::Cancelled);
            }
            let len = buffer.len().min(before.len() as usize - bytes.len());
            let n = file.read(&mut buffer[..len]).map_err(error)?;
            *remaining -= n;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        if cancel() {
            return Ok(NoteRead::Cancelled);
        }
        let Some(after) = check()? else { return Ok(NoteRead::Missing) };
        if bytes.len() != before.len() as usize
            || before.len() != after.len()
            || before.modified().map_err(error)? != after.modified().map_err(error)?
        {
            return Err(format!("{path}: changed while reading; retry later"));
        }
        Ok(NoteRead::Bytes(bytes))
    }
}

fn create_collection(dir: &Path, options: &CollectionOptions) -> Result<Collection, String> {
    let mut schema = CollectionSchema::builder("aero_semantic");
    for name in ["version", "title", "body"] {
        schema = schema.add_field(FieldSchema::new(name, DataType::String, false, 0).map_err(error)?);
    }
    let schema = schema
        .add_field(FieldSchema::new("block_ord", DataType::Uint32, false, 0).map_err(error)?)
        .add_indexed_field("path", DataType::String, IndexParams::invert(false, false).map_err(error)?)
        .add_indexed_field("tags", DataType::ArrayUint64, IndexParams::invert(false, false).map_err(error)?)
        .add_vector_field(
            "embedding",
            DataType::VectorFp32,
            SEMANTIC_DIMENSION as u32,
            IndexParams::hnsw(MetricType::Cosine, 16, 100).map_err(error)?,
        )
        .build()
        .map_err(error)?;
    Collection::create_and_open(&dir.to_string_lossy(), &schema, Some(options)).map_err(error)
}

fn version(fingerprint: u64) -> String {
    format!("{fingerprint:016x}")
}

fn validate_vector(vector: &[f32]) -> Result<(), String> {
    if vector.len() != SEMANTIC_DIMENSION || vector.iter().any(|v| !v.is_finite()) {
        return Err(format!("expected {SEMANTIC_DIMENSION} finite semantic vector values"));
    }
    if vector.iter().all(|v| *v == 0.0) {
        return Err("cosine vector must be nonzero".into());
    }
    Ok(())
}

// English word starts cost one unit; each additional six ASCII letters costs one
// more (conservative subword estimate). CJK/other Unicode scalars cost one each.
fn unit_cost(c: char, ascii_run: &mut usize) -> usize {
    if c.is_ascii_alphanumeric() {
        let cost = usize::from((*ascii_run).is_multiple_of(6));
        *ascii_run += 1;
        cost
    } else {
        *ascii_run = 0;
        usize::from(!c.is_whitespace())
    }
}

fn estimated_units(text: &str) -> usize {
    let mut run = 0;
    text.chars().map(|c| unit_cost(c, &mut run)).sum()
}

fn budget_end(text: &str, max_chars: usize, max_units: usize) -> usize {
    let (mut units, mut run) = (0, 0);
    for (chars, (byte, c)) in text.char_indices().enumerate() {
        units += unit_cost(c, &mut run);
        if chars == max_chars || units > max_units {
            return byte;
        }
    }
    text.len()
}

/// RRF k=60, each route votes once per (path, block_ord). Stable ties prefer the
/// first FTS occurrence, whose display body/title also win over semantic snippets.
pub fn fuse_results(fts: &[SearchHit], semantic: &[SearchHit], limit: usize) -> Vec<SearchHit> {
    let mut merged: Vec<SearchHit> = Vec::new();
    let mut positions: HashMap<(VaultPath, u32), usize> = HashMap::new();
    for route in [fts, semantic] {
        let mut seen = HashSet::new();
        for (rank, hit) in route.iter().enumerate() {
            let key = (hit.path.clone(), hit.block_ord);
            if !seen.insert(key.clone()) {
                continue;
            }
            let vote = 1.0 / (60.0 + rank as f32 + 1.0);
            if let Some(&pos) = positions.get(&key) {
                merged[pos].score += vote;
            } else {
                positions.insert(key, merged.len());
                let mut hit = hit.clone();
                hit.score = vote;
                merged.push(hit);
            }
        }
    }
    merged.sort_by(|a, b| b.score.total_cmp(&a.score));
    merged.truncate(limit);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_core::{FtsQuery, Vault, VaultPath};
    use std::{fs, sync::Arc};

    // Count only allocations on the measured test thread, not zvec/background work.
    struct MeasuredAllocator;
    thread_local! {
        static ALLOCATED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }
    #[global_allocator]
    static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;
    // SAFETY: Accounting uses a thread-local counter without allocating. System owns the allocations.
    unsafe impl std::alloc::GlobalAlloc for MeasuredAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|count| {
                if let Some(n) = count.get() {
                    count.set(Some(n.saturating_add(layout.size())));
                }
            });
            // SAFETY: GlobalAlloc's caller supplies a valid allocation layout.
            unsafe { std::alloc::System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: The caller supplies the live pointer and layout from the corresponding allocation.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATED.with(|count| count.set(Some(0)));
        let value = f();
        let bytes = ALLOCATED.with(|count| count.replace(None).unwrap());
        (value, bytes)
    }

    fn unit(axis: usize) -> Vec<f32> {
        let mut v = vec![0.0; SEMANTIC_DIMENSION];
        v[axis] = 1.0;
        v
    }

    #[test]
    fn oversized_metadata_is_skipped_before_vector_storage_amplifies_it() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("oversized-title.md").unwrap();
        fs::write(
            path.to_fs(vault.root()),
            format!("---\ntitle: {}\n---\n\nbody\n", "T".repeat(1024 * 1024)),
        )
        .unwrap();
        assert!(index.prepare_note(&path).is_err());
        assert_eq!(index.stats(), (0, 0));
    }

    #[test]
    fn preparation_streams_bodies_without_multiplying_large_metadata() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("large-title.md").unwrap();
        let title = "T".repeat(8 * 1024);
        let metadata = format!("---\ntitle: {title}\ntags: [rust, '{}']\n---\n\n", "tag".repeat(1024));
        fs::write(path.to_fs(vault.root()), format!("{metadata}body\n\n")).unwrap();
        let (_, one) = measured(|| index.prepare_note(&path).unwrap().unwrap());
        fs::write(path.to_fs(vault.root()), format!("{metadata}{}", "body\n\n".repeat(64))).unwrap();
        let (plan, many) = measured(|| index.prepare_note(&path).unwrap().unwrap());
        assert_eq!(plan.chunks.len(), 64);
        assert!(many.saturating_sub(one) < 4 * 1024 * 1024, "one={one}, many={many}");
        for (ord, chunk) in plan.chunks.iter().enumerate() {
            assert_eq!(&*chunk.title, title);
            assert_eq!(chunk.block_ord, ord as u32);
            assert!(Arc::ptr_eq(&chunk.title, &plan.chunks[0].title));
            assert!(Arc::ptr_eq(&chunk.tags, &plan.chunks[0].tags));
        }
    }

    #[test]
    fn preparation_keeps_alias_only_behavior_and_core_ord_alignment() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("alias.md").unwrap();
        let metadata = "---\naliases: [Alias]\ntitle: Display\n---\n\n";
        fs::write(path.to_fs(vault.root()), metadata).unwrap();
        assert!(index.prepare_note(&path).unwrap().unwrap().chunks.is_empty());
        let text = format!("{metadata}<!-- hidden -->\n\nfirst<script>\n\nhidden\n\n</script>\n\nlast\n");
        fs::write(path.to_fs(vault.root()), &text).unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        let doc = Document::from_text(&text);
        let blocks = aero_core::index::blocks_for_note(&doc, "alias", "alias.md", "Display");
        assert_eq!(plan.chunks.len(), 2);
        for (chunk, block) in plan.chunks.iter().zip(blocks) {
            assert_eq!(chunk.block_ord, block.block_ord);
            assert_eq!(chunk.body, block.body);
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<Vault>, SemanticIndex) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("vault")).unwrap();
        let vault = Arc::new(Vault::open(dir.path().join("vault")).unwrap());
        let index = SemanticIndex::open(vault.clone(), &dir.path().join("data"), "test-model").unwrap();
        (dir, vault, index)
    }

    fn put(index: &mut SemanticIndex, path: &str, text: &str, axis: usize) -> SemanticPlan {
        let path = VaultPath::new(path).unwrap();
        fs::write(path.to_fs(index.vault.root()), text).unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        assert!(index.commit_note(&plan, &vec![unit(axis); plan.chunks.len()]).unwrap());
        plan
    }

    #[test]
    fn prepare_commit_reopen_update_remove() {
        fn assert_send<T: Send>() {}
        assert_send::<SemanticIndex>();
        let (dir, vault, mut index) = setup();
        let plan = put(&mut index, "a.md", "---\ntitle: Real title\n---\n\nfirst body\n\nsecond body\n", 0);
        assert_eq!(index.stats(), (1, 2));
        assert!(index.prepare_note(&plan.path).unwrap().is_none());
        assert_eq!(index.known_paths().unwrap(), std::slice::from_ref(&plan.path));
        let hits = index.search(&unit(0), &FtsQuery::new("meaning")).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Real title");
        assert!(hits.iter().all(|h| h.score > 0.99));
        index.flush().unwrap();
        drop(index);
        let mut index = SemanticIndex::open(vault, &dir.path().join("data"), "test-model").unwrap();
        assert_eq!(index.stats(), (1, 2));
        assert!(index.prepare_note(&plan.path).unwrap().is_none());
        put(&mut index, "a.md", "replacement\n", 1);
        assert_eq!(index.stats(), (1, 1));
        assert_eq!(index.search(&unit(1), &FtsQuery::new("x")).unwrap()[0].body, "replacement");
        assert_eq!(index.col.stats().unwrap().doc_count, 1);
        fs::remove_file(plan.path.to_fs(index.vault.root())).unwrap();
        assert!(index.search(&unit(1), &FtsQuery::new("x")).unwrap().is_empty());
        assert!(index.prepare_note(&plan.path).unwrap().is_none());
        index.remove_note(&plan.path).unwrap();
        assert_eq!(index.stats(), (0, 0));
        assert_eq!(index.col.stats().unwrap().doc_count, 0);
    }

    #[test]
    fn rejects_stale_and_invalid_embeddings_without_publishing() {
        let (_dir, _vault, mut index) = setup();
        let path = VaultPath::new("a.md").unwrap();
        fs::write(path.to_fs(index.vault.root()), "original").unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        assert!(index.commit_note(&plan, &[]).is_err());
        assert!(index.commit_note(&plan, &[vec![1.0; 3]]).is_err());
        assert!(index.commit_note(&plan, &[vec![f32::NAN; SEMANTIC_DIMENSION]]).is_err());
        assert!(index.commit_note(&plan, &[vec![f32::INFINITY; SEMANTIC_DIMENSION]]).is_err());
        fs::write(path.to_fs(index.vault.root()), "changed!").unwrap();
        assert!(!index.commit_note(&plan, &[unit(0)]).unwrap());
        assert_eq!(index.stats(), (0, 0));
        put(&mut index, "a.md", "committed", 0);
        fs::write(path.to_fs(index.vault.root()), "new disk!").unwrap();
        assert!(index.search(&unit(0), &FtsQuery::new("x")).unwrap().is_empty());
        assert!(index.prepare_note(&path).unwrap().is_some());
    }

    #[test]
    fn chunks_preserve_tail_utf8_plain_text_and_block_ord() {
        let (_dir, _vault, mut index) = setup();
        let text = format!(
            "---\ntitle: {}\n---\n\n# Heading\n\n> **bold** [label](https://example.com)\n\n{}TAIL中文\n",
            "题".repeat(300),
            "中文🌲 English words ".repeat(500)
        );
        let path = VaultPath::new("a.md").unwrap();
        fs::write(path.to_fs(index.vault.root()), &text).unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        assert!(plan.chunks.len() > 10);
        assert!(plan.chunks.len() < 100);
        assert!(plan.chunks.last().unwrap().body.contains("TAIL中文"));
        for chunk in &plan.chunks {
            assert!(chunk.text.chars().count() <= 700);
            assert!(estimated_units(&chunk.text) <= 200);
            assert!(!chunk.text.contains("**"));
            assert!(!chunk.text.contains("https://"));
            assert!(!chunk.text.contains("> "));
        }
        assert_eq!(plan.chunks[0].block_ord, 0);
        assert!(plan.chunks.iter().filter(|c| c.body.contains("English")).all(|c| c.block_ord == 2));
        assert!(index.commit_note(&plan, &vec![unit(0); plan.chunks.len()]).unwrap());
        let hits = index.search(&unit(0), &FtsQuery::new("x")).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|h| h.title == "题".repeat(300)));
    }

    #[test]
    fn tags_and_path_filter_before_limit_with_cosine_order() {
        let (_dir, _vault, mut index) = setup();
        assert!(index.col.schema().unwrap().has_index("path"));
        for i in 0..70 {
            put(&mut index, &format!("d{i}.md"), "---\ntags: [rust/cli, hidden/child]\n---\ndistractor", 0);
        }
        put(&mut index, "target.md", "---\ntags: [RuSt/web, ÉTÉ/学习]\n---\ntarget", 1);
        put(&mut index, "plain.md", "no tags", 1);
        let mut q = FtsQuery::new("meaning");
        q.limit = 1;
        q.tags = vec!["RUST".into(), "été".into()];
        assert_eq!(index.search(&unit(0), &q).unwrap()[0].path.as_str(), "target.md");
        q.tags = vec!["rust".into()];
        q.exclude_tags = vec!["HIDDEN".into()];
        assert_eq!(index.search(&unit(0), &q).unwrap()[0].path.as_str(), "target.md");
        q.exclude_tags = vec!["rust/web".into()];
        assert_ne!(index.search(&unit(0), &q).unwrap()[0].path.as_str(), "target.md");
        q.tags.clear();
        q.exclude_tags.clear();
        q.note_id = Some("target.md".into());
        assert_eq!(index.search(&unit(0), &q).unwrap()[0].path.as_str(), "target.md");
        q.note_id = Some("target".into());
        assert!(index.search(&unit(0), &q).unwrap().is_empty());
        q.note_id = None;
        q.limit = 100;
        let hits = index.search(&unit(0), &q).unwrap();
        assert!(hits[0].score > hits.last().unwrap().score);
        q.tags = vec!["%' OR 1=1 --".into()];
        assert!(index.search(&unit(0), &q).unwrap().is_empty());
    }

    // Large valid files, but only tiny semantic vectors/bodies: exercise validation
    // I/O without allocating/parsing thousands of large documents in the regression.
    fn put_large(index: &mut SemanticIndex, name: &str, chunks: usize, axis: usize) {
        let bytes = vec![b' '; MAX_NOTE_BYTES];
        let path = VaultPath::new(name).unwrap();
        fs::write(path.to_fs(index.vault.root()), &bytes).unwrap();
        let chunk = SemanticChunk {
            text: "body".into(),
            body: "body".into(),
            block_ord: 0,
            title: "Title".into(),
            tags: Arc::default(),
        };
        let plan = SemanticPlan { path, fingerprint: hash_bytes(&bytes), chunks: vec![chunk; chunks] };
        assert!(index.commit_note(&plan, &vec![unit(axis); chunks]).unwrap());
    }

    #[test]
    fn bounded_reads_stop_mid_file_and_charge_invalid_reads() {
        let (_dir, vault, index) = setup();
        let path = VaultPath::new("large.md").unwrap();
        let disk = path.to_fs(vault.root());
        fs::write(&disk, vec![b'a'; MAX_NOTE_BYTES]).unwrap();
        let mut remaining = MAX_SEARCH_BYTES;
        let calls = std::cell::Cell::new(0);
        assert!(matches!(
            index.read_note_bounded(&path, &mut remaining, &cancel_on(&calls, 4)).unwrap(),
            NoteRead::Cancelled
        ));
        assert_eq!(calls.get(), 4);
        assert_eq!(remaining, MAX_SEARCH_BYTES - READ_BATCH_BYTES);
        let mut insufficient = MAX_NOTE_BYTES - 1;
        assert!(matches!(
            index.read_note_bounded(&path, &mut insufficient, &|| false).unwrap(),
            NoteRead::BudgetExhausted
        ));
        assert_eq!(insufficient, MAX_NOTE_BYTES - 1);
        let calls = std::cell::Cell::new(0);
        assert!(
            index
                .read_note_bounded(&path, &mut remaining, &|| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 4 {
                        fs::write(&disk, "changed").unwrap();
                    }
                    false
                })
                .unwrap_err()
                .contains("changed while reading")
        );
        assert_eq!(remaining, MAX_SEARCH_BYTES - 2 * READ_BATCH_BYTES);
    }

    #[test]
    fn search_caps_total_file_validation_bytes() {
        let (_dir, _vault, mut index) = setup();
        for i in 0..20 {
            put_large(&mut index, &format!("large-{i}.md"), 1, 0);
        }
        let mut query = FtsQuery::new("meaning");
        query.limit = 20;
        let hits = index.search(&unit(0), &query).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.len() <= 16, "32 MiB must not validate twenty 2 MiB files: {}", hits.len());
    }

    #[test]
    fn search_reuses_validation_across_chunks_and_overfetch_and_can_cancel() {
        let (_dir, _vault, mut index) = setup();
        put_large(&mut index, "many-windows.md", 200, 0);
        put_large(&mut index, "other.md", 1, 1);
        let mut query = FtsQuery::new("meaning");
        query.limit = 2;
        let hits = index.search(&unit(0), &query).unwrap();
        assert_eq!(hits.len(), 2, "a path must not consume the file budget once per window/round");
        assert!(index.search_interruptible(&unit(0), &query, || true).unwrap().is_empty());
        for at in [8, 80] {
            let calls = std::cell::Cell::new(0);
            assert!(index.search_interruptible(&unit(0), &query, cancel_on(&calls, at)).unwrap().is_empty());
            assert_eq!(calls.get(), at, "stop immediately, including during overfetch");
        }
        assert_eq!(index.search_interruptible(&unit(0), &query, || false).unwrap().len(), 2);
    }

    #[test]
    fn live_snapshot_rejects_and_evicts_dynamically_ignored_notes() {
        let (_dir, vault, mut index) = setup();
        fs::create_dir(vault.root().join("drafts")).unwrap();
        let old = put(&mut index, "drafts/secret.md", &paragraphs("secret"), 0);
        put(&mut index, "public.md", "public", 1);
        index.set_live_paths(vault.notes().unwrap().into_iter().collect());
        let mut query = FtsQuery::new("meaning");
        query.note_id = Some(old.path.to_string());
        assert!(!index.search(&unit(0), &query).unwrap().is_empty());
        fs::write(old.path.to_fs(vault.root()), "updated secret").unwrap();
        let pending = index.prepare_note(&old.path).unwrap().unwrap();
        fs::write(vault.root().join(".aero/ignore"), "drafts/\n").unwrap();
        // The worker already rescans via this ignore-aware core API. Pass its result
        // once, never enumerate the vault from a candidate validation/query.
        let live = vault.notes().unwrap();
        assert_eq!(live, [VaultPath::new("public.md").unwrap()]);
        index.set_live_paths(live.into_iter().collect());
        assert!(index.read_note(&old.path).unwrap().is_none());
        assert!(index.search(&unit(0), &query).unwrap().is_empty());
        assert!(!index.commit_note(&pending, &[unit(0)]).unwrap());
        assert!(index.prepare_note_interruptible(&old.path, || false).unwrap().is_none());
        assert_eq!(index.stats(), (1, 1));
        assert_eq!(journal_count(&index), 1);
        fs::write(vault.root().join(".aero/ignore"), "").unwrap();
        index.set_live_paths(vault.notes().unwrap().into_iter().collect());
        let plan = index.prepare_note(&old.path).unwrap().unwrap();
        assert!(index.commit_note(&plan, &[unit(0)]).unwrap());
        assert_eq!(index.search(&unit(0), &query).unwrap().len(), 1);
    }

    #[test]
    fn refuses_unsafe_paths_and_oversized_files() {
        let (_dir, vault, mut index) = setup();
        for name in [".hidden.md", "file.txt", "a.assets/n.md", ".dir/n.md"] {
            assert!(index.prepare_note(&VaultPath::new(name).unwrap()).is_err());
        }
        let path = VaultPath::new("large.md").unwrap();
        fs::write(path.to_fs(vault.root()), vec![b'a'; MAX_NOTE_BYTES + 1]).unwrap();
        assert!(index.prepare_note(&path).unwrap_err().contains("2 MiB"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(vault.root().join("large.md"), vault.root().join("link.md")).unwrap();
            assert!(index.prepare_note(&VaultPath::new("link.md").unwrap()).is_err());
            std::os::unix::fs::symlink(vault.root(), vault.root().join("linked")).unwrap();
            assert!(index.prepare_note(&VaultPath::new("linked/large.md").unwrap()).is_err());
        }
    }

    #[test]
    fn model_reset_and_locked_open_leave_other_caches_alone() {
        let (dir, vault, mut index) = setup();
        put(&mut index, "a.md", "keep source", 0);
        let sentinel = dir.path().join("data").join(vault.id()).join("fts-sentinel");
        fs::write(&sentinel, "untouched").unwrap();
        assert!(SemanticIndex::open(vault.clone(), &dir.path().join("data"), "different").is_err());
        assert_eq!(index.stats(), (1, 1));
        assert_eq!(index.search(&unit(0), &FtsQuery::new("x")).unwrap().len(), 1);
        drop(index);
        let index = SemanticIndex::open(vault.clone(), &dir.path().join("data"), "different").unwrap();
        assert_eq!(index.stats(), (0, 0));
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(vault.root().join("a.md")).unwrap(), "keep source");
    }

    fn paragraphs(prefix: &str) -> String {
        (0..200).map(|i| format!("{prefix} paragraph {i}\n\n")).collect()
    }

    fn cancel_on(calls: &std::cell::Cell<usize>, at: usize) -> impl Fn() -> bool + '_ {
        move || {
            calls.set(calls.get() + 1);
            calls.get() == at
        }
    }

    fn journal_count(index: &SemanticIndex) -> usize {
        index.db.query_row("SELECT count(*) FROM vector_keys", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn interruptible_commit_checks_pause_before_writes_and_validates_vectors() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("a.md").unwrap();
        fs::write(path.to_fs(vault.root()), paragraphs("new")).unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        assert_eq!(plan.chunks.len(), 200);
        let vectors = vec![unit(0); plan.chunks.len()];
        let calls = std::cell::Cell::new(0);
        assert!(!index.commit_note_interruptible(&plan, &vectors, cancel_on(&calls, 1)).unwrap());
        assert_eq!(calls.get(), 1);
        assert_eq!(index.col.stats().unwrap().doc_count, 0);
        assert_eq!(journal_count(&index), 0);
        assert_eq!(index.stats(), (0, 0));
        assert!(index.prepare_note(&path).unwrap().is_some());
        for invalid in [
            vec![1.0; 3],
            vec![f32::NAN; SEMANTIC_DIMENSION],
            vec![f32::INFINITY; SEMANTIC_DIMENSION],
            vec![0.0; SEMANTIC_DIMENSION],
        ] {
            let mut invalid_vectors = vectors.clone();
            invalid_vectors[199] = invalid;
            assert!(index.commit_note_interruptible(&plan, &invalid_vectors, || false).is_err());
        }
        assert_eq!(index.col.stats().unwrap().doc_count, 0);
        assert_eq!(journal_count(&index), 0);
    }

    #[test]
    fn interruptible_commit_keeps_old_version_and_retries_every_write_boundary() {
        // Entry + four validation checks, four journal batches, four vector batches,
        // then flush and publication. No tags are registered for these plans.
        for (at, pending_keys, pending_vectors) in [
            (6, 0, 0),
            (7, 64, 0),
            (10, 200, 0),
            (11, 200, 64),
            (12, 200, 128),
            (13, 200, 192),
            (14, 200, 200),
            (15, 200, 200),
        ] {
            let (_dir, vault, mut index) = setup();
            let old_text = paragraphs("old");
            let old = put(&mut index, "a.md", &old_text, 0);
            let new_text = paragraphs("new");
            fs::write(old.path.to_fs(vault.root()), &new_text).unwrap();
            let plan = index.prepare_note(&old.path).unwrap().unwrap();
            assert_eq!(plan.chunks.len(), 200);
            let vectors = vec![unit(1); plan.chunks.len()];
            let calls = std::cell::Cell::new(0);
            assert!(
                !index.commit_note_interruptible(&plan, &vectors, cancel_on(&calls, at)).unwrap(),
                "{at}"
            );
            assert_eq!(calls.get(), at);
            assert_eq!(journal_count(&index), 200 + pending_keys, "{at}");
            assert_eq!(index.col.stats().unwrap().doc_count, 200 + pending_vectors, "{at}");
            assert_eq!(index.stats(), (1, 200));
            assert_eq!(index.note_state(&plan.path).unwrap(), Some((version(old.fingerprint), 200)));
            assert!(index.prepare_note(&plan.path).unwrap().is_some());
            let mut query = FtsQuery::new("meaning");
            query.limit = 200;
            assert!(index.search(&unit(1), &query).unwrap().is_empty());
            // A changed disk file is intentionally suppressed; restoring it proves the
            // original committed vectors are intact, and pending vectors cannot leak.
            fs::write(old.path.to_fs(vault.root()), &old_text).unwrap();
            let hits = index.search(&unit(0), &query).unwrap();
            assert_eq!(hits.len(), 200, "{at}");
            assert!(hits.iter().all(|h| h.body.starts_with("old paragraph")));
            fs::write(old.path.to_fs(vault.root()), &new_text).unwrap();
            assert!(index.commit_note_interruptible(&plan, &vectors, || false).unwrap());
            assert_eq!(index.stats(), (1, 200));
            assert_eq!(index.col.stats().unwrap().doc_count, 200);
            assert_eq!(journal_count(&index), 200);
            let hits = index.search(&unit(1), &query).unwrap();
            assert_eq!(hits.len(), 200, "{at}");
            assert!(hits.iter().all(|h| h.body.starts_with("new paragraph")));
        }
    }

    #[test]
    fn interruptible_commit_recovers_journal_on_reopen_and_rejects_stale_retry() {
        let (dir, vault, mut index) = setup();
        let old = put(&mut index, "a.md", "old body", 0);
        fs::write(old.path.to_fs(vault.root()), paragraphs("new")).unwrap();
        let plan = index.prepare_note(&old.path).unwrap().unwrap();
        let vectors = vec![unit(1); plan.chunks.len()];
        let calls = std::cell::Cell::new(0);
        assert!(!index.commit_note_interruptible(&plan, &vectors, cancel_on(&calls, 11)).unwrap());
        assert_eq!(index.col.stats().unwrap().doc_count, 65);
        drop(index);
        let mut index = SemanticIndex::open(vault.clone(), &dir.path().join("data"), "test-model").unwrap();
        assert_eq!(index.col.stats().unwrap().doc_count, 1);
        assert_eq!(journal_count(&index), 1);
        assert!(index.prepare_note(&plan.path).unwrap().is_some());
        assert!(index.commit_note_interruptible(&plan, &vectors, || false).unwrap());
        assert_eq!(index.col.stats().unwrap().doc_count, 200);
        let winner = put(&mut index, "a.md", "externally changed body", 2);
        assert!(!index.commit_note_interruptible(&plan, &vectors, || false).unwrap());
        assert_eq!(index.note_state(&plan.path).unwrap(), Some((version(winner.fingerprint), 1)));
        assert_eq!(index.stats(), (1, 1));
        assert_eq!(index.col.stats().unwrap().doc_count, 1);
        assert_eq!(index.search(&unit(2), &FtsQuery::new("x")).unwrap()[0].body, "externally changed body");
    }

    #[test]
    fn interruptible_commit_rechecks_external_edits_before_publication() {
        for edit_at in [11, 15] {
            let (_dir, vault, mut index) = setup();
            let old = put(&mut index, "a.md", "old body", 0);
            let disk = old.path.to_fs(vault.root());
            fs::write(&disk, paragraphs("new")).unwrap();
            let plan = index.prepare_note(&old.path).unwrap().unwrap();
            let vectors = vec![unit(1); plan.chunks.len()];
            let calls = std::cell::Cell::new(0);
            assert!(
                !index
                    .commit_note_interruptible(&plan, &vectors, || {
                        calls.set(calls.get() + 1);
                        if calls.get() == edit_at {
                            fs::write(&disk, "external edit").unwrap();
                        }
                        false
                    })
                    .unwrap()
            );
            assert_eq!(index.note_state(&plan.path).unwrap(), Some((version(old.fingerprint), 1)));
            assert_eq!(index.stats(), (1, 1));
            assert!(index.search(&unit(1), &FtsQuery::new("x")).unwrap().is_empty());
            let winner = index.prepare_note(&old.path).unwrap().unwrap();
            assert!(index.commit_note(&winner, &[unit(2)]).unwrap());
            assert!(!index.commit_note_interruptible(&plan, &vectors, || false).unwrap());
            assert_eq!(index.search(&unit(2), &FtsQuery::new("x")).unwrap()[0].body, "external edit");
        }
    }

    #[test]
    fn interruptible_commit_cleanup_cancellation_reports_publication_and_keeps_journal() {
        // Cleanup checks before selecting/deleting a batch, flushing deletes, and
        // deleting journal entries; interruption must never undo publication.
        for (at, obsolete_keys) in [(16, 200), (17, 200), (18, 200), (19, 200), (20, 136)] {
            let (_dir, vault, mut index) = setup();
            let old = put(&mut index, "a.md", &paragraphs("old"), 0);
            fs::write(old.path.to_fs(vault.root()), paragraphs("new")).unwrap();
            let plan = index.prepare_note(&old.path).unwrap().unwrap();
            let vectors = vec![unit(1); plan.chunks.len()];
            let calls = std::cell::Cell::new(0);
            assert!(index.commit_note_interruptible(&plan, &vectors, cancel_on(&calls, at)).unwrap());
            assert_eq!(calls.get(), at);
            assert_eq!(index.stats(), (1, 200));
            assert_eq!(journal_count(&index), 200 + obsolete_keys);
            let mut query = FtsQuery::new("meaning");
            query.limit = 200;
            let hits = index.search(&unit(1), &query).unwrap();
            assert_eq!(hits.len(), 200);
            assert!(hits.iter().all(|h| h.body.starts_with("new paragraph")));
            assert!(index.commit_note_interruptible(&plan, &vectors, || false).unwrap());
            assert_eq!(journal_count(&index), 200);
            assert_eq!(index.col.stats().unwrap().doc_count, 200);
        }
    }

    #[test]
    fn recovery_can_pause_without_losing_durable_journal_and_retry() {
        let (dir, vault, mut index) = setup();
        let old = put(&mut index, "a.md", "old", 0);
        fs::write(old.path.to_fs(vault.root()), paragraphs("new")).unwrap();
        let plan = index.prepare_note(&old.path).unwrap().unwrap();
        let calls = std::cell::Cell::new(0);
        assert!(!index.commit_note_interruptible(&plan, &vec![unit(1); 200], cancel_on(&calls, 11)).unwrap());
        assert_eq!(journal_count(&index), 201);
        drop(index);
        // Cancel once recovery reaches its first batch. Opening/closing must not
        // silently finish the remaining journal with a never-cancel callback.
        let db_path = dir.path().join("data").join(vault.id()).join("semantic/metadata.sqlite");
        let deferred = SemanticIndex::open_interruptible(
            vault.clone(),
            &dir.path().join("data"),
            "test-model",
            false,
            || false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(journal_count(&deferred), 201, "query-only open must leave cleanup to idle indexing");
        drop(deferred);
        let calls = std::cell::Cell::new(0);
        let paused = SemanticIndex::open_interruptible(
            vault.clone(),
            &dir.path().join("data"),
            "test-model",
            true,
            || {
                calls.set(calls.get() + 1);
                calls.get() >= 6
            },
        )
        .unwrap();
        assert!(paused.is_none());
        let db = Connection::open(&db_path).unwrap();
        let pending: usize = db.query_row("SELECT count(*) FROM vector_keys", [], |r| r.get(0)).unwrap();
        assert!(pending > 1);
        drop(db);
        let mut index =
            SemanticIndex::open_interruptible(vault, &dir.path().join("data"), "test-model", true, || false)
                .unwrap()
                .unwrap();
        assert_eq!(journal_count(&index), 1);
        assert_eq!(index.stats(), (1, 1));
        assert!(index.commit_note(&plan, &vec![unit(1); 200]).unwrap());
        assert_eq!(index.stats(), (1, 200));
    }

    #[test]
    fn remove_and_flush_can_pause_at_every_cleanup_boundary() {
        for at in 1..=6 {
            let (_dir, _vault, mut index) = setup();
            let plan = put(&mut index, "a.md", &paragraphs("body"), 0);
            let calls = std::cell::Cell::new(0);
            let removed = index.remove_note_interruptible(&plan.path, cancel_on(&calls, at)).unwrap();
            assert_eq!(calls.get(), at);
            assert_eq!(removed, at != 1, "unpublication is the remove commit point");
            assert_eq!(index.stats(), if at == 1 { (1, 200) } else { (0, 0) });
            assert!(journal_count(&index) > 0);
            assert!(!index.flush_interruptible(|| true).unwrap());
            assert!(index.remove_note_interruptible(&plan.path, || false).unwrap());
            assert!(index.flush_interruptible(|| false).unwrap());
            assert_eq!(journal_count(&index), 0);
            assert_eq!(index.col.stats().unwrap().doc_count, 0);
        }
    }

    #[test]
    fn flush_can_defer_each_cleanup_boundary_then_reopen() {
        for at in 1..=6 {
            let (dir, vault, mut index) = setup();
            let plan = put(&mut index, "a.md", &paragraphs("body"), 0);
            let calls = std::cell::Cell::new(0);
            assert!(index.remove_note_interruptible(&plan.path, cancel_on(&calls, 2)).unwrap());
            let calls = std::cell::Cell::new(0);
            assert!(!index.flush_interruptible(cancel_on(&calls, at)).unwrap());
            assert_eq!(calls.get(), at);
            assert_eq!(journal_count(&index), if at <= 4 { 200 } else { 136 });
            assert_eq!(index.stats(), (0, 0));
            drop(index);
            let index = SemanticIndex::open(vault, &dir.path().join("data"), "test-model").unwrap();
            assert_eq!(journal_count(&index), 0);
            assert_eq!(index.col.stats().unwrap().doc_count, 0);
        }
    }

    #[test]
    fn prepare_removal_and_streaming_respect_cancellation() {
        let (_dir, vault, mut index) = setup();
        let plan = put(&mut index, "a.md", &paragraphs("body"), 0);
        fs::remove_file(plan.path.to_fs(vault.root())).unwrap();
        assert!(index.prepare_note_interruptible(&plan.path, || true).unwrap().is_none());
        assert_eq!(index.stats(), (1, 200));
        // Cancel the cleanup after unpublishing the missing file.
        let calls = std::cell::Cell::new(0);
        assert!(index.prepare_note_interruptible(&plan.path, cancel_on(&calls, 8)).unwrap().is_none());
        assert!(journal_count(&index) > 0);
        assert!(index.flush_interruptible(|| false).unwrap());
        assert_eq!(journal_count(&index), 0);
        fs::write(plan.path.to_fs(vault.root()), paragraphs("new")).unwrap();
        let calls = std::cell::Cell::new(0);
        assert!(index.prepare_note_interruptible(&plan.path, cancel_on(&calls, 12)).unwrap().is_none());
        assert_eq!(calls.get(), 12);
        assert_eq!(
            index.prepare_note_interruptible(&plan.path, || false).unwrap().unwrap().chunks.len(),
            200
        );
    }

    #[test]
    fn failed_metadata_publication_hides_vectors_and_can_retry_after_reopen() {
        let (dir, vault, mut index) = setup();
        let old = put(&mut index, "a.md", "old body", 0);
        fs::write(old.path.to_fs(vault.root()), "new body").unwrap();
        let plan = index.prepare_note(&old.path).unwrap().unwrap();
        index
            .db
            .execute_batch(
                "CREATE TRIGGER fail_publish BEFORE INSERT ON notes
            BEGIN SELECT RAISE(FAIL, 'test publication failure'); END;",
            )
            .unwrap();
        assert!(index.commit_note(&plan, &[unit(1)]).unwrap_err().contains("test publication failure"));
        assert_eq!(index.stats(), (1, 1));
        assert_eq!(index.col.stats().unwrap().doc_count, 2);
        assert!(index.search(&unit(1), &FtsQuery::new("x")).unwrap().is_empty());
        assert!(index.prepare_note(&old.path).unwrap().is_some());
        index.db.execute_batch("DROP TRIGGER fail_publish;").unwrap();
        drop(index);
        let mut index = SemanticIndex::open(vault, &dir.path().join("data"), "test-model").unwrap();
        assert_eq!(index.col.stats().unwrap().doc_count, 1);
        assert!(index.commit_note(&plan, &[unit(1)]).unwrap());
        assert_eq!(index.col.stats().unwrap().doc_count, 1);
        assert_eq!(index.search(&unit(1), &FtsQuery::new("x")).unwrap()[0].body, "new body");
    }

    #[test]
    fn batched_upsert_failure_is_unpublished_and_retry_cleans_all_keys() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("雪 '引用'.md").unwrap();
        let text = "paragraph body\n\n".repeat(130);
        fs::write(path.to_fs(vault.root()), &text).unwrap();
        let mut plan = index.prepare_note(&path).unwrap().unwrap();
        assert_eq!(plan.chunks.len(), 130);
        let vectors = vec![unit(0); plan.chunks.len()];
        let body = plan.chunks[64].body.clone();
        plan.chunks[64].body = "invalid\0C string".into();
        assert!(index.commit_note(&plan, &vectors).is_err());
        assert_eq!(index.col.stats().unwrap().doc_count, 64);
        assert_eq!(index.stats(), (0, 0));
        assert!(index.search(&unit(0), &FtsQuery::new("x")).unwrap().is_empty());
        plan.chunks[64].body = body;
        assert!(index.commit_note(&plan, &vectors).unwrap());
        assert_eq!(index.stats(), (1, 130));
        let mut q = FtsQuery::new("x");
        q.note_id = Some(path.to_string());
        assert!(!index.search(&unit(0), &q).unwrap().is_empty());
        index.remove_note(&path).unwrap();
        assert_eq!(index.col.stats().unwrap().doc_count, 0);
        assert_eq!(fs::read_to_string(path.to_fs(vault.root())).unwrap(), text);
    }

    #[test]
    fn maximum_size_note_has_bounded_windows_and_reaches_the_tail() {
        let (_dir, vault, mut index) = setup();
        let path = VaultPath::new("large.md").unwrap();
        let text = format!("{}TAIL", "word ".repeat((MAX_NOTE_BYTES - 4) / 5));
        fs::write(path.to_fs(vault.root()), text).unwrap();
        let plan = index.prepare_note(&path).unwrap().unwrap();
        assert!((1000..=MAX_CHUNKS).contains(&plan.chunks.len()));
        assert!(plan.chunks.last().unwrap().body.ends_with("TAIL"));
        assert!(plan.chunks.iter().all(|c| c.text.chars().count() <= 700 && estimated_units(&c.text) <= 200));
        assert_eq!(index.stats(), (0, 0));
    }

    #[test]
    fn empty_notes_commit_and_engine_errors_never_delete_cache_files() {
        let (dir, vault, mut index) = setup();
        put(&mut index, "empty.md", "", 0);
        assert_eq!(index.stats(), (1, 0));
        assert!(index.prepare_note(&VaultPath::new("empty.md").unwrap()).unwrap().is_none());
        drop(index);
        let vectors = dir.path().join("data").join(vault.id()).join("semantic/vectors");
        let col = Collection::open(&vectors.to_string_lossy(), None).unwrap();
        assert!(SemanticIndex::open(vault.clone(), &dir.path().join("data"), "changed").is_err());
        assert!(vectors.exists());
        col.close().unwrap();
        let index = SemanticIndex::open(vault, &dir.path().join("data"), "test-model").unwrap();
        assert_eq!(index.stats(), (1, 0));
    }

    fn hit(path: &str, ord: u32, body: &str) -> crate::SearchHit {
        crate::SearchHit {
            path: VaultPath::new(path).unwrap(),
            title: path.into(),
            block_ord: ord,
            body: body.into(),
            score: 1.0,
        }
    }

    #[test]
    fn rrf_deduplicates_each_route_and_stably_prefers_fts() {
        let a = hit("a.md", 0, "FTS body");
        let b = hit("b.md", 0, "B");
        let c = hit("c.md", 0, "C");
        let fused =
            fuse_results(&[a.clone(), b.clone(), a.clone()], &[c.clone(), hit("a.md", 0, "semantic")], 10);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].body, "FTS body");
        assert!((fused[0].score - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-6);
        assert_eq!(fuse_results(std::slice::from_ref(&a), std::slice::from_ref(&c), 1)[0].path, a.path);
        assert_eq!(fuse_results(&[b.clone(), a.clone()], &[a, b], 10)[0].path.as_str(), "b.md");
        assert!(fuse_results(&[], &[c], 0).is_empty());
    }
}
