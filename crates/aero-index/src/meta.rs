//! 元数据库（方案 10.2 的 M1b 子集）：每篇笔记的 `note_id`、路径、标题、文件指纹与已索引块数，
//! 以及 M2 的 wikilink 关系表 `links` 与笔记级标签目录 `note_tags`。
//! 它是索引状态的权威（10.5）：启动时用指纹决定哪些文件要重索引；保存时用块数算出要删的旧 pk。
//! 附件表随 M2 后续加入。

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aero_core::FileStamp;
use aero_core::links::LinkSpan;
use rusqlite::{Connection, OptionalExtension, params};

/// 6: attachment reference targets live in the index, so the file tree does not rescan every note.
const SCHEMA_VERSION: i64 = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagCount {
    pub tag: String,
    pub note_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRow {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub stamp: FileStamp,
    pub block_count: u32,
}

/// 反链来源及其归一化文档中的 UTF-8 字节区间。
pub struct BacklinkRow {
    pub source: NoteRow,
    pub range: std::ops::Range<usize>,
}

/// 一条还没解析到目标的链接行：`(源 note_id, 源路径, target, fragment, byte_start)`。
pub type PendingLink = (String, String, String, Option<String>, i64);

/// 测试用的链接行只读视图：`(target, resolved_note_id, fragment)`。
#[cfg(test)]
pub type LinkRow = (String, Option<String>, Option<String>);

pub struct Meta {
    conn: Connection,
}

impl Meta {
    /// 打开（或建立）`path` 处的数据库；版本不符时整库重建——索引状态可以随时从文件重建。
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        // cache_size 负数是 KiB：32 MB 页缓存，十万篇的 links 索引维护不至于反复读盘
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA cache_size = -32768;",
        )?;
        let mut meta = Meta { conn };
        if meta.version()? != SCHEMA_VERSION {
            meta.reset()?;
        }
        Ok(meta)
    }

    #[cfg(test)]
    pub fn in_memory() -> rusqlite::Result<Self> {
        let mut meta = Meta { conn: Connection::open_in_memory()? };
        meta.reset()?;
        Ok(meta)
    }

    fn version(&self) -> rusqlite::Result<i64> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'index_state'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;
        if !exists {
            return Ok(0);
        }
        self.conn
            .query_row("SELECT version FROM index_state WHERE name = 'meta'", [], |r| r.get(0))
            .optional()
            .map(|v| v.unwrap_or(0))
    }

    /// 清空全部状态（全量重建前调用）。
    pub fn reset(&mut self) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS notes;
             DROP TABLE IF EXISTS links;
             DROP TABLE IF EXISTS note_tags;
             DROP TABLE IF EXISTS attachment_refs;
             DROP TABLE IF EXISTS index_state;
             CREATE TABLE notes (
                 note_id     TEXT PRIMARY KEY,
                 path        TEXT NOT NULL UNIQUE,
                 title       TEXT NOT NULL,
                 mtime_ns    INTEGER NOT NULL,
                 size        INTEGER NOT NULL,
                 hash        INTEGER NOT NULL,
                 block_count INTEGER NOT NULL,
                 aliases     TEXT NOT NULL
             );
             CREATE TABLE links (
                 source_note_id  TEXT NOT NULL,
                 target          TEXT NOT NULL,
                 resolved_note_id TEXT,
                 fragment        TEXT,
                 byte_start      INTEGER NOT NULL,
                 byte_end        INTEGER NOT NULL
             );
             CREATE INDEX links_source_idx ON links(source_note_id);
             CREATE INDEX links_target_idx ON links(resolved_note_id);
             CREATE TABLE note_tags (
                 note_id TEXT NOT NULL,
                 tag TEXT NOT NULL,
                 PRIMARY KEY(note_id, tag)
             );
             CREATE INDEX note_tags_tag_idx ON note_tags(tag);
             CREATE TABLE attachment_refs (
                 source_note_id TEXT NOT NULL,
                 target TEXT NOT NULL
             );
             CREATE INDEX attachment_refs_source_idx ON attachment_refs(source_note_id);
             CREATE TABLE index_state (name TEXT PRIMARY KEY, version INTEGER NOT NULL);
             INSERT INTO index_state (name, version) VALUES ('meta', {SCHEMA_VERSION});"
        ))?;
        tx.commit()
    }

    pub fn fts_version(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT version FROM index_state WHERE name = 'fts'", [], |r| r.get(0))
            .optional()
            .map(|v| v.unwrap_or(0))
    }

    pub fn set_fts_version(&mut self, version: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO index_state (name, version) VALUES ('fts', ?1)
             ON CONFLICT(name) DO UPDATE SET version = excluded.version",
            [version],
        )?;
        Ok(())
    }

    pub fn note_by_path(&self, path: &str) -> rusqlite::Result<Option<NoteRow>> {
        self.conn
            .query_row(
                "SELECT note_id, path, title, mtime_ns, size, hash, block_count, aliases FROM notes WHERE path = ?1",
                params![path],
                row_to_note,
            )
            .optional()
    }

    pub fn all_notes(&self) -> rusqlite::Result<Vec<NoteRow>> {
        let mut st = self.conn.prepare(
            "SELECT note_id, path, title, mtime_ns, size, hash, block_count, aliases FROM notes ORDER BY path",
        )?;
        let rows = st.query_map([], row_to_note)?;
        rows.collect()
    }

    pub fn upsert_note(&self, row: &NoteRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO notes (note_id, path, title, mtime_ns, size, hash, block_count, aliases)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(note_id) DO UPDATE SET
                 path = excluded.path, title = excluded.title, mtime_ns = excluded.mtime_ns,
                 size = excluded.size, hash = excluded.hash, block_count = excluded.block_count,
                 aliases = excluded.aliases",
            params![
                row.note_id,
                row.path,
                row.title,
                mtime_ns(row.stamp.mtime),
                row.stamp.size as i64,
                row.stamp.hash as i64,
                row.block_count as i64,
                aero_core::document::encode_aliases(&row.aliases)
            ],
        )?;
        Ok(())
    }

    pub fn replace_attachment_targets(&mut self, note_id: &str, targets: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM attachment_refs WHERE source_note_id = ?1", params![note_id])?;
        let mut insert =
            tx.prepare_cached("INSERT INTO attachment_refs (source_note_id, target) VALUES (?1, ?2)")?;
        for target in targets {
            insert.execute(params![note_id, target])?;
        }
        drop(insert);
        tx.commit()
    }

    /// `(笔记路径, 原文里的附件目标)`。
    pub fn attachment_targets(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.path, a.target FROM attachment_refs a JOIN notes n ON n.note_id = a.source_note_id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    /// 指向这篇笔记的链接：`(源路径, target, 字节起, 字节止)`。区间是整个 `[[...]]`。
    pub fn incoming_link_locations(
        &self,
        note_id: &str,
    ) -> rusqlite::Result<Vec<(String, String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.path, l.target, l.byte_start, l.byte_end
             FROM links l JOIN notes n ON n.note_id = l.source_note_id
             WHERE l.resolved_note_id = ?1",
        )?;
        let rows =
            stmt.query_map(params![note_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
        rows.collect()
    }

    pub fn remove_note(&mut self, note_id: &str) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM note_tags WHERE note_id = ?1", params![note_id])?;
        tx.execute("DELETE FROM attachment_refs WHERE source_note_id = ?1", params![note_id])?;
        tx.execute("DELETE FROM notes WHERE note_id = ?1", params![note_id])?;
        tx.commit()
    }

    /// 笔记指纹与标签同时提交；失败时旧指纹仍可促使下一次同步重试。
    pub fn upsert_note_with_tags(&mut self, row: &NoteRow, tags: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        self.upsert_note(row)?;
        tx.execute("DELETE FROM note_tags WHERE note_id = ?1", [&row.note_id])?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR IGNORE INTO note_tags(note_id, tag) VALUES (?1, ?2)")?;
            for tag in tags {
                insert.execute(params![row.note_id, tag])?;
            }
        }
        tx.commit()
    }

    /// 每篇笔记至多计一次，只统计实际使用的标签，不额外制造祖先标签。
    /// 排除当前笔记后，调用方可用未保存版本的标签补回。
    pub fn tag_counts(&self, exclude_path: Option<&str>) -> rusqlite::Result<Vec<TagCount>> {
        let mut st = self.conn.prepare(
            "SELECT t.tag, count(*) FROM note_tags t JOIN notes n ON n.note_id = t.note_id
             WHERE (?1 IS NULL OR n.path != ?1) GROUP BY t.tag ORDER BY count(*) DESC, t.tag",
        )?;
        st.query_map([exclude_path], |r| Ok(TagCount { tag: r.get(0)?, note_count: r.get(1)? }))?.collect()
    }

    /// 只改路径（重命名 / 移动），`note_id` 与块不变。
    pub fn rename_note(&mut self, note_id: &str, new_path: &str, new_title: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE notes SET path = ?2, title = ?3 WHERE note_id = ?1",
            params![note_id, new_path, new_title],
        )?;
        Ok(())
    }

    pub fn note_count(&self) -> rusqlite::Result<u64> {
        self.conn.query_row("SELECT count(*) FROM notes", [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
    }

    /// 按来源路径、出现顺序列出入链。自身链接不属于笔记间的反链。
    pub fn backlink_count(&self, path: &str, fragment: Option<&str>) -> rusqlite::Result<u64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM notes dst JOIN links l ON l.resolved_note_id = dst.note_id
             JOIN notes n ON n.note_id = l.source_note_id
             WHERE dst.path = ?1 AND n.note_id != dst.note_id AND (?2 IS NULL OR l.fragment = ?2)",
            params![path, fragment],
            |r| r.get(0),
        )
    }

    pub fn backlinks(
        &self,
        path: &str,
        fragment: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<BacklinkRow>> {
        let mut st = self.conn.prepare(
            "SELECT n.note_id, n.path, n.title, n.mtime_ns, n.size, n.hash, n.block_count, n.aliases,
                    l.byte_start, l.byte_end
             FROM notes dst JOIN links l ON l.resolved_note_id = dst.note_id
             JOIN notes n ON n.note_id = l.source_note_id
             WHERE dst.path = ?1 AND n.note_id != dst.note_id AND (?2 IS NULL OR l.fragment = ?2)
             ORDER BY n.path, l.byte_start LIMIT ?3",
        )?;
        st.query_map(params![path, fragment, limit.min(i64::MAX as usize) as i64], |r| {
            Ok(BacklinkRow { source: row_to_note(r)?, range: r.get(8)?..r.get(9)? })
        })?
        .collect()
    }

    /// 重建一篇笔记的全部链接行：先删旧行，再按扫描区间插入。解析在调用方（`Indexer`）做，
    /// 这里只存 `(span, 解析到的 note_id)`；没解析到的行 `resolved_note_id` 为 NULL，
    /// 由 [`Self::pending_links`] + [`Self::set_link_resolved`] 在目标入库后补上。
    pub fn replace_links<'a>(
        &mut self,
        source_note_id: &str,
        links: impl IntoIterator<Item = (&'a LinkSpan, Option<&'a str>)>,
    ) -> rusqlite::Result<()> {
        let rows: Vec<(LinkSpan, Option<String>)> =
            links.into_iter().map(|(s, id)| (s.clone(), id.map(str::to_owned))).collect();
        self.replace_links_many([(source_note_id, rows.as_slice())])
    }

    /// 多篇的 [`Self::replace_links`] 合成一个事务（全量同步分批写：逐篇提交时每篇都要把
    /// 表与两个索引的脏页写进 WAL，十万篇下来是十几秒）。
    pub fn replace_links_many<'a>(
        &mut self,
        notes: impl IntoIterator<Item = (&'a str, &'a [(LinkSpan, Option<String>)])>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut delete = tx.prepare_cached("DELETE FROM links WHERE source_note_id = ?1")?;
            let mut insert = tx.prepare_cached(
                "INSERT INTO links (source_note_id, target, resolved_note_id, fragment, byte_start, byte_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (source_note_id, rows) in notes {
                delete.execute(params![source_note_id])?;
                for (span, resolved) in rows {
                    insert.execute(params![
                        source_note_id,
                        span.target,
                        resolved,
                        span.fragment,
                        span.range.start as i64,
                        span.range.end as i64
                    ])?;
                }
            }
        }
        tx.commit()
    }

    /// 笔记被删时清掉它的链接行。
    pub fn remove_links(&mut self, note_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM links WHERE source_note_id = ?1", params![note_id])?;
        Ok(())
    }

    /// 指向某篇笔记的链接行全部退回未解析（它被删、改名或改标题时调用）；随后的补挂流程
    /// 会把仍然能解析到的行重新挂上，解析不到的留为 NULL。返回受影响的行数。
    pub fn unresolve_incoming(&mut self, note_id: &str) -> rusqlite::Result<usize> {
        self.conn
            .execute("UPDATE links SET resolved_note_id = NULL WHERE resolved_note_id = ?1", params![note_id])
    }

    /// 还没解析到目标的行：`(源 note_id, 源路径, target, fragment, byte_start)`。
    /// 全量同步或新笔记索引后重新解析这些行，解决"源比目标先索引"的顺序问题。
    pub fn pending_links(&self) -> rusqlite::Result<Vec<PendingLink>> {
        let mut st = self.conn.prepare(
            "SELECT l.source_note_id, n.path, l.target, l.fragment, l.byte_start
             FROM links l JOIN notes n ON n.note_id = l.source_note_id
             WHERE l.resolved_note_id IS NULL ORDER BY l.byte_start",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        rows.collect()
    }

    /// 指向这些笔记的链接行 `(当前 resolved_note_id, 行)`：名字有竞争的目标入库后要重判。
    pub fn links_resolved_to(&self, note_ids: &[String]) -> rusqlite::Result<Vec<(String, PendingLink)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT l.source_note_id, n.path, l.target, l.fragment, l.byte_start
             FROM links l JOIN notes n ON n.note_id = l.source_note_id
             WHERE l.resolved_note_id = ?1 ORDER BY l.byte_start",
        )?;
        let mut out = Vec::new();
        for id in note_ids {
            let rows =
                st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            for row in rows {
                out.push((id.clone(), row?));
            }
        }
        Ok(out)
    }

    /// 指向已不存在笔记的行退回未解析（上次全量同步中途被取消时可能留下预分配的 id）。返回行数。
    pub fn clear_dangling_targets(&mut self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE links SET resolved_note_id = NULL
             WHERE resolved_note_id IS NOT NULL AND resolved_note_id NOT IN (SELECT note_id FROM notes)",
            [],
        )
    }

    /// 改写一批行的解析结果（按源 note_id + 源内字节起点定位行；`None` = 退回未解析），一个事务写完。
    pub fn set_links_resolved<'a>(
        &mut self,
        updates: impl IntoIterator<Item = (&'a str, i64, Option<&'a str>)>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut update = tx.prepare_cached(
                "UPDATE links SET resolved_note_id = ?3 WHERE source_note_id = ?1 AND byte_start = ?2",
            )?;
            for (source_note_id, byte_start, resolved_note_id) in updates {
                update.execute(params![source_note_id, byte_start, resolved_note_id])?;
            }
        }
        tx.commit()
    }

    /// 测试用的只读视图：全部链接行，按源字节区间排序。
    #[cfg(test)]
    pub fn links_rows(&self) -> rusqlite::Result<Vec<LinkRow>> {
        let mut st =
            self.conn.prepare("SELECT target, resolved_note_id, fragment FROM links ORDER BY byte_start")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }
}

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    let ns: i64 = r.get(3)?;
    Ok(NoteRow {
        note_id: r.get(0)?,
        path: r.get(1)?,
        title: r.get(2)?,
        aliases: aero_core::document::decode_aliases(&r.get::<_, String>(7)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        stamp: FileStamp {
            mtime: UNIX_EPOCH + Duration::from_nanos(ns.max(0) as u64),
            size: r.get::<_, i64>(4)? as u64,
            hash: r.get::<_, i64>(5)? as u64,
        },
        block_count: r.get::<_, i64>(6)? as u32,
    })
}

