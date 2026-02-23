use anyhow::{Context, Result};
use libsql::{Builder, Database};
use tracing::info;

// ─── Database bootstrap ───────────────────────────────────────────────────────

/// Open (or create) the local libSQL database and create all tables.
pub async fn init_db(path: &str) -> Result<Database> {
    let db = Builder::new_local(path)
        .build()
        .await
        .context("Failed to open local database")?;

    let conn = db.connect().context("Failed to connect to database")?;

    // Tasks table — generic work units
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            task_type   TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'pending',
            agent_id    TEXT NOT NULL DEFAULT '',
            payload     TEXT NOT NULL DEFAULT '{}',
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create tasks table")?;

    // Pipeline runs — tracks kernel evolution pipeline stages
    conn.execute(
        "CREATE TABLE IF NOT EXISTS pipeline_runs (
            id          TEXT PRIMARY KEY,
            stage       TEXT NOT NULL,
            artifact_id TEXT NOT NULL DEFAULT '',
            status      TEXT NOT NULL DEFAULT 'pending',
            result      TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create pipeline_runs table")?;

    // Agent heartbeats & status
    conn.execute(
        "CREATE TABLE IF NOT EXISTS agent_status (
            agent_id       TEXT PRIMARY KEY,
            role           TEXT NOT NULL DEFAULT '',
            status         TEXT NOT NULL DEFAULT 'offline',
            last_heartbeat TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create agent_status table")?;

    // Config lifecycle history
    conn.execute(
        "CREATE TABLE IF NOT EXISTS config_history (
            id          TEXT PRIMARY KEY,
            config_hash TEXT NOT NULL,
            action      TEXT NOT NULL,
            backup_path TEXT NOT NULL DEFAULT '',
            timestamp   TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create config_history table")?;

    info!(path = %path, "database initialized");
    Ok(db)
}

// ─── Agent CRUD ───────────────────────────────────────────────────────────────

/// Insert or update an agent's status. Pass `role = ""` to keep existing role.
pub async fn upsert_agent(db: &Database, agent_id: &str, role: &str, status: &str) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO agent_status (agent_id, role, status, last_heartbeat)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
             role           = CASE WHEN ?2 = '' THEN role ELSE ?2 END,
             status         = ?3,
             last_heartbeat = ?4",
        libsql::params![agent_id, role, status, now.as_str()],
    )
    .await
    .context("upsert agent status")?;

    Ok(())
}

// ─── Config history ───────────────────────────────────────────────────────────

/// Record a gateway config change event.
pub async fn log_config_event(
    db: &Database,
    config_hash: &str,
    action: &str,
    backup_path: Option<&str>,
) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let backup = backup_path.unwrap_or("");

    conn.execute(
        "INSERT INTO config_history (id, config_hash, action, backup_path, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        libsql::params![id.as_str(), config_hash, action, backup, now.as_str()],
    )
    .await
    .context("insert config_history")?;

    Ok(())
}

// ─── Tasks ────────────────────────────────────────────────────────────────────

/// Create a new task and return its ID.
pub async fn create_task(
    db: &Database,
    task_type: &str,
    agent_id: Option<&str>,
    payload: &str,
) -> Result<String> {
    let conn = db.connect().context("DB connect")?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO tasks (id, task_type, status, agent_id, payload, created_at, updated_at)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6)",
        libsql::params![
            id.as_str(),
            task_type,
            agent_id.unwrap_or(""),
            payload,
            now.as_str(),
            now.as_str()
        ],
    )
    .await
    .context("create task")?;

    Ok(id)
}
