use crate::task_db::db_connect;
use anyhow::{Context, Result};
use libsql::Database;
// ─── Row structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub scope: String,
    pub category: String,
    pub key: String,
    pub metadata: String,
    pub tags: String,
    pub agent_id: String,
    pub run_id: String,
    pub skill_id: String,
    pub relevance_score: f64,
    pub access_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct MemoryTierRow {
    pub id: String,
    pub memory_id: String,
    pub tier: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TaskMemoryRow {
    pub id: String,
    pub task_id: String,
    pub memory_id: String,
    pub created_at: String,
}

// ─── Memory CRUD ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn create_memory(
    db: &Database,
    scope: &str,
    category: &str,
    key: &str,
    metadata: &str,
    tags: &str,
    agent_id: &str,
    run_id: &str,
    skill_id: &str,
    relevance_score: f64,
) -> Result<MemoryRow> {
    let conn = db_connect(db).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO memories (id, scope, category, key, metadata, tags, agent_id, run_id, skill_id, relevance_score, access_count, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?11)",
        libsql::params![
            id.as_str(),
            scope,
            category,
            key,
            metadata,
            tags,
            agent_id,
            run_id,
            skill_id,
            relevance_score,
            now.as_str()
        ],
    )
    .await
    .context("insert memory")?;

    Ok(MemoryRow {
        id,
        scope: scope.to_string(),
        category: category.to_string(),
        key: key.to_string(),
        metadata: metadata.to_string(),
        tags: tags.to_string(),
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        skill_id: skill_id.to_string(),
        relevance_score,
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn get_memory(db: &Database, memory_id: &str) -> Result<Option<MemoryRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, scope, category, key, metadata, tags, agent_id, run_id, skill_id, relevance_score, access_count, created_at, updated_at
             FROM memories WHERE id = ?1",
            libsql::params![memory_id],
        )
        .await
        .context("get memory")?;

    match rows.next().await.context("read memory row")? {
        Some(row) => Ok(Some(row_to_memory(&row)?)),
        None => Ok(None),
    }
}

