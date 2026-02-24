use anyhow::{Context, Result};
use libsql::{Builder, Database};
use tracing::{info, warn};

// serde_json is a workspace dep; used for diagnostic JSON in mark_agent_failed_by_role
#[allow(unused_imports)]
use serde_json;

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

    // Cron job registry — system and user-defined recurring tasks
    conn.execute(
        "CREATE TABLE IF NOT EXISTS cron_jobs (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            schedule    TEXT NOT NULL,
            enabled     INTEGER NOT NULL DEFAULT 1,
            last_run_at TEXT,
            next_run_at TEXT,
            last_status TEXT,
            last_error  TEXT,
            created_at  TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create cron_jobs table")?;

    // ── Schema migrations ────────────────────────────────────────────────────
    // Add new columns to agent_status for enhanced metadata persistence.
    // SQLite doesn't support ADD COLUMN IF NOT EXISTS, so we catch the
    // "duplicate column" error when the migration has already been applied.
    let migrations = [
        "ALTER TABLE agent_status ADD COLUMN capabilities TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE agent_status ADD COLUMN skills TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE agent_status ADD COLUMN pid INTEGER NOT NULL DEFAULT 0",
        // Phase 5: pipeline coordinator columns
        "ALTER TABLE pipeline_runs ADD COLUMN run_id TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE pipeline_runs ADD COLUMN agent_id TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE pipeline_runs ADD COLUMN error TEXT",
    ];

    for sql in &migrations {
        match conn.execute(sql, ()).await {
            Ok(_) => info!(sql = %sql, "schema migration applied"),
            Err(e) => {
                let err_str = e.to_string();
                // "duplicate column" means migration already ran — safe to ignore
                if !err_str.contains("duplicate column") {
                    warn!(err = %err_str, sql = %sql, "schema migration warning");
                }
            }
        }
    }

    info!(path = %path, "database initialized");
    Ok(db)
}

// ─── Agent CRUD ───────────────────────────────────────────────────────────────

/// Insert or update an agent's status and metadata.
///
/// - Pass `role = ""` to keep the existing role.
/// - Pass `capabilities = None` / `skills = None` to keep existing values.
/// - Pass `pid = None` or `Some(0)` to keep the existing PID.
pub async fn upsert_agent(
    db: &Database,
    agent_id: &str,
    role: &str,
    status: &str,
    capabilities: Option<&str>,
    skills: Option<&str>,
    pid: Option<u32>,
) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();

    let caps = capabilities.unwrap_or("[]");
    let sk = skills.unwrap_or("[]");
    let p = pid.unwrap_or(0) as i64;

    conn.execute(
        "INSERT INTO agent_status (agent_id, role, status, last_heartbeat, capabilities, skills, pid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent_id) DO UPDATE SET
             role           = CASE WHEN ?2 = '' THEN role ELSE ?2 END,
             status         = ?3,
             last_heartbeat = ?4,
             capabilities   = CASE WHEN ?5 = '[]' AND capabilities != '[]' THEN capabilities ELSE ?5 END,
             skills         = CASE WHEN ?6 = '[]' AND skills != '[]' THEN skills ELSE ?6 END,
             pid            = CASE WHEN ?7 = 0 THEN pid ELSE ?7 END",
        libsql::params![agent_id, role, status, now.as_str(), caps, sk, p],
    )
    .await
    .context("upsert agent status")?;

    Ok(())
}

/// Mark an agent as `"crashed"` by matching its role name.
///
/// Used by the process monitor when a runner exits unexpectedly.
pub async fn mark_agent_crashed_by_role(db: &Database, role_hint: &str) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE agent_status SET status = 'crashed', last_heartbeat = ?1
         WHERE role LIKE '%' || ?2 || '%' AND status != 'crashed'",
        libsql::params![now.as_str(), role_hint],
    )
    .await
    .context("mark agent crashed")?;

    Ok(())
}

// ─── Agent query ──────────────────────────────────────────────────────────────