fn mtime_ns(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_persist_reopen_update_remove_and_reset_old_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.sqlite");
        let mut m = Meta::open(&db).unwrap();
        let mut row = NoteRow {
            note_id: "n".into(),
            path: "n.md".into(),
            title: "Display".into(),
            aliases: vec![
                "雪 山".into(),
                "a|b".into(),
                "a,b".into(),
                "line\nbreak".into(),
                "quoted \"yes\"".into(),
            ],
            stamp: stamp(1),
            block_count: 1,
        };
        m.upsert_note_with_tags(&row, &["rust".into()]).unwrap();
        drop(m);
        let mut m = Meta::open(&db).unwrap();
        assert_eq!(m.note_by_path("n.md").unwrap(), Some(row.clone()));
        assert_eq!(m.all_notes().unwrap(), [row.clone()]);
        row.aliases = vec!["New".into()];
        row.title = "Retitled".into();
        row.stamp = stamp(2);
        m.upsert_note_with_tags(&row, &["new".into()]).unwrap();
        assert_eq!(m.note_by_path("n.md").unwrap(), Some(row.clone()));
        m.remove_note("n").unwrap();
        assert!(m.all_notes().unwrap().is_empty());
        m.upsert_note(&row).unwrap();
        m.conn.execute("UPDATE index_state SET version = 4 WHERE name = 'meta'", []).unwrap();
        drop(m);
        assert!(Meta::open(&db).unwrap().all_notes().unwrap().is_empty());
    }

    #[test]
    fn tag_counts_replace_deduplicate_exclude_and_remove_notes() {
        let mut m = Meta::in_memory().unwrap();
        let a = NoteRow {
            note_id: "a".into(),
            path: "a.md".into(),
            title: "A".into(),
            aliases: Vec::new(),
            stamp: stamp(1),
            block_count: 0,
        };
        let b = NoteRow { note_id: "b".into(), path: "b.md".into(), ..a.clone() };
        m.upsert_note_with_tags(&a, &["rust".into(), "rust".into(), "中文/工具".into()]).unwrap();
        m.upsert_note_with_tags(&b, &["rust".into()]).unwrap();
        assert_eq!(
            m.tag_counts(None).unwrap(),
            [
                TagCount { tag: "rust".into(), note_count: 2 },
                TagCount { tag: "中文/工具".into(), note_count: 1 }
            ]
        );
        assert_eq!(m.tag_counts(Some("a.md")).unwrap(), [TagCount { tag: "rust".into(), note_count: 1 }]);
        m.upsert_note(&NoteRow { stamp: stamp(2), ..a.clone() }).unwrap();
        assert_eq!(m.tag_counts(None).unwrap()[0].note_count, 2, "stamp-only update preserves tags");
        m.upsert_note_with_tags(&a, &["rust/cli".into()]).unwrap();
        m.rename_note("a", "renamed.md", "A").unwrap();
        assert_eq!(
            m.tag_counts(Some("renamed.md")).unwrap(),
            [TagCount { tag: "rust".into(), note_count: 1 }]
        );
        m.remove_note("b").unwrap();
        assert_eq!(m.tag_counts(None).unwrap(), [TagCount { tag: "rust/cli".into(), note_count: 1 }]);
        m.reset().unwrap();
        assert!(m.tag_counts(None).unwrap().is_empty());
    }

    #[test]
    fn tag_write_failure_rolls_back_note_stamp_and_previous_tags() {
        let mut m = Meta::in_memory().unwrap();
        let row = NoteRow {
            note_id: "a".into(),
            path: "a.md".into(),
            title: "A".into(),
            aliases: Vec::new(),
            stamp: stamp(1),
            block_count: 1,
        };
        m.upsert_note_with_tags(&row, &["old".into()]).unwrap();
        m.conn.execute_batch("CREATE TRIGGER deny_tag BEFORE INSERT ON note_tags WHEN NEW.tag = 'fail' BEGIN SELECT RAISE(ABORT, 'test'); END;").unwrap();
        assert!(
            m.upsert_note_with_tags(
                &NoteRow {
                    stamp: stamp(2),
                    title: "Changed".into(),
                    aliases: vec!["Changed alias".into()],
                    ..row.clone()
                },
                &["new".into(), "fail".into()]
            )
            .is_err()
        );
        assert_eq!(m.note_by_path("a.md").unwrap(), Some(row));
        assert_eq!(m.tag_counts(None).unwrap(), [TagCount { tag: "old".into(), note_count: 1 }]);
    }

    fn stamp(n: u64) -> FileStamp {
        FileStamp { mtime: UNIX_EPOCH + Duration::from_secs(n), size: n, hash: n * 7 }
    }

    #[test]
    fn crud_and_rename() {
        let mut m = Meta::in_memory().unwrap();
        assert_eq!(m.note_count().unwrap(), 0);
        let a = NoteRow {
            note_id: "n1".into(),
            path: "a.md".into(),
            title: "a".into(),
            aliases: Vec::new(),
            stamp: stamp(1),
            block_count: 3,
        };
        m.upsert_note(&a).unwrap();
        assert_eq!(m.note_by_path("a.md").unwrap(), Some(a.clone()));
        // 同 note_id 更新指纹与块数
        let a2 = NoteRow { stamp: stamp(2), block_count: 5, ..a.clone() };
        m.upsert_note(&a2).unwrap();
        assert_eq!(m.note_by_path("a.md").unwrap().unwrap().block_count, 5);
        assert_eq!(m.note_count().unwrap(), 1);
        m.rename_note("n1", "sub/b.md", "b").unwrap();
        assert!(m.note_by_path("a.md").unwrap().is_none());
        let row = m.note_by_path("sub/b.md").unwrap().unwrap();
        assert_eq!(row.title, "b");
        assert_eq!(row.stamp, stamp(2));
        assert_eq!(m.all_notes().unwrap().len(), 1);
        m.remove_note("n1").unwrap();
        assert_eq!(m.note_count().unwrap(), 0);
    }

    #[test]
    fn reset_rolls_back_when_ddl_fails() {
        let mut m = Meta::in_memory().unwrap();
        m.upsert_note(&NoteRow {
            note_id: "n".into(),
            path: "a.md".into(),
            title: "a".into(),
            aliases: Vec::new(),
            stamp: stamp(1),
            block_count: 1,
        })
        .unwrap();
        m.conn
            .execute_batch(
                "DROP TABLE index_state; CREATE VIEW index_state AS SELECT 'meta' AS name, 1 AS version;",
            )
            .unwrap();
        assert!(m.reset().is_err());
        assert!(m.conn.is_autocommit());
        assert_eq!(m.note_count().unwrap(), 1);
    }

    #[test]
    fn links_store_replace_unresolve_and_clear() {
        let mut m = Meta::in_memory().unwrap();
        for (id, path) in [("n1", "rust/notes.md"), ("n2", "rust/cargo.md")] {
            m.upsert_note(&NoteRow {
                note_id: id.into(),
                path: path.into(),
                title: path.into(),
                aliases: Vec::new(),
                stamp: stamp(1),
                block_count: 1,
            })
            .unwrap();
        }

        let spans = aero_core::links::scan("[[cargo]] [[Cargo 指南]] [[missing]] [[cargo#安装]]");
        let resolved = [Some("n2"), Some("n2"), None, Some("n2")];
        m.replace_links("n1", spans.iter().zip(resolved)).unwrap();
        assert_eq!(
            m.links_rows().unwrap(),
            [
                ("cargo".to_owned(), Some("n2".to_owned()), None),
                ("Cargo 指南".to_owned(), Some("n2".to_owned()), None),
                ("missing".to_owned(), None, None),
                ("cargo".to_owned(), Some("n2".to_owned()), Some("安装".to_owned())),
            ]
        );

        // 目标被删 / 改名：指向它的行退回未解析，等补挂
        assert_eq!(m.unresolve_incoming("n2").unwrap(), 3);
        assert_eq!(m.pending_links().unwrap().len(), 4);

        // 重索引同一篇：旧行整体替换
        m.replace_links("n1", aero_core::links::scan("[[other]]").iter().map(|s| (s, None))).unwrap();
        let n: i64 = m
            .conn
            .query_row("SELECT count(*) FROM links WHERE source_note_id = 'n1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);

        m.remove_links("n1").unwrap();
        let n: i64 = m.conn.query_row("SELECT count(*) FROM links", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn pending_links_reconcile_after_target_is_indexed() {
        let mut m = Meta::in_memory().unwrap();
        // 只有源被索引（目标还没索引）：行先落库为未解析
        m.upsert_note(&NoteRow {
            note_id: "n1".into(),
            path: "notes.md".into(),
            title: "notes".into(),
            aliases: Vec::new(),
            stamp: stamp(1),
            block_count: 1,
        })
        .unwrap();
        m.replace_links("n1", aero_core::links::scan("[[b]]").iter().map(|s| (s, None))).unwrap();
        assert_eq!(m.pending_links().unwrap().len(), 1);

        // 目标入库后：pending 行能补挂上
        m.upsert_note(&NoteRow {
            note_id: "n2".into(),
            path: "b.md".into(),
            title: "b".into(),
            aliases: Vec::new(),
            stamp: stamp(2),
            block_count: 1,
        })
        .unwrap();
        let pending = m.pending_links().unwrap();
        assert_eq!(pending.len(), 1);
        let (note_id, path, target, fragment, byte_start) = &pending[0];
        assert_eq!(
            (note_id.as_str(), path.as_str(), target.as_str(), fragment),
            ("n1", "notes.md", "b", &None)
        );
        m.set_links_resolved([(note_id.as_str(), *byte_start, Some("n2"))]).unwrap();
        assert!(m.pending_links().unwrap().is_empty());
        assert_eq!(m.links_resolved_to(&["n2".to_owned()]).unwrap().len(), 1);
        assert_eq!(m.links_resolved_to(&["nope".to_owned()]).unwrap().len(), 0);

        // 目标行不存在（同步中途取消留下的预分配 id）：清成未解析
        m.set_links_resolved([(note_id.as_str(), *byte_start, Some("ghost"))]).unwrap();
        assert_eq!(m.clear_dangling_targets().unwrap(), 1);
        assert_eq!(m.pending_links().unwrap().len(), 1);
        m.set_links_resolved([(note_id.as_str(), *byte_start, Some("n2"))]).unwrap();
        assert_eq!(m.clear_dangling_targets().unwrap(), 0);
        let rows = m.links_rows().unwrap();
        assert_eq!(rows, [("b".to_owned(), Some("n2".to_owned()), None)]);
    }

    #[test]
    fn persists_and_resets_on_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.sqlite");
        {
            let m = Meta::open(&db).unwrap();
            m.upsert_note(&NoteRow {
                note_id: "n".into(),
                path: "p.md".into(),
                title: "p".into(),
                aliases: Vec::new(),
                stamp: stamp(3),
                block_count: 1,
            })
            .unwrap();
        }
        let m = Meta::open(&db).unwrap();
        assert_eq!(m.note_count().unwrap(), 1);
        // 版本号改了：下次打开整库重建
        m.conn.execute("UPDATE index_state SET version = 999", []).unwrap();
        drop(m);
        assert_eq!(Meta::open(&db).unwrap().note_count().unwrap(), 0);
    }
}
