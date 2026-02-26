use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database};
use tracing::{info, warn};

// ─── Connection helper ────────────────────────────────────────────────────────

/// Create a libSQL connection and immediately set `busy_timeout=5000`.
///
/// SQLite only allows one writer at a time. Without a busy timeout, concurrent
/// writes (e.g. 6 agents all registering at startup) immediately fail with
/// SQLITE_BUSY. A 5-second timeout makes writers retry before giving up.
/// The WAL journal mode (set once in `init_db`) further improves throughput.
pub(crate) async fn db_connect(db: &Database) -> Result<Connection> {
    let conn = db.connect().context("DB connect")?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")
        .await
        .context("set busy_timeout")?;
    Ok(conn)
}

// ─── Database bootstrap ───────────────────────────────────────────────────────

/// Open (or create) the local libSQL database and create all tables.
pub async fn init_db(path: &str) -> Result<Database> {
    let db = Builder::new_local(path)
        .build()
        .await
        .context("Failed to open local database")?;

    let conn = db.connect().context("Failed to connect to database")?;

    // Enable WAL journal mode (persistent, survives restarts) for better
    // concurrency: multiple readers can coexist with one writer.
    // Set a 5-second busy timeout so concurrent writers retry instead of
    // immediately returning SQLITE_BUSY (which caused 4/6 agent:register
    // upserts to fail when all agents connect simultaneously).
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .await
        .context("Failed to set WAL + busy_timeout pragmas")?;

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

    // Task logs — detailed running log entries for task progress
    conn.execute(
        "CREATE TABLE IF NOT EXISTS task_logs (
            id         TEXT PRIMARY KEY,
            task_id    TEXT NOT NULL,
            level      TEXT NOT NULL DEFAULT 'info',
            message    TEXT NOT NULL,
            detail     TEXT NOT NULL DEFAULT '',
            agent_id   TEXT NOT NULL DEFAULT '',
            stage      TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create task_logs table")?;

    // Memories — core memory records (L0/L1/L2 tiers stored in memory_tiers)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memories (
            id              TEXT PRIMARY KEY,
            scope           TEXT NOT NULL DEFAULT 'system',
            category        TEXT NOT NULL DEFAULT 'general',
            key             TEXT NOT NULL DEFAULT '',
            metadata        TEXT NOT NULL DEFAULT '{}',
            tags            TEXT NOT NULL DEFAULT '',
            agent_id        TEXT NOT NULL DEFAULT '',
            run_id          TEXT NOT NULL DEFAULT '',
            skill_id        TEXT NOT NULL DEFAULT '',
            relevance_score REAL NOT NULL DEFAULT 0.0,
            access_count    INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create memories table")?;

    // Memory tiers — L0 (abstract), L1 (overview), L2 (full content)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_tiers (
            id         TEXT PRIMARY KEY,
            memory_id  TEXT NOT NULL,
            tier       TEXT NOT NULL,
            content    TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create memory_tiers table")?;

    // Task-memory junction — binds memories to tasks for fast filtering
    conn.execute(
        "CREATE TABLE IF NOT EXISTS task_memories (
            id         TEXT PRIMARY KEY,
            task_id    TEXT NOT NULL,
            memory_id  TEXT NOT NULL,
            created_at TEXT NOT NULL
        )",
        (),
    )
    .await
    .context("create task_memories table")?;

    // Memory indexes
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
         CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
         CREATE INDEX IF NOT EXISTS idx_memories_agent_id ON memories(agent_id);
         CREATE INDEX IF NOT EXISTS idx_memories_run_id ON memories(run_id);
         CREATE INDEX IF NOT EXISTS idx_memories_key ON memories(key);
         CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
         CREATE INDEX IF NOT EXISTS idx_memory_tiers_memory_id ON memory_tiers(memory_id);
         CREATE INDEX IF NOT EXISTS idx_memory_tiers_tier ON memory_tiers(tier);
         CREATE INDEX IF NOT EXISTS idx_task_memories_task_id ON task_memories(task_id);
         CREATE INDEX IF NOT EXISTS idx_task_memories_memory_id ON task_memories(memory_id);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_task_memories_unique ON task_memories(task_id, memory_id);",
    )
    .await
    .context("create memory indexes")?;

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
        // Phase 6: task management — link tasks to pipeline runs
        "ALTER TABLE tasks ADD COLUMN run_id TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE tasks ADD COLUMN current_stage TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE tasks ADD COLUMN summary TEXT NOT NULL DEFAULT ''",
        // Phase 7: subtask hierarchy support
        "ALTER TABLE tasks ADD COLUMN parent_id TEXT NOT NULL DEFAULT ''",
        // Phase 8: task room — index task_logs for fast lookup
        "CREATE INDEX IF NOT EXISTS idx_task_logs_task_id ON task_logs(task_id)",
        // Phase 9: per-agent model preference
        "ALTER TABLE agent_status ADD COLUMN preferred_model TEXT NOT NULL DEFAULT ''",
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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;
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
    pub preferred_model: String,
}