/// Row returned from agent_status queries.
#[derive(Debug, Clone)]
pub struct AgentRow {
    pub agent_id: String,
    pub role: String,
    pub status: String,
    pub last_heartbeat: String,
    pub capabilities: String,
    pub skills: String,
    pub pid: i64,
}

/// List all registered agents ordered by agent_id.
pub async fn list_agents(db: &Database) -> Result<Vec<AgentRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut rows = conn
        .query(
            "SELECT agent_id, role, status, last_heartbeat, capabilities, skills, pid
             FROM agent_status ORDER BY agent_id",
            (),
        )
        .await
        .context("list agents")?;

    let mut agents = Vec::new();
    while let Some(row) = rows.next().await.context("read agent row")? {
        agents.push(AgentRow {
            agent_id: row.get::<String>(0).context("read agent_id")?,
            role: row.get::<String>(1).context("read role")?,
            status: row.get::<String>(2).context("read status")?,
            last_heartbeat: row.get::<String>(3).context("read last_heartbeat")?,
            capabilities: row.get::<String>(4).unwrap_or_else(|_| "[]".to_string()),
            skills: row.get::<String>(5).unwrap_or_else(|_| "[]".to_string()),
            pid: row.get::<i64>(6).unwrap_or(0),
        });
    }

    Ok(agents)
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

/// Internal task row from database queries.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: String,
    pub task_type: String,
    pub status: String,
    pub agent_id: String,
    pub payload: String,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_task(row: &libsql::Row) -> Result<TaskRow> {
    Ok(TaskRow {
        id: row.get::<String>(0).context("read id")?,
        task_type: row.get::<String>(1).context("read task_type")?,
        status: row.get::<String>(2).context("read status")?,
        agent_id: row.get::<String>(3).context("read agent_id")?,
        payload: row.get::<String>(4).context("read payload")?,
        created_at: row.get::<String>(5).context("read created_at")?,
        updated_at: row.get::<String>(6).context("read updated_at")?,
    })
}

/// Create a new task and return the full row.
pub async fn create_task(
    db: &Database,
    task_type: &str,
    agent_id: Option<&str>,
    payload: &str,
) -> Result<TaskRow> {
    let conn = db.connect().context("DB connect")?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let agent = agent_id.unwrap_or("");

    conn.execute(
        "INSERT INTO tasks (id, task_type, status, agent_id, payload, created_at, updated_at)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6)",
        libsql::params![
            id.as_str(),
            task_type,
            agent,
            payload,
            now.as_str(),
            now.as_str()
        ],
    )
    .await
    .context("create task")?;

    Ok(TaskRow {
        id,
        task_type: task_type.to_string(),
        status: "pending".to_string(),
        agent_id: agent.to_string(),
        payload: payload.to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Fetch a single task by ID.
pub async fn get_task(db: &Database, task_id: &str) -> Result<Option<TaskRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut rows = conn
        .query(
            "SELECT id, task_type, status, agent_id, payload, created_at, updated_at
             FROM tasks WHERE id = ?1",
            libsql::params![task_id],
        )
        .await
        .context("query task by id")?;

    match rows.next().await.context("read row")? {
        Some(row) => Ok(Some(row_to_task(&row)?)),
        None => Ok(None),
    }
}

/// List tasks with optional status and agent filters, ordered by newest first.
pub async fn list_tasks(
    db: &Database,
    limit: u32,
    status_filter: Option<&str>,
    agent_filter: Option<&str>,
) -> Result<Vec<TaskRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut sql = String::from(
        "SELECT id, task_type, status, agent_id, payload, created_at, updated_at FROM tasks WHERE 1=1",
    );
    let mut param_values: Vec<libsql::Value> = Vec::new();

    if let Some(status) = status_filter {
        param_values.push(libsql::Value::Text(status.to_string()));
        sql.push_str(&format!(" AND status = ?{}", param_values.len()));
    }
    if let Some(agent) = agent_filter {
        param_values.push(libsql::Value::Text(agent.to_string()));
        sql.push_str(&format!(" AND agent_id = ?{}", param_values.len()));
    }

    param_values.push(libsql::Value::Integer(i64::from(limit)));
    sql.push_str(&format!(
        " ORDER BY created_at DESC LIMIT ?{}",
        param_values.len()
    ));

    let mut rows = conn.query(&sql, param_values).await.context("list tasks")?;
    let mut tasks = Vec::new();

    while let Some(row) = rows.next().await.context("read row")? {
        tasks.push(row_to_task(&row)?);
    }

    Ok(tasks)
}

/// Update a task's status, agent assignment, or payload. Returns the updated row.
pub async fn update_task(
    db: &Database,
    task_id: &str,
    status: Option<&str>,
    agent_id: Option<&str>,
    payload: Option<&str>,
) -> Result<Option<TaskRow>> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();

    let mut set_parts = vec!["updated_at = ?1".to_string()];
    let mut param_values: Vec<libsql::Value> = vec![libsql::Value::Text(now)];

    if let Some(s) = status {
        param_values.push(libsql::Value::Text(s.to_string()));
        set_parts.push(format!("status = ?{}", param_values.len()));
    }
    if let Some(a) = agent_id {
        param_values.push(libsql::Value::Text(a.to_string()));
        set_parts.push(format!("agent_id = ?{}", param_values.len()));
    }
    if let Some(p) = payload {
        param_values.push(libsql::Value::Text(p.to_string()));
        set_parts.push(format!("payload = ?{}", param_values.len()));
    }

    param_values.push(libsql::Value::Text(task_id.to_string()));
    let id_param = param_values.len();

    let sql = format!(
        "UPDATE tasks SET {} WHERE id = ?{}",
        set_parts.join(", "),
        id_param,
    );

    let rows_affected = conn
        .execute(&sql, param_values)
        .await
        .context("update task")?;

    if rows_affected == 0 {
        return Ok(None);
    }

    get_task(db, task_id).await
}

