use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use brother_scan::Source;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    root: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Job {
    pub id: String,
    pub source: String,
    pub trigger: String,
    pub state: String,
    pub created: i64,
    pub updated: i64,
    pub pages: i64,
    pub sha256: Option<String>,
    pub error: Option<String>,
    pub task_id: Option<String>,
    pub document_id: Option<i64>,
    pub attempts: i64,
    pub next_attempt: i64,
}

pub enum Admission {
    Started(Job),
    Existing(Job),
    Rejected(String),
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl Store {
    pub async fn open(root: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite:")?
            .filename(root.join("jobs.sqlite"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS jobs (
          id TEXT PRIMARY KEY, source TEXT NOT NULL, trigger TEXT NOT NULL, state TEXT NOT NULL,
          created INTEGER NOT NULL, updated INTEGER NOT NULL, pages INTEGER NOT NULL DEFAULT 0,
          sha256 TEXT, error TEXT, task_id TEXT, document_id INTEGER,
          attempts INTEGER NOT NULL DEFAULT 0, next_attempt INTEGER NOT NULL DEFAULT 0);
          CREATE UNIQUE INDEX IF NOT EXISTS one_capture ON jobs(state) WHERE state='acquiring';
          CREATE TABLE IF NOT EXISTS requests (key TEXT PRIMARY KEY, source TEXT NOT NULL, created INTEGER NOT NULL, job_id TEXT, rejection TEXT);
          CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
          INSERT OR IGNORE INTO settings VALUES ('uncertain', '0');")
            .execute(&pool).await?;
        let store = Self {
            pool,
            root: root.into(),
        };
        store.recover().await?;
        Ok(store)
    }

    pub fn directory(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn recover(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let count = sqlx::query("UPDATE jobs SET state='interrupted', error='Service stopped during acquisition; inspect printer before acknowledging', updated=? WHERE state='acquiring'")
            .bind(now()).execute(&mut *tx).await?.rows_affected();
        if count > 0 {
            sqlx::query("UPDATE settings SET value='1' WHERE key='uncertain'")
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE jobs SET state='unknown', error='Upload interrupted; verify Paperless before retrying', updated=? WHERE state='uploading'")
            .bind(now()).execute(&mut *tx).await?;
        tx.commit().await?;
        let retained: Vec<Job> = sqlx::query_as("SELECT * FROM jobs WHERE state IN ('captured','ready','submitted','unknown','delivery_failed')").fetch_all(&self.pool).await?;
        for job in retained {
            let file = match job.state.as_str() {
                "captured" => "capture.json",
                _ => "document.pdf",
            };
            if !self.directory(&job.id).join(file).is_file() {
                let state = if ["submitted", "unknown"].contains(&job.state.as_str()) {
                    job.state.as_str()
                } else {
                    "failed"
                };
                self.transition(
                    &job.id,
                    state,
                    Some("Referenced artifact is missing; inspect persistent storage"),
                    None,
                    None,
                )
                .await?;
            }
        }
        // Unreferenced directories are deliberately retained. They are never auto-imported.
        Ok(())
    }

    /// Atomically consume an identity even when busy, so a retransmission cannot scan later.
    pub async fn admit(
        &self,
        key: &str,
        source: Source,
        trigger: &str,
        storage_error: Option<&str>,
    ) -> Result<Admission> {
        let source = match source {
            Source::Feeder => "feeder",
            Source::Platen => "platen",
        };
        let mut tx = self.pool.begin().await?;
        // Notification IDs have no established global uniqueness. Limit their lifetime.
        sqlx::query("DELETE FROM requests WHERE (key LIKE 'button:%' AND created < ?) OR (key LIKE 'ui:%' AND created < ?)")
            .bind(now() - 86400).bind(now() - 7 * 86400).execute(&mut *tx).await?;
        if let Some(row) = sqlx::query("SELECT source,job_id,rejection FROM requests WHERE key=?")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?
        {
            if key.starts_with("ui:") && row.get::<String, _>("source") != source {
                return Ok(Admission::Rejected(
                    "Idempotency key was used for another source".into(),
                ));
            }
            let id: Option<String> = row.get("job_id");
            if let Some(id) = id {
                let job = sqlx::query_as("SELECT * FROM jobs WHERE id=?")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                return Ok(Admission::Existing(job));
            }
            return Ok(Admission::Rejected(row.get("rejection")));
        }
        let busy: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state='acquiring'")
            .fetch_one(&mut *tx)
            .await?;
        let uncertain: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key='uncertain'")
                .fetch_one(&mut *tx)
                .await?;
        let rejection = if busy > 0 {
            Some("Scanner is busy")
        } else if uncertain == "1" {
            Some("Scanner state is uncertain; inspect printer and acknowledge before scanning")
        } else {
            storage_error
        };
        if let Some(error) = rejection {
            sqlx::query("INSERT INTO requests VALUES (?, ?, ?, NULL, ?)")
                .bind(key)
                .bind(source)
                .bind(now())
                .bind(error)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(Admission::Rejected(error.into()));
        }
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO jobs (id,source,trigger,state,created,updated) VALUES (?,?,?,'acquiring',?,?)")
            .bind(&id).bind(source).bind(trigger).bind(now()).bind(now()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO requests VALUES (?,?,?,?,NULL)")
            .bind(key)
            .bind(source)
            .bind(now())
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Admission::Started(
            self.job(&id).await?.context("admitted job missing")?,
        ))
    }

    pub async fn job(&self, id: &str) -> Result<Option<Job>> {
        Ok(sqlx::query_as("SELECT * FROM jobs WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
    pub async fn recent(&self) -> Result<Vec<Job>> {
        Ok(
            sqlx::query_as("SELECT * FROM jobs ORDER BY created DESC, rowid DESC LIMIT 100")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn pending(&self) -> Result<Vec<Job>> {
        Ok(sqlx::query_as("SELECT * FROM jobs WHERE state IN ('captured','ready','submitted') AND next_attempt<=? ORDER BY created").bind(now()).fetch_all(&self.pool).await?)
    }
    pub async fn uncertain(&self) -> Result<bool> {
        Ok(
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key='uncertain'")
                .fetch_one(&self.pool)
                .await?
                == "1",
        )
    }
    pub async fn acknowledge(&self) -> Result<()> {
        sqlx::query("UPDATE settings SET value='0' WHERE key='uncertain'")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn interrupted(&self, id: &str, error: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE settings SET value='1' WHERE key='uncertain'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE jobs SET state='interrupted',error=?,updated=? WHERE id=?")
            .bind(error)
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn captured(&self, id: &str, pages: usize) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state='captured',pages=?,updated=? WHERE id=? AND state='acquiring'",
        )
        .bind(pages as i64)
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn ready(&self, id: &str, hash: &str) -> Result<()> {
        sqlx::query("UPDATE jobs SET state='ready',sha256=?,error=NULL,updated=? WHERE id=? AND state='captured'")
            .bind(hash).bind(now()).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn transition(
        &self,
        id: &str,
        state: &str,
        error: Option<&str>,
        task: Option<&str>,
        document: Option<i64>,
    ) -> Result<()> {
        sqlx::query("UPDATE jobs SET state=?,error=?,task_id=COALESCE(?,task_id),document_id=COALESCE(?,document_id),updated=?,attempts=0,next_attempt=0 WHERE id=?")
            .bind(state).bind(error).bind(task).bind(document).bind(now()).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn defer(&self, job: &Job, state: &str, error: &str) -> Result<()> {
        let delay = (5i64 << job.attempts.clamp(0, 6)).min(300);
        sqlx::query("UPDATE jobs SET state=?,error=?,updated=?,attempts=attempts+1,next_attempt=? WHERE id=?")
            .bind(state).bind(error).bind(now()).bind(now() + delay).bind(&job.id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn retry(&self, id: &str, confirm_unknown: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if state != "delivery_failed" && !(state == "unknown" && confirm_unknown) {
            bail!(
                "delivery cannot be retried in this state; unknown outcomes require confirmation"
            );
        }
        if !self.directory(id).join("document.pdf").is_file() {
            bail!("PDF is missing");
        }
        sqlx::query("UPDATE jobs SET state='ready', task_id=NULL, error=NULL, updated=?,attempts=0,next_attempt=0 WHERE id=?").bind(now()).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn expired(&self, before: i64) -> Result<Vec<Job>> {
        Ok(
            sqlx::query_as("SELECT * FROM jobs WHERE state='imported' AND updated < ?")
                .bind(before)
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn queue_stats(&self) -> Result<(i64, i64)> {
        let row = sqlx::query("SELECT count(*) AS n, min(created) AS oldest FROM jobs WHERE state IN ('captured','ready','uploading','submitted','unknown','delivery_failed')").fetch_one(&self.pool).await?;
        Ok((
            row.get("n"),
            row.get::<Option<i64>, _>("oldest")
                .map(|v| now() - v)
                .unwrap_or(0),
        ))
    }
}
