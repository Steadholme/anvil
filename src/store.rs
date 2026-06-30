//! Pipeline + run storage.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring the
//! keystone/inkwell/loom seam: handlers (and the run scheduler) depend only on the trait, so a
//! FusionDB-backed store can drop in later. The PostgreSQL layer uses ONLY portable standard SQL
//! (TEXT/BIGINT, PK/NOT NULL/DEFAULT, parameterized queries, an `ON CONFLICT` guard, `CREATE
//! INDEX`, string `||` append) and runtime queries (no compile-time macros), so the build needs NO
//! database and the same statements later run unchanged on FusionDB over pgwire.
//!
//! The methods are `async`: the axum handlers + the run scheduler `.await` them directly, and
//! `PgStore` drives sqlx natively — there is NO `block_in_place` and NO sync-over-async bridge, so a
//! DB round-trip never blocks a worker thread. `PgStore` serializes its writes behind a
//! `tokio::sync::Mutex` so a run's `running -> append-log -> finish` sequence never interleaves
//! with another writer mid-transaction; reads stay lock-free on the shared pool.

use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

/// A CI pipeline definition (maps 1:1 to a `pipelines` row).
#[derive(Clone, Debug)]
pub struct Pipeline {
    pub id: String,
    pub name: String,
    pub repo_url: String,
    pub branch: String,
    /// Newline-separated shell commands run in order in the cloned workspace.
    pub steps: String,
    pub created_at: i64,
}

/// One execution of a pipeline (maps 1:1 to a `runs` row).
#[derive(Clone, Debug)]
pub struct Run {
    pub id: String,
    pub pipeline_id: String,
    /// `queued` | `running` | `success` | `failed`.
    pub status: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub exit_code: i64,
    pub log: String,
}

/// Storage failure surfaced to the handler / scheduler layer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A row with this id already exists (the PRIMARY KEY guard).
    #[error("id already exists: {0}")]
    Conflict(String),
    /// Backend I/O failure (mapped to a 500).
    #[error("store error: {0}")]
    Backend(String),
}

/// Pluggable pipeline/run store.
#[async_trait]
pub trait Store: Send + Sync {
    /// All pipelines, newest-first (`created_at` DESC).
    async fn list_pipelines(&self) -> Vec<Pipeline>;
    /// One pipeline by id.
    async fn get_pipeline(&self, id: &str) -> Option<Pipeline>;
    /// Insert a new pipeline. Errors with [`StoreError::Conflict`] if the id is taken.
    async fn create_pipeline(&self, p: &Pipeline) -> Result<(), StoreError>;

    /// Recent runs across all pipelines, newest-first, capped at `limit`.
    async fn list_recent_runs(&self, limit: usize) -> Vec<Run>;
    /// One run by id.
    async fn get_run(&self, id: &str) -> Option<Run>;
    /// Insert a new run (status `queued`). Errors with [`StoreError::Conflict`] on a duplicate id.
    async fn create_run(&self, r: &Run) -> Result<(), StoreError>;
    /// Mark a run as `running` with its start time.
    async fn mark_run_running(&self, id: &str, started_at: i64) -> Result<(), StoreError>;
    /// Append a chunk to a run's combined log.
    async fn append_run_log(&self, id: &str, chunk: &str) -> Result<(), StoreError>;
    /// Set a run's terminal status (`success`/`failed`), exit code, and finish time.
    async fn finish_run(
        &self,
        id: &str,
        status: &str,
        exit_code: i64,
        finished_at: i64,
    ) -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    pipelines: Mutex<Vec<Pipeline>>,
    runs: Mutex<Vec<Run>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    // The std `Mutex` is fine throughout: each critical section is fully synchronous (no `.await`
    // inside), so a guard is never held across a yield point.
    async fn list_pipelines(&self) -> Vec<Pipeline> {
        let mut v: Vec<Pipeline> = self.pipelines.lock().expect("pipelines lock").clone();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        v
    }