/// Delete a task by ID. Returns true if a row was deleted.
pub async fn delete_task(db: &Database, task_id: &str) -> Result<bool> {
    let conn = db.connect().context("DB connect")?;

    let rows_affected = conn
        .execute("DELETE FROM tasks WHERE id = ?1", libsql::params![task_id])
        .await
        .context("delete task")?;

    Ok(rows_affected > 0)
}

// ─── Pipeline runs ──────────────────────────────────────────────────────────

/// Row returned from pipeline_runs queries.
#[derive(Debug, Clone)]
pub struct PipelineRow {
    pub id: String,
    pub run_id: String,
    pub stage: String,
    pub artifact_id: String,
    pub status: String,
    pub agent_id: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_pipeline(row: &libsql::Row) -> Result<PipelineRow> {
    Ok(PipelineRow {
        id: row.get::<String>(0).context("read id")?,
        run_id: row.get::<String>(1).context("read run_id")?,
        stage: row.get::<String>(2).context("read stage")?,
        artifact_id: row.get::<String>(3).context("read artifact_id")?,
        status: row.get::<String>(4).context("read status")?,
        agent_id: row.get::<String>(5).unwrap_or_default(),
        result: row.get::<String>(6).ok(),
        error: row.get::<String>(7).ok(),
        created_at: row.get::<String>(8).context("read created_at")?,
        updated_at: row.get::<String>(9).context("read updated_at")?,
    })
}

/// Create a new pipeline stage record.
pub async fn create_pipeline_stage(
    db: &Database,
    run_id: &str,
    stage: &str,
    artifact_id: &str,
) -> Result<PipelineRow> {
    let conn = db.connect().context("DB connect")?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO pipeline_runs (id, run_id, stage, artifact_id, status, agent_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'running', '', ?5, ?6)",
        libsql::params![id.as_str(), run_id, stage, artifact_id, now.as_str(), now.as_str()],
    )
    .await
    .context("create pipeline stage")?;

