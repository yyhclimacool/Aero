//! 元数据库（方案 10.2 的 M1b 子集）：每篇笔记的 `note_id`、路径、标题、文件指纹与已索引块数。
//! 它是索引状态的权威（10.5）：启动时用指纹决定哪些文件要重索引；保存时用块数算出要删的旧 pk。
//! 链接 / 标签 / 附件表随 M2 加入。

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aero_core::FileStamp;
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRow {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub stamp: FileStamp,
    pub block_count: u32,
}

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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
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
        self.conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS notes;
             DROP TABLE IF EXISTS index_state;
             CREATE TABLE notes (
                 note_id     TEXT PRIMARY KEY,
                 path        TEXT NOT NULL UNIQUE,
                 title       TEXT NOT NULL,
                 mtime_ns    INTEGER NOT NULL,
                 size        INTEGER NOT NULL,
                 hash        INTEGER NOT NULL,
                 block_count INTEGER NOT NULL
             );
             CREATE TABLE index_state (name TEXT PRIMARY KEY, version INTEGER NOT NULL);
             INSERT INTO index_state (name, version) VALUES ('meta', {SCHEMA_VERSION});"
        ))
    }

    pub fn note_by_path(&self, path: &str) -> rusqlite::Result<Option<NoteRow>> {
        self.conn
            .query_row(
                "SELECT note_id, path, title, mtime_ns, size, hash, block_count FROM notes WHERE path = ?1",
                params![path],
                row_to_note,
            )
            .optional()
    }

    pub fn all_notes(&self) -> rusqlite::Result<Vec<NoteRow>> {
        let mut st = self.conn.prepare(
            "SELECT note_id, path, title, mtime_ns, size, hash, block_count FROM notes ORDER BY path",
        )?;
        let rows = st.query_map([], row_to_note)?;
        rows.collect()
    }

    pub fn upsert_note(&mut self, row: &NoteRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO notes (note_id, path, title, mtime_ns, size, hash, block_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(note_id) DO UPDATE SET
                 path = excluded.path, title = excluded.title, mtime_ns = excluded.mtime_ns,
                 size = excluded.size, hash = excluded.hash, block_count = excluded.block_count",
            params![
                row.note_id,
                row.path,
                row.title,
                mtime_ns(row.stamp.mtime),
                row.stamp.size as i64,
                row.stamp.hash as i64,
                row.block_count as i64
            ],
        )?;
        Ok(())
    }

    pub fn remove_note(&mut self, note_id: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM notes WHERE note_id = ?1", params![note_id])?;
        Ok(())
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
}

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    let ns: i64 = r.get(3)?;
    Ok(NoteRow {
        note_id: r.get(0)?,
        path: r.get(1)?,
        title: r.get(2)?,
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
    fn persists_and_resets_on_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.sqlite");
        {
            let mut m = Meta::open(&db).unwrap();
            m.upsert_note(&NoteRow {
                note_id: "n".into(),
                path: "p.md".into(),
                title: "p".into(),
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