    async fn get_pipeline(&self, id: &str) -> Option<Pipeline> {
        self.pipelines
            .lock()
            .expect("pipelines lock")
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    async fn create_pipeline(&self, p: &Pipeline) -> Result<(), StoreError> {
        let mut pipelines = self.pipelines.lock().expect("pipelines lock");
        if pipelines.iter().any(|x| x.id == p.id) {
            return Err(StoreError::Conflict(p.id.clone()));
        }
        pipelines.push(p.clone());
        Ok(())
    }

    async fn list_recent_runs(&self, limit: usize) -> Vec<Run> {
        let mut v: Vec<Run> = self.runs.lock().expect("runs lock").clone();
        // Newest-first by start time, then id; queued runs (started_at == 0) sort to the top.
        v.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(limit);
        v
    }

    async fn get_run(&self, id: &str) -> Option<Run> {
        self.runs
            .lock()
            .expect("runs lock")
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    async fn create_run(&self, r: &Run) -> Result<(), StoreError> {
        let mut runs = self.runs.lock().expect("runs lock");
        if runs.iter().any(|x| x.id == r.id) {
            return Err(StoreError::Conflict(r.id.clone()));
        }
        runs.push(r.clone());
        Ok(())
    }

    async fn mark_run_running(&self, id: &str, started_at: i64) -> Result<(), StoreError> {
        let mut runs = self.runs.lock().expect("runs lock");
        match runs.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.status = "running".to_string();
                r.started_at = started_at;
                Ok(())
            }
            None => Err(StoreError::Backend(format!("no run with id {id}"))),
        }
    }

    async fn append_run_log(&self, id: &str, chunk: &str) -> Result<(), StoreError> {
        let mut runs = self.runs.lock().expect("runs lock");
        match runs.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.log.push_str(chunk);
                Ok(())
            }
            None => Err(StoreError::Backend(format!("no run with id {id}"))),
        }
    }

    async fn finish_run(
        &self,
        id: &str,
        status: &str,
        exit_code: i64,
        finished_at: i64,
    ) -> Result<(), StoreError> {
        let mut runs = self.runs.lock().expect("runs lock");
        match runs.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.status = status.to_string();
                r.exit_code = exit_code;
                r.finished_at = finished_at;
                Ok(())
            }
            None => Err(StoreError::Backend(format!("no run with id {id}"))),
        }
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `ANVIL_STORE=postgres`. Each method drives sqlx natively and the callers
// `.await` it on the serving runtime — NO `block_in_place`, NO sync-over-async. Writes are
// serialized behind a `tokio::sync::Mutex`; reads use the shared pool directly.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use tokio::sync::Mutex as AsyncMutex;

/// PostgreSQL-backed [`Store`]. Holds a `PgPool` (reads) + a write serializer.
pub struct PgStore {
    pool: PgPool,
    write_lock: AsyncMutex<()>,
}