    Ok(PipelineRow {
        id,
        run_id: run_id.to_string(),
        stage: stage.to_string(),
        artifact_id: artifact_id.to_string(),
        status: "running".to_string(),
        agent_id: String::new(),
        result: None,
        error: None,
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Update a pipeline stage with completion status, agent, result, and optional error.
pub async fn update_pipeline_stage(
    db: &Database,
    id: &str,
    status: &str,
    agent_id: &str,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();
    let res = result.unwrap_or("");
    let err = error.unwrap_or("");

    conn.execute(
        "UPDATE pipeline_runs SET status = ?1, agent_id = ?2, result = ?3, error = ?4, updated_at = ?5
         WHERE id = ?6",
        libsql::params![status, agent_id, res, err, now.as_str(), id],
    )
    .await
    .context("update pipeline stage")?;

    Ok(())
}

/// Get all stages for a specific pipeline run, ordered by creation time.
pub async fn get_pipeline_run_stages(db: &Database, run_id: &str) -> Result<Vec<PipelineRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut rows = conn
        .query(
            "SELECT id, run_id, stage, artifact_id, status, agent_id, result, error, created_at, updated_at
             FROM pipeline_runs WHERE run_id = ?1 ORDER BY created_at ASC",
            libsql::params![run_id],
        )
        .await
        .context("get pipeline stages")?;

    let mut stages = Vec::new();
    while let Some(row) = rows.next().await.context("read pipeline row")? {
        stages.push(row_to_pipeline(&row)?);
    }

    Ok(stages)
}

/// Get the most recent stage for each active (running) pipeline run.
pub async fn get_active_pipeline_runs(db: &Database) -> Result<Vec<PipelineRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut rows = conn
        .query(
            "SELECT id, run_id, stage, artifact_id, status, agent_id, result, error, created_at, updated_at
             FROM pipeline_runs WHERE status = 'running' ORDER BY created_at DESC",
            (),
        )
        .await
        .context("get active pipeline runs")?;

    let mut runs = Vec::new();
    while let Some(row) = rows.next().await.context("read pipeline row")? {
        runs.push(row_to_pipeline(&row)?);
    }

    Ok(runs)
}

/// Find pipeline stages that have been running longer than `timeout_seconds`.
pub async fn get_timed_out_stages(db: &Database, timeout_seconds: i64) -> Result<Vec<PipelineRow>> {
    let conn = db.connect().context("DB connect")?;
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(timeout_seconds)).to_rfc3339();

    let mut rows = conn
        .query(
            "SELECT id, run_id, stage, artifact_id, status, agent_id, result, error, created_at, updated_at
             FROM pipeline_runs WHERE status = 'running' AND created_at < ?1",
            libsql::params![cutoff.as_str()],
        )
        .await
        .context("get timed out stages")?;

    let mut timed_out = Vec::new();
    while let Some(row) = rows.next().await.context("read pipeline row")? {
        timed_out.push(row_to_pipeline(&row)?);
    }

    Ok(timed_out)
}

// ─── Cron jobs ────────────────────────────────────────────────────────────────

/// Row returned from cron_jobs queries.
#[derive(Debug, Clone)]
pub struct CronJobRow {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
}

/// Insert or update a cron job record.
///
/// If a job with the same `name` already exists its `schedule` and `enabled`
/// fields are preserved unless `overwrite = true`.
pub async fn upsert_cron_job(
    db: &Database,
    name: &str,
    schedule: &str,
    enabled: bool,
) -> Result<String> {
    let conn = db.connect().context("DB connect")?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let enabled_int = if enabled { 1i64 } else { 0i64 };

    conn.execute(
        "INSERT INTO cron_jobs (id, name, schedule, enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             schedule = ?3,
             enabled  = ?4",
        libsql::params![id.as_str(), name, schedule, enabled_int, now.as_str()],
    )
    .await
    .context("upsert cron_job")?;

    // Return the actual id (may differ from generated uuid if row already existed)
    let mut rows = conn
        .query(
            "SELECT id FROM cron_jobs WHERE name = ?1",
            libsql::params![name],
        )
        .await
        .context("fetch cron job id")?;

    let existing_id = if let Some(row) = rows.next().await.context("read cron id row")? {
        row.get::<String>(0).context("read id")?
    } else {
        id
    };

    Ok(existing_id)
}