#[allow(dead_code)]
pub async fn get_memory_by_key(db: &Database, key: &str) -> Result<Option<MemoryRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, scope, category, key, metadata, tags, agent_id, run_id, skill_id, relevance_score, access_count, created_at, updated_at
             FROM memories WHERE key = ?1",
            libsql::params![key],
        )
        .await
        .context("get memory by key")?;

    match rows.next().await.context("read memory row")? {
        Some(row) => Ok(Some(row_to_memory(&row)?)),
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn list_memories(
    db: &Database,
    limit: u32,
    scope: Option<&str>,
    category: Option<&str>,
    agent_id: Option<&str>,
    run_id: Option<&str>,
    skill_id: Option<&str>,
    tag: Option<&str>,
) -> Result<Vec<MemoryRow>> {
    let conn = db_connect(db).await?;

    let mut sql = String::from(
        "SELECT id, scope, category, key, metadata, tags, agent_id, run_id, skill_id, relevance_score, access_count, created_at, updated_at
         FROM memories WHERE 1=1",
    );
    let mut params: Vec<libsql::Value> = Vec::new();
    let mut idx = 1;

    if let Some(s) = scope {
        sql.push_str(&format!(" AND scope = ?{idx}"));
        params.push(s.to_string().into());
        idx += 1;
    }
    if let Some(c) = category {
        sql.push_str(&format!(" AND category = ?{idx}"));
        params.push(c.to_string().into());
        idx += 1;
    }
    if let Some(a) = agent_id {
        sql.push_str(&format!(" AND agent_id = ?{idx}"));
        params.push(a.to_string().into());
        idx += 1;
    }
    if let Some(r) = run_id {
        sql.push_str(&format!(" AND run_id = ?{idx}"));
        params.push(r.to_string().into());
        idx += 1;
    }
    if let Some(s) = skill_id {
        sql.push_str(&format!(" AND skill_id = ?{idx}"));
        params.push(s.to_string().into());
        idx += 1;
    }
    if let Some(t) = tag {
        sql.push_str(&format!(" AND tags LIKE '%' || ?{idx} || '%'"));
        params.push(t.to_string().into());
        idx += 1;
    }

    sql.push_str(&format!(
        " ORDER BY relevance_score DESC, updated_at DESC LIMIT ?{idx}"
    ));
    params.push(libsql::Value::Integer(i64::from(limit)));

    let mut rows = conn
        .query(&sql, libsql::params_from_iter(params))
        .await
        .context("list memories")?;

    let mut memories = Vec::new();
    while let Some(row) = rows.next().await.context("read memory row")? {
        memories.push(row_to_memory(&row)?);
    }

    Ok(memories)
}

pub async fn update_memory(
    db: &Database,
    memory_id: &str,
    metadata: Option<&str>,
    tags: Option<&str>,
    relevance_score: Option<f64>,
) -> Result<Option<MemoryRow>> {
    let conn = db_connect(db).await?;
    let now = chrono::Utc::now().to_rfc3339();

    let mut sets = vec!["updated_at = ?1".to_string()];
    let mut params: Vec<libsql::Value> = vec![now.clone().into()];
    let mut idx = 2;

    if let Some(m) = metadata {
        sets.push(format!("metadata = ?{idx}"));
        params.push(m.to_string().into());
        idx += 1;
    }
    if let Some(t) = tags {
        sets.push(format!("tags = ?{idx}"));
        params.push(t.to_string().into());
        idx += 1;
    }
    if let Some(r) = relevance_score {
        sets.push(format!("relevance_score = ?{idx}"));
        params.push(libsql::Value::Real(r));
        idx += 1;
    }

    let sql = format!("UPDATE memories SET {} WHERE id = ?{idx}", sets.join(", "));
    params.push(memory_id.to_string().into());

    conn.execute(&sql, libsql::params_from_iter(params))
        .await
        .context("update memory")?;

    get_memory(db, memory_id).await
}

pub async fn delete_memory(db: &Database, memory_id: &str) -> Result<bool> {
    let conn = db_connect(db).await?;

    // Delete related tiers and task bindings first
    conn.execute(
        "DELETE FROM memory_tiers WHERE memory_id = ?1",
        libsql::params![memory_id],
    )
    .await
    .context("delete memory tiers")?;

    conn.execute(
        "DELETE FROM task_memories WHERE memory_id = ?1",
        libsql::params![memory_id],
    )
    .await
    .context("delete task_memories")?;

    let affected = conn
        .execute(
            "DELETE FROM memories WHERE id = ?1",
            libsql::params![memory_id],
        )
        .await
        .context("delete memory")?;

    Ok(affected > 0)
}

pub async fn touch_memory(db: &Database, memory_id: &str) -> Result<()> {
    let conn = db_connect(db).await?;
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE memories SET access_count = access_count + 1, updated_at = ?1 WHERE id = ?2",
        libsql::params![now.as_str(), memory_id],
    )
    .await
    .context("touch memory")?;

    Ok(())
}

pub async fn count_memories_by_scope(db: &Database) -> Result<Vec<(String, i64)>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT scope, COUNT(*) as cnt FROM memories GROUP BY scope ORDER BY cnt DESC",
            (),
        )
        .await
        .context("count memories by scope")?;

    let mut counts = Vec::new();
    while let Some(row) = rows.next().await.context("read count row")? {
        let scope = row.get::<String>(0).context("read scope")?;
        let count = row.get::<i64>(1).context("read count")?;
        counts.push((scope, count));
    }

    Ok(counts)
}

// ─── Tier CRUD ───────────────────────────────────────────────────────────────