impl PgStore {
    /// Open a pooled connection. Async; call from within a Tokio runtime.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            write_lock: AsyncMutex::new(()),
        }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pipelines (\
                 id TEXT PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 repo_url TEXT NOT NULL, \
                 branch TEXT NOT NULL DEFAULT 'main', \
                 steps TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS runs (\
                 id TEXT PRIMARY KEY, \
                 pipeline_id TEXT NOT NULL, \
                 status TEXT NOT NULL DEFAULT 'queued', \
                 started_at BIGINT NOT NULL DEFAULT 0, \
                 finished_at BIGINT NOT NULL DEFAULT 0, \
                 exit_code BIGINT NOT NULL DEFAULT 0, \
                 log TEXT NOT NULL DEFAULT ''\
             )",
        )
        .execute(&self.pool)
        .await?;
        // Backs the per-pipeline run history scan.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_runs_pipeline_started ON runs (pipeline_id, started_at)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn pipeline_from_row(row: &sqlx::postgres::PgRow) -> Result<Pipeline, sqlx::Error> {
        Ok(Pipeline {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            repo_url: row.try_get("repo_url")?,
            branch: row.try_get("branch")?,
            steps: row.try_get("steps")?,
            created_at: row.try_get::<Option<i64>, _>("created_at")?.unwrap_or(0),
        })
    }

    fn run_from_row(row: &sqlx::postgres::PgRow) -> Result<Run, sqlx::Error> {
        Ok(Run {
            id: row.try_get("id")?,
            pipeline_id: row.try_get("pipeline_id")?,
            status: row.try_get("status")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            exit_code: row.try_get("exit_code")?,
            log: row.try_get("log")?,
        })
    }

    async fn list_pipelines_async(&self) -> Result<Vec<Pipeline>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, repo_url, branch, steps, created_at \
             FROM pipelines ORDER BY created_at DESC, id DESC LIMIT $1",
        )
        .bind(crate::config::PIPELINE_LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::pipeline_from_row).collect()
    }

    async fn get_pipeline_async(&self, id: &str) -> Result<Option<Pipeline>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, repo_url, branch, steps, created_at FROM pipelines WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(Self::pipeline_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_recent_runs_async(&self, limit: usize) -> Result<Vec<Run>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, pipeline_id, status, started_at, finished_at, exit_code, log \
             FROM runs ORDER BY started_at DESC, id DESC LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::run_from_row).collect()
    }

    async fn get_run_async(&self, id: &str) -> Result<Option<Run>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, pipeline_id, status, started_at, finished_at, exit_code, log \
             FROM runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(Self::run_from_row(&r)?)),
            None => Ok(None),
        }
    }
}

/// True when a sqlx error is a UNIQUE/PK violation (Postgres SQLSTATE 23505).
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

#[async_trait]
impl Store for PgStore {
    async fn list_pipelines(&self) -> Vec<Pipeline> {
        self.list_pipelines_async().await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_pipelines failed");
            Vec::new()
        })
    }

    async fn get_pipeline(&self, id: &str) -> Option<Pipeline> {
        self.get_pipeline_async(id).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg get_pipeline failed");
            None
        })
    }

    async fn create_pipeline(&self, p: &Pipeline) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO pipelines (id, name, repo_url, branch, steps, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&p.id)
        .bind(&p.name)
        .bind(&p.repo_url)
        .bind(&p.branch)
        .bind(&p.steps)
        .bind(p.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(p.id.clone())
            } else {
                StoreError::Backend(e.to_string())
            }
        })?;
        Ok(())
    }

    async fn list_recent_runs(&self, limit: usize) -> Vec<Run> {
        self.list_recent_runs_async(limit).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_recent_runs failed");
            Vec::new()
        })
    }

    async fn get_run(&self, id: &str) -> Option<Run> {
        self.get_run_async(id).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg get_run failed");
            None
        })
    }

    async fn create_run(&self, r: &Run) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "INSERT INTO runs (id, pipeline_id, status, started_at, finished_at, exit_code, log) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&r.id)
        .bind(&r.pipeline_id)
        .bind(&r.status)
        .bind(r.started_at)
        .bind(r.finished_at)
        .bind(r.exit_code)
        .bind(&r.log)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn mark_run_running(&self, id: &str, started_at: i64) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        sqlx::query("UPDATE runs SET status = 'running', started_at = $1 WHERE id = $2")
            .bind(started_at)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn append_run_log(&self, id: &str, chunk: &str) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        sqlx::query("UPDATE runs SET log = log || $1 WHERE id = $2")
            .bind(chunk)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn finish_run(
        &self,
        id: &str,
        status: &str,
        exit_code: i64,
        finished_at: i64,
    ) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        sqlx::query(
            "UPDATE runs SET status = $1, exit_code = $2, finished_at = $3 WHERE id = $4",
        )
        .bind(status)
        .bind(exit_code)
        .bind(finished_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(())
    }
}