/// Update a cron job after it has run.
pub async fn update_cron_run(
    db: &Database,
    name: &str,
    status: &str,
    error: Option<&str>,
    next_run_at: Option<&str>,
) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();
    let err = error.unwrap_or("");
    let next = next_run_at.unwrap_or("");

    conn.execute(
        "UPDATE cron_jobs SET
             last_run_at = ?1,
             last_status = ?2,
             last_error  = ?3,
             next_run_at = CASE WHEN ?4 = '' THEN next_run_at ELSE ?4 END
         WHERE name = ?5",
        libsql::params![now.as_str(), status, err, next, name],
    )
    .await
    .context("update cron run")?;

    Ok(())
}

/// Return all cron jobs ordered by name.
pub async fn list_cron_jobs(db: &Database) -> Result<Vec<CronJobRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut rows = conn
        .query(
            "SELECT id, name, schedule, enabled, last_run_at, next_run_at,
                    last_status, last_error, created_at
             FROM cron_jobs ORDER BY name",
            (),
        )
        .await
        .context("list cron jobs")?;

    let mut jobs = Vec::new();
    while let Some(row) = rows.next().await.context("read cron row")? {
        jobs.push(CronJobRow {
            id: row.get::<String>(0).context("read id")?,
            name: row.get::<String>(1).context("read name")?,
            schedule: row.get::<String>(2).context("read schedule")?,
            enabled: row.get::<i64>(3).unwrap_or(1) != 0,
            last_run_at: row.get::<String>(4).ok(),
            next_run_at: row.get::<String>(5).ok(),
            last_status: row.get::<String>(6).ok(),
            last_error: row.get::<String>(7).ok(),
            created_at: row.get::<String>(8).context("read created_at")?,
        });
    }

    Ok(jobs)
}

// ─── Agent failure tracking ───────────────────────────────────────────────────

/// Mark an agent as `"failed"` (all retries exhausted) by matching its role.
///
/// Unlike `mark_agent_crashed_by_role`, this status indicates that king gave up
/// attempting to respawn the agent and operator intervention is required.
pub async fn mark_agent_failed_by_role(db: &Database, role_hint: &str, error: &str) -> Result<()> {
    let conn = db.connect().context("DB connect")?;
    let now = chrono::Utc::now().to_rfc3339();

    // Store the error in the `capabilities` JSON field as a diagnostic payload
    // so it appears in the `GET /agents` response without a schema change.
    let error_json = serde_json::json!({ "spawn_error": error }).to_string();

    conn.execute(
        "UPDATE agent_status SET status = 'failed', last_heartbeat = ?1,
              capabilities = ?2
         WHERE role LIKE '%' || ?3 || '%' AND status != 'failed'",
        libsql::params![now.as_str(), error_json.as_str(), role_hint],
    )
    .await
    .context("mark agent failed")?;

    Ok(())
}

/// List all pipeline runs (most recent first), limited and optionally filtered by status.
pub async fn list_pipeline_runs(
    db: &Database,
    limit: u32,
    status_filter: Option<&str>,
) -> Result<Vec<PipelineRow>> {
    let conn = db.connect().context("DB connect")?;

    let mut sql = String::from(
        "SELECT id, run_id, stage, artifact_id, status, agent_id, result, error, created_at, updated_at
         FROM pipeline_runs WHERE 1=1",
    );
    let mut param_values: Vec<libsql::Value> = Vec::new();

    if let Some(status) = status_filter {
        param_values.push(libsql::Value::Text(status.to_string()));
        sql.push_str(&format!(" AND status = ?{}", param_values.len()));
    }

    param_values.push(libsql::Value::Integer(i64::from(limit)));
    sql.push_str(&format!(
        " ORDER BY created_at DESC LIMIT ?{}",
        param_values.len()
    ));

    let mut rows = conn.query(&sql, param_values).await.context("list pipeline runs")?;
    let mut runs = Vec::new();

    while let Some(row) = rows.next().await.context("read pipeline row")? {
        runs.push(row_to_pipeline(&row)?);
    }

    Ok(runs)
}