/// List all registered agents ordered by agent_id.
pub async fn list_agents(db: &Database) -> Result<Vec<AgentRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT agent_id, role, status, last_heartbeat, capabilities, skills, pid, preferred_model
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
            preferred_model: row.get::<String>(7).unwrap_or_default(),
        });
    }

    Ok(agents)
}

/// Set the preferred model for an agent.
pub async fn set_agent_model(db: &Database, agent_id: &str, model: &str) -> Result<()> {
    let conn = db_connect(db).await?;
    conn.execute(
        "UPDATE agent_status SET preferred_model = ?1 WHERE agent_id = ?2",
        libsql::params![model, agent_id],
    )
    .await
    .context("set agent model")?;
    Ok(())
}

/// Get the preferred model for an agent.
pub async fn get_agent_model(db: &Database, agent_id: &str) -> Result<String> {
    let conn = db_connect(db).await?;
    let mut rows = conn
        .query(
            "SELECT preferred_model FROM agent_status WHERE agent_id = ?1",
            libsql::params![agent_id],
        )
        .await
        .context("get agent model")?;
    if let Some(row) = rows.next().await.context("read row")? {
        Ok(row.get::<String>(0).unwrap_or_default())
    } else {
        Ok(String::new())
    }
}

// ─── Config history ───────────────────────────────────────────────────────────

/// Record a gateway config change event.
pub async fn log_config_event(
    db: &Database,
    config_hash: &str,
    action: &str,
    backup_path: Option<&str>,
) -> Result<()> {
    let conn = db_connect(db).await?;
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

/// Row returned from config_history queries.
#[derive(Debug, Clone)]
pub struct ConfigHistoryRow {
    pub id: String,
    pub config_hash: String,
    pub action: String,
    pub backup_path: String,
    pub timestamp: String,
}

/// List config change history, newest first.
pub async fn list_config_history(db: &Database, limit: u32) -> Result<Vec<ConfigHistoryRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, config_hash, action, backup_path, timestamp
             FROM config_history ORDER BY timestamp DESC LIMIT ?1",
            libsql::params![i64::from(limit)],
        )
        .await
        .context("list config history")?;

    let mut history = Vec::new();
    while let Some(row) = rows.next().await.context("read config_history row")? {
        history.push(ConfigHistoryRow {
            id: row.get::<String>(0).context("read id")?,
            config_hash: row.get::<String>(1).context("read config_hash")?,
            action: row.get::<String>(2).context("read action")?,
            backup_path: row.get::<String>(3).unwrap_or_default(),
            timestamp: row.get::<String>(4).context("read timestamp")?,
        });
    }

    Ok(history)
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
    pub run_id: String,
    pub current_stage: String,
    pub summary: String,
    pub parent_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// SELECT column list for task queries — must match `row_to_task` field order.
const TASK_COLS: &str = "id, task_type, status, agent_id, payload, run_id, current_stage, summary, parent_id, created_at, updated_at";