pub async fn upsert_tier(
    db: &Database,
    memory_id: &str,
    tier: &str,
    content: &str,
) -> Result<MemoryTierRow> {
    let conn = db_connect(db).await?;
    let now = chrono::Utc::now().to_rfc3339();

    // Check if tier exists for this memory
    let mut existing = conn
        .query(
            "SELECT id FROM memory_tiers WHERE memory_id = ?1 AND tier = ?2",
            libsql::params![memory_id, tier],
        )
        .await
        .context("check existing tier")?;

    let id = if let Some(row) = existing.next().await.context("read existing tier")? {
        let existing_id = row.get::<String>(0).context("read tier id")?;
        conn.execute(
            "UPDATE memory_tiers SET content = ?1, updated_at = ?2 WHERE id = ?3",
            libsql::params![content, now.as_str(), existing_id.as_str()],
        )
        .await
        .context("update tier")?;
        existing_id
    } else {
        let new_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_tiers (id, memory_id, tier, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            libsql::params![new_id.as_str(), memory_id, tier, content, now.as_str()],
        )
        .await
        .context("insert tier")?;
        new_id
    };

    Ok(MemoryTierRow {
        id,
        memory_id: memory_id.to_string(),
        tier: tier.to_string(),
        content: content.to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn get_tiers(db: &Database, memory_id: &str) -> Result<Vec<MemoryTierRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, memory_id, tier, content, created_at, updated_at
             FROM memory_tiers WHERE memory_id = ?1 ORDER BY tier",
            libsql::params![memory_id],
        )
        .await
        .context("get tiers")?;

    let mut tiers = Vec::new();
    while let Some(row) = rows.next().await.context("read tier row")? {
        tiers.push(row_to_tier(&row)?);
    }

    Ok(tiers)
}

pub async fn get_tier(db: &Database, memory_id: &str, tier: &str) -> Result<Option<MemoryTierRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT id, memory_id, tier, content, created_at, updated_at
             FROM memory_tiers WHERE memory_id = ?1 AND tier = ?2",
            libsql::params![memory_id, tier],
        )
        .await
        .context("get tier")?;

    match rows.next().await.context("read tier row")? {
        Some(row) => Ok(Some(row_to_tier(&row)?)),
        None => Ok(None),
    }
}

pub async fn delete_tier(db: &Database, memory_id: &str, tier: &str) -> Result<bool> {
    let conn = db_connect(db).await?;

    let affected = conn
        .execute(
            "DELETE FROM memory_tiers WHERE memory_id = ?1 AND tier = ?2",
            libsql::params![memory_id, tier],
        )
        .await
        .context("delete tier")?;

    Ok(affected > 0)
}

// ─── Task-Memory binding ─────────────────────────────────────────────────────

pub async fn bind_task_memory(
    db: &Database,
    task_id: &str,
    memory_id: &str,
) -> Result<TaskMemoryRow> {
    let conn = db_connect(db).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    // Use INSERT OR IGNORE to handle duplicate bindings gracefully
    conn.execute(
        "INSERT OR IGNORE INTO task_memories (id, task_id, memory_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        libsql::params![id.as_str(), task_id, memory_id, now.as_str()],
    )
    .await
    .context("bind task memory")?;

    Ok(TaskMemoryRow {
        id,
        task_id: task_id.to_string(),
        memory_id: memory_id.to_string(),
        created_at: now,
    })
}

#[allow(dead_code)]
pub async fn unbind_task_memory(db: &Database, task_id: &str, memory_id: &str) -> Result<bool> {
    let conn = db_connect(db).await?;

    let affected = conn
        .execute(
            "DELETE FROM task_memories WHERE task_id = ?1 AND memory_id = ?2",
            libsql::params![task_id, memory_id],
        )
        .await
        .context("unbind task memory")?;

    Ok(affected > 0)
}

pub async fn list_memories_for_task(
    db: &Database,
    task_id: &str,
    limit: u32,
) -> Result<Vec<MemoryRow>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT m.id, m.scope, m.category, m.key, m.metadata, m.tags, m.agent_id, m.run_id, m.skill_id, m.relevance_score, m.access_count, m.created_at, m.updated_at
             FROM memories m
             INNER JOIN task_memories tm ON m.id = tm.memory_id
             WHERE tm.task_id = ?1
             ORDER BY m.relevance_score DESC, m.updated_at DESC
             LIMIT ?2",
            libsql::params![task_id, i64::from(limit)],
        )
        .await
        .context("list memories for task")?;

    let mut memories = Vec::new();
    while let Some(row) = rows.next().await.context("read memory row")? {
        memories.push(row_to_memory(&row)?);
    }

    Ok(memories)
}

pub async fn list_tasks_for_memory(db: &Database, memory_id: &str) -> Result<Vec<String>> {
    let conn = db_connect(db).await?;

    let mut rows = conn
        .query(
            "SELECT task_id FROM task_memories WHERE memory_id = ?1 ORDER BY created_at DESC",
            libsql::params![memory_id],
        )
        .await
        .context("list tasks for memory")?;

    let mut task_ids = Vec::new();
    while let Some(row) = rows.next().await.context("read task_id")? {
        task_ids.push(row.get::<String>(0).context("read task_id")?);
    }

    Ok(task_ids)
}

// ─── Search ──────────────────────────────────────────────────────────────────

