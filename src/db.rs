use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemMode {
    Audio,
    Video,
}

impl ItemMode {
    fn as_str(self) -> &'static str {
        match self {
            ItemMode::Audio => "audio",
            ItemMode::Video => "video",
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db dir {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS items (
                video_id    TEXT NOT NULL,
                source_name TEXT NOT NULL,
                mode        TEXT NOT NULL,
                title       TEXT,
                file_path   TEXT,
                downloaded_at INTEGER NOT NULL,
                PRIMARY KEY (video_id, mode)
            );
            CREATE INDEX IF NOT EXISTS idx_items_source ON items(source_name);

            CREATE TABLE IF NOT EXISTS failures (
                video_id    TEXT NOT NULL,
                source_name TEXT NOT NULL,
                mode        TEXT NOT NULL,
                last_error  TEXT,
                attempts    INTEGER NOT NULL DEFAULT 0,
                last_attempt_at INTEGER NOT NULL,
                PRIMARY KEY (video_id, mode)
            );

            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                ok_count    INTEGER NOT NULL DEFAULT 0,
                fail_count  INTEGER NOT NULL DEFAULT 0,
                notes       TEXT
            );
            "#,
        )?;
        // Migration: cookies_suspicious column was added after the initial
        // schema. ALTER is idempotent via the PRAGMA check below so the
        // upgrade path is safe on existing databases.
        let has_col: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('runs') WHERE name = 'cookies_suspicious'",
            [],
            |r| r.get(0),
        )?;
        if has_col == 0 {
            self.conn.execute(
                "ALTER TABLE runs ADD COLUMN cookies_suspicious INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        Ok(())
    }

    pub fn is_done(&self, video_id: &str, mode: ItemMode) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE video_id = ?1 AND mode = ?2",
            params![video_id, mode.as_str()],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn mark_done(
        &self,
        video_id: &str,
        source: &str,
        mode: ItemMode,
        title: Option<&str>,
        file_path: &Path,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO items (video_id, source_name, mode, title, file_path, downloaded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                video_id,
                source,
                mode.as_str(),
                title,
                file_path.to_string_lossy(),
                now_epoch(),
            ],
        )?;
        self.conn.execute(
            "DELETE FROM failures WHERE video_id = ?1 AND mode = ?2",
            params![video_id, mode.as_str()],
        )?;
        Ok(())
    }

    pub fn record_failure(
        &self,
        video_id: &str,
        source: &str,
        mode: ItemMode,
        err: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO failures (video_id, source_name, mode, last_error, attempts, last_attempt_at)
               VALUES (?1, ?2, ?3, ?4, 1, ?5)
               ON CONFLICT(video_id, mode) DO UPDATE SET
                 last_error = excluded.last_error,
                 attempts = failures.attempts + 1,
                 last_attempt_at = excluded.last_attempt_at,
                 source_name = excluded.source_name"#,
            params![video_id, source, mode.as_str(), err, now_epoch()],
        )?;
        Ok(())
    }

    pub fn start_run(&self) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (started_at) VALUES (?1)",
            params![now_epoch()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_run(
        &self,
        run_id: i64,
        ok: u64,
        fail: u64,
        notes: Option<&str>,
        cookies_suspicious: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at = ?1, ok_count = ?2, fail_count = ?3, notes = ?4, cookies_suspicious = ?5 WHERE id = ?6",
            params![
                now_epoch(),
                ok as i64,
                fail as i64,
                notes,
                if cookies_suspicious { 1 } else { 0 },
                run_id,
            ],
        )?;
        Ok(())
    }

    pub fn last_run_summary(&self) -> Result<Option<RunSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, finished_at, ok_count, fail_count, notes, cookies_suspicious FROM runs ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let cookies_suspicious: i64 = row.get(6)?;
            Ok(Some(RunSummary {
                id: row.get(0)?,
                started_at: row.get(1)?,
                finished_at: row.get(2)?,
                ok_count: row.get(3)?,
                fail_count: row.get(4)?,
                notes: row.get(5)?,
                cookies_suspicious: cookies_suspicious != 0,
            }))
        } else {
            Ok(None)
        }
    }

    /// Returns the run id of the most recent run that flagged cookies_suspicious,
    /// if any.
    pub fn last_cookies_warning_run(&self) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM runs WHERE cookies_suspicious = 1 ORDER BY id DESC LIMIT 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn failure_count(&self) -> Result<i64> {
        let c: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM failures", [], |r| r.get(0))?;
        Ok(c)
    }
}

#[derive(Debug)]
pub struct RunSummary {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub ok_count: i64,
    pub fail_count: i64,
    pub notes: Option<String>,
    pub cookies_suspicious: bool,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