fn row_to_task(row: &libsql::Row) -> Result<TaskRow> {
    Ok(TaskRow {
        id: row.get::<String>(0).context("read id")?,
        task_type: row.get::<String>(1).context("read task_type")?,
        status: row.get::<String>(2).context("read status")?,
        agent_id: row.get::<String>(3).context("read agent_id")?,
        payload: row.get::<String>(4).context("read payload")?,
        run_id: row.get::<String>(5).unwrap_or_default(),
        current_stage: row.get::<String>(6).unwrap_or_default(),
        summary: row.get::<String>(7).unwrap_or_default(),
        parent_id: row.get::<String>(8).unwrap_or_default(),
        created_at: row.get::<String>(9).context("read created_at")?,
        updated_at: row.get::<String>(10).context("read updated_at")?,
    })
}

/// Create a new task and return the full row.
///
/// `run_id` links the task to a pipeline run (1:1).
/// `current_stage` is the initial pipeline stage (e.g. "learning").
/// `summary` is a human-readable description of the current activity.
/// `parent_id` links this task as a subtask of another task (empty = top-level).
#[allow(clippy::too_many_arguments)]
pub async fn create_task(
    db: &Database,
    task_type: &str,
    status: &str,
    agent_id: Option<&str>,
    payload: &str,
    run_id: &str,
    current_stage: &str,
    summary: &str,
    parent_id: &str,
) -> Result<TaskRow> {
    let conn = db_connect(db).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let agent = agent_id.unwrap_or("");

    conn.execute(
        "INSERT INTO tasks (id, task_type, status, agent_id, payload, run_id, current_stage, summary, parent_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        libsql::params![
            id.as_str(),
            task_type,
            status,
            agent,
            payload,
            run_id,
            current_stage,
            summary,
            parent_id,
            now.as_str(),
            now.as_str()
        ],
    )
    .await
    .context("create task")?;

    Ok(TaskRow {
        id,
        task_type: task_type.to_string(),
        status: status.to_string(),
        agent_id: agent.to_string(),
        payload: payload.to_string(),
        run_id: run_id.to_string(),
        current_stage: current_stage.to_string(),
        summary: summary.to_string(),
        parent_id: parent_id.to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Fetch a single task by ID.
pub async fn get_task(db: &Database, task_id: &str) -> Result<Option<TaskRow>> {
    let conn = db_connect(db).await?;

    let sql = format!("SELECT {} FROM tasks WHERE id = ?1", TASK_COLS);
    let mut rows = conn
        .query(&sql, libsql::params![task_id])
        .await
        .context("query task by id")?;

    match rows.next().await.context("read row")? {
        Some(row) => Ok(Some(row_to_task(&row)?)),
        None => Ok(None),
    }
}

/// List tasks with optional status, agent, and parent_id filters, ordered by newest first.
pub async fn list_tasks(
    db: &Database,
    limit: u32,
    status_filter: Option<&str>,
    agent_filter: Option<&str>,
    parent_id_filter: Option<&str>,
) -> Result<Vec<TaskRow>> {
    let conn = db_connect(db).await?;

    let mut sql = format!("SELECT {} FROM tasks WHERE 1=1", TASK_COLS);
    let mut param_values: Vec<libsql::Value> = Vec::new();

    if let Some(status) = status_filter {
        param_values.push(libsql::Value::Text(status.to_string()));
        sql.push_str(&format!(" AND status = ?{}", param_values.len()));
    }
    if let Some(agent) = agent_filter {
        param_values.push(libsql::Value::Text(agent.to_string()));
        sql.push_str(&format!(" AND agent_id = ?{}", param_values.len()));
    }
    if let Some(pid) = parent_id_filter {
        param_values.push(libsql::Value::Text(pid.to_string()));
        sql.push_str(&format!(" AND parent_id = ?{}", param_values.len()));
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

/// Update a task's status, agent, payload, current_stage, and/or summary.
/// Returns the updated row.
pub async fn update_task(
    db: &Database,
    task_id: &str,
    status: Option<&str>,
    agent_id: Option<&str>,
    payload: Option<&str>,
    current_stage: Option<&str>,
    summary: Option<&str>,
) -> Result<Option<TaskRow>> {
    let conn = db_connect(db).await?;
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
    if let Some(cs) = current_stage {
        param_values.push(libsql::Value::Text(cs.to_string()));
        set_parts.push(format!("current_stage = ?{}", param_values.len()));
    }
    if let Some(sm) = summary {
        param_values.push(libsql::Value::Text(sm.to_string()));
        set_parts.push(format!("summary = ?{}", param_values.len()));
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
    let conn = db_connect(db).await?;

    let rows_affected = conn
        .execute("DELETE FROM tasks WHERE id = ?1", libsql::params![task_id])
        .await
        .context("delete task")?;

    Ok(rows_affected > 0)
}

/// Get the current (most recent) task. Prefers running tasks over others.
///
/// Returns the most recently created task with status "running", or the most
/// recently created task of any status if none is running.
pub async fn get_current_task(db: &Database) -> Result<Option<TaskRow>> {
    let conn = db_connect(db).await?;

    // Try running first
    let sql = format!(
        "SELECT {} FROM tasks WHERE status = 'running' ORDER BY created_at DESC LIMIT 1",
        TASK_COLS
    );
    let mut rows = conn
        .query(&sql, ())
        .await
        .context("get current running task")?;
    if let Some(row) = rows.next().await.context("read row")? {
        return Ok(Some(row_to_task(&row)?));
    }

    // Fall back to most recent of any status
    let sql = format!(
        "SELECT {} FROM tasks ORDER BY created_at DESC LIMIT 1",
        TASK_COLS
    );
    let mut rows = conn.query(&sql, ()).await.context("get most recent task")?;
    match rows.next().await.context("read row")? {
        Some(row) => Ok(Some(row_to_task(&row)?)),
        None => Ok(None),
    }
}

/// Find the running task associated with a pipeline run_id.
pub async fn get_task_by_run_id(db: &Database, run_id: &str) -> Result<Option<TaskRow>> {
    let conn = db_connect(db).await?;

    let sql = format!(
        "SELECT {} FROM tasks WHERE run_id = ?1 ORDER BY created_at DESC LIMIT 1",
        TASK_COLS
    );
    let mut rows = conn
        .query(&sql, libsql::params![run_id])
        .await
        .context("get task by run_id")?;

    match rows.next().await.context("read row")? {
        Some(row) => Ok(Some(row_to_task(&row)?)),
        None => Ok(None),
    }
}

/// List all subtasks of a parent task, ordered by creation time.
pub async fn list_subtasks(db: &Database, parent_id: &str, limit: u32) -> Result<Vec<TaskRow>> {
    let conn = db_connect(db).await?;

    let sql = format!(
        "SELECT {} FROM tasks WHERE parent_id = ?1 ORDER BY created_at ASC LIMIT ?2",
        TASK_COLS
    );
    let mut rows = conn
        .query(&sql, libsql::params![parent_id, i64::from(limit)])
        .await
        .context("list subtasks")?;

    let mut tasks = Vec::new();
    while let Some(row) = rows.next().await.context("read row")? {
        tasks.push(row_to_task(&row)?);
    }
    Ok(tasks)
}

/// Count subtasks of a parent task. Returns `(total, completed)`.
pub async fn count_subtasks(db: &Database, parent_id: &str) -> Result<(u32, u32)> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT COUNT(*), SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END)
             FROM tasks WHERE parent_id = ?1",
            libsql::params![parent_id],
        )
        .await
        .context("count subtasks")?;

    match rows.next().await.context("read count row")? {
        Some(row) => {
            let total = row.get::<i64>(0).unwrap_or(0) as u32;
            let completed = row.get::<i64>(1).unwrap_or(0) as u32;
            Ok((total, completed))
        }
        None => Ok((0, 0)),
    }
}

// ─── Task logs ──────────────────────────────────────────────────────────────

/// Row returned from task_logs queries.
#[derive(Debug, Clone)]
pub struct TaskLogRow {
    pub id: String,
    pub task_id: String,
    pub level: String,
    pub message: String,
    pub detail: String,
    pub agent_id: String,
    pub stage: String,
    pub created_at: String,
}

fn row_to_task_log(row: &libsql::Row) -> Result<TaskLogRow> {
    Ok(TaskLogRow {
        id: row.get::<String>(0).context("read id")?,
        task_id: row.get::<String>(1).context("read task_id")?,
        level: row.get::<String>(2).context("read level")?,
        message: row.get::<String>(3).context("read message")?,
        detail: row.get::<String>(4).unwrap_or_default(),
        agent_id: row.get::<String>(5).unwrap_or_default(),
        stage: row.get::<String>(6).unwrap_or_default(),
        created_at: row.get::<String>(7).context("read created_at")?,
    })
}

/// Insert a new task log entry and return the full row.
pub async fn create_task_log(
    db: &Database,
    task_id: &str,
    level: &str,
    message: &str,
    detail: &str,
    agent_id: &str,
    stage: &str,
) -> Result<TaskLogRow> {
    let conn = db_connect(db).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO task_logs (id, task_id, level, message, detail, agent_id, stage, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        libsql::params![
            id.as_str(),
            task_id,
            level,
            message,
            detail,
            agent_id,
            stage,
            now.as_str()
        ],
    )
    .await
    .context("create task log")?;

    Ok(TaskLogRow {
        id,
        task_id: task_id.to_string(),
        level: level.to_string(),
        message: message.to_string(),
        detail: detail.to_string(),
        agent_id: agent_id.to_string(),
        stage: stage.to_string(),
        created_at: now,
    })
}

/// List log entries for a task, ordered by creation time (oldest first).
/// Supports pagination via `limit` and `offset`.
pub async fn list_task_logs(
    db: &Database,
    task_id: &str,
    limit: u32,
    offset: u32,
) -> Result<Vec<TaskLogRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, task_id, level, message, detail, agent_id, stage, created_at
             FROM task_logs WHERE task_id = ?1
             ORDER BY created_at ASC LIMIT ?2 OFFSET ?3",
            libsql::params![task_id, i64::from(limit), i64::from(offset)],
        )
        .await
        .context("list task logs")?;

    let mut logs = Vec::new();
    while let Some(row) = rows.next().await.context("read task_log row")? {
        logs.push(row_to_task_log(&row)?);
    }

    Ok(logs)
}