pub async fn search_memories(
    db: &Database,
    query: &str,
    limit: u32,
    scope: Option<&str>,
    category: Option<&str>,
    agent_id: Option<&str>,
    tier: Option<&str>,
) -> Result<Vec<MemoryRow>> {
    let conn = db_connect(db).await?;

    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return list_memories(db, limit, scope, category, agent_id, None, None, None).await;
    }

    // Build a query that JOINs memory_tiers and searches content
    let mut sql = String::from(
        "SELECT DISTINCT m.id, m.scope, m.category, m.key, m.metadata, m.tags, m.agent_id, m.run_id, m.skill_id, m.relevance_score, m.access_count, m.created_at, m.updated_at
         FROM memories m
         INNER JOIN memory_tiers mt ON m.id = mt.memory_id
         WHERE 1=1",
    );
    let mut params: Vec<libsql::Value> = Vec::new();
    let mut idx = 1;

    // Filter by tier if specified
    if let Some(t) = tier {
        sql.push_str(&format!(" AND mt.tier = ?{idx}"));
        params.push(t.to_string().into());
        idx += 1;
    }

    // Text search: each word must appear in tier content OR memory key
    for word in &words {
        sql.push_str(&format!(
            " AND (mt.content LIKE '%' || ?{idx} || '%' OR m.key LIKE '%' || ?{idx} || '%')"
        ));
        params.push(word.to_string().into());
        idx += 1;
    }

    // Scope/category/agent filters
    if let Some(s) = scope {
        sql.push_str(&format!(" AND m.scope = ?{idx}"));
        params.push(s.to_string().into());
        idx += 1;
    }
    if let Some(c) = category {
        sql.push_str(&format!(" AND m.category = ?{idx}"));
        params.push(c.to_string().into());
        idx += 1;
    }
    if let Some(a) = agent_id {
        sql.push_str(&format!(" AND m.agent_id = ?{idx}"));
        params.push(a.to_string().into());
        idx += 1;
    }

    sql.push_str(&format!(
        " ORDER BY m.relevance_score DESC, m.access_count DESC, m.updated_at DESC LIMIT ?{idx}"
    ));
    params.push(libsql::Value::Integer(i64::from(limit)));

    let mut rows = conn
        .query(&sql, libsql::params_from_iter(params))
        .await
        .context("search memories")?;

    let mut memories = Vec::new();
    while let Some(row) = rows.next().await.context("read memory row")? {
        memories.push(row_to_memory(&row)?);
    }

    Ok(memories)
}

// ─── Row parsers ─────────────────────────────────────────────────────────────

fn row_to_memory(row: &libsql::Row) -> Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get::<String>(0).context("read id")?,
        scope: row.get::<String>(1).context("read scope")?,
        category: row.get::<String>(2).context("read category")?,
        key: row.get::<String>(3).context("read key")?,
        metadata: row.get::<String>(4).unwrap_or_else(|_| "{}".to_string()),
        tags: row.get::<String>(5).unwrap_or_default(),
        agent_id: row.get::<String>(6).unwrap_or_default(),
        run_id: row.get::<String>(7).unwrap_or_default(),
        skill_id: row.get::<String>(8).unwrap_or_default(),
        relevance_score: row.get::<f64>(9).unwrap_or(0.0),
        access_count: row.get::<i64>(10).unwrap_or(0),
        created_at: row.get::<String>(11).context("read created_at")?,
        updated_at: row.get::<String>(12).context("read updated_at")?,
    })
}

fn row_to_tier(row: &libsql::Row) -> Result<MemoryTierRow> {
    Ok(MemoryTierRow {
        id: row.get::<String>(0).context("read id")?,
        memory_id: row.get::<String>(1).context("read memory_id")?,
        tier: row.get::<String>(2).context("read tier")?,
        content: row.get::<String>(3).unwrap_or_default(),
        created_at: row.get::<String>(4).context("read created_at")?,
        updated_at: row.get::<String>(5).context("read updated_at")?,
    })
}

// Suppress dead-code warning — used by other modules but compiler doesn't see it yet
#[allow(dead_code)]
fn _row_to_task_memory(row: &libsql::Row) -> Result<TaskMemoryRow> {
    Ok(TaskMemoryRow {
        id: row.get::<String>(0).context("read id")?,
        task_id: row.get::<String>(1).context("read task_id")?,
        memory_id: row.get::<String>(2).context("read memory_id")?,
        created_at: row.get::<String>(3).context("read created_at")?,
    })
}