/// Count total log entries for a task (for pagination).
pub async fn count_task_logs(db: &Database, task_id: &str) -> Result<u32> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM task_logs WHERE task_id = ?1",
            libsql::params![task_id],
        )
        .await
        .context("count task logs")?;

    match rows.next().await.context("read count row")? {
        Some(row) => Ok(row.get::<i64>(0).unwrap_or(0) as u32),
        None => Ok(0),
    }
}

/// Get all task log messages concatenated as text, limited to `max_chars`.
/// Used by the evaluation flow to build a summarisable output.
#[allow(dead_code)]
pub async fn get_task_logs_text(db: &Database, task_id: &str, max_chars: usize) -> Result<String> {
    let conn = db_connect(db).await?;
    let mut rows = conn
        .query(
            "SELECT message, detail FROM task_logs WHERE task_id = ?1 ORDER BY created_at ASC",
            libsql::params![task_id],
        )
        .await
        .context("get task logs text")?;

    let mut text = String::new();
    while let Some(row) = rows.next().await.context("read log row")? {
        let msg = row.get::<String>(0).unwrap_or_default();
        let detail = row.get::<String>(1).unwrap_or_default();
        if !msg.is_empty() {
            text.push_str(&msg);
            text.push('\n');
        }
        if !detail.is_empty() && text.len() < max_chars {
            text.push_str(&detail);
            text.push('\n');
        }
        if text.len() >= max_chars {
            text.truncate(max_chars);
            break;
        }
    }
    Ok(text)
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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;

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
#[allow(dead_code)]
pub async fn get_active_pipeline_runs(db: &Database) -> Result<Vec<PipelineRow>> {
    let conn = db_connect(db).await?;

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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;

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
    let conn = db_connect(db).await?;
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
    let conn = db_connect(db).await?;

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

    let mut rows = conn
        .query(&sql, param_values)
        .await
        .context("list pipeline runs")?;
    let mut runs = Vec::new();

    while let Some(row) = rows.next().await.context("read pipeline row")? {
        runs.push(row_to_pipeline(&row)?);
    }

    Ok(runs)
}
