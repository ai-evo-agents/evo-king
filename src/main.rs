mod agent_manager;
mod config_watcher;
mod cron_manager;
mod doctor;
mod error;
mod gateway_manager;
mod pipeline_coordinator;
mod socket_server;
mod state;
mod task_db;

use agent_manager::AgentRegistry;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use clap::{Parser, Subcommand};
use evo_common::logging::init_logging;
use pipeline_coordinator::PipelineCoordinator;
use serde::Deserialize;
use serde_json::json;
use socketioxide::SocketIo;
use state::KingState;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::info;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_DB_PATH: &str = "king.db";
const DEFAULT_GATEWAY_CONFIG: &str = "../evo-gateway/gateway.json";
const DEFAULT_KERNEL_AGENTS_DIR: &str = "..";
const DEFAULT_RUNNER_BINARY: &str = "../evo-agents/target/release/evo-runner";
const DEFAULT_DASHBOARD_DIR: &str = "./dashboard";

/// evo-king — central orchestrator for the evo multi-agent system.
#[derive(Parser)]
#[command(name = "evo-king", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate (and optionally fix) the ~/.evo-agents/ installation.
    Doctor {
        /// Attempt to automatically fix issues.
        #[arg(long)]
        fix: bool,

        /// Path to the evo-agents installation directory.
        #[arg(long, default_value = "~/.evo-agents")]
        evo_home: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Structured JSON logging to logs/king.log (+ stdout)
    let _log_guard = init_logging("king");

    let cli = Cli::parse();

    // ── Handle subcommands that exit early ──────────────────────────────────
    if let Some(Commands::Doctor { fix, evo_home }) = cli.command {
        return doctor::run_doctor(&evo_home, fix).await;
    }

    // ── Server mode (default) ───────────────────────────────────────────────

    let port: u16 = std::env::var("KING_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let db_path = std::env::var("KING_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());

    let gateway_config_path =
        std::env::var("GATEWAY_CONFIG_PATH").unwrap_or_else(|_| DEFAULT_GATEWAY_CONFIG.to_string());

    let kernel_agents_dir = std::env::var("KERNEL_AGENTS_DIR")
        .unwrap_or_else(|_| DEFAULT_KERNEL_AGENTS_DIR.to_string());

    let runner_binary =
        std::env::var("RUNNER_BINARY").unwrap_or_else(|_| DEFAULT_RUNNER_BINARY.to_string());

    let dashboard_dir =
        std::env::var("EVO_DASHBOARD_DIR").unwrap_or_else(|_| DEFAULT_DASHBOARD_DIR.to_string());

    // ── Database ──────────────────────────────────────────────────────────────
    info!(db = %db_path, "initializing database");
    let db = task_db::init_db(&db_path)
        .await
        .context("Failed to initialize Turso/libSQL database")?;

    // ── Socket.IO server ──────────────────────────────────────────────────────
    let (socket_layer, io) = SocketIo::new_layer();

    // ── Shared state ──────────────────────────────────────────────────────────
    let http_client = Arc::new(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?,
    );

    let agent_registry = Arc::new(AgentRegistry::new());
    let db_arc = Arc::new(db);

    let pipeline_coordinator = Arc::new(PipelineCoordinator::new(Arc::clone(&db_arc), io.clone()));

    let state = Arc::new(KingState {
        db: db_arc,
        io: io.clone(),
        http_client,
        gateway_config_path: gateway_config_path.clone(),
        agent_registry: Arc::clone(&agent_registry),
        pipeline_coordinator: Arc::clone(&pipeline_coordinator),
    });

    // ── Socket.IO event handlers ──────────────────────────────────────────────
    socket_server::register_handlers(io, Arc::clone(&state));

    // ── Config watcher (background task) ─────────────────────────────────────
    {
        let watcher_state = Arc::clone(&state);
        let config_path = gateway_config_path.clone();
        tokio::spawn(async move {
            if let Err(e) = config_watcher::watch_config(&config_path, watcher_state).await {
                tracing::error!(err = %e, "config watcher stopped unexpectedly");
            }
        });
    }

    // ── Pipeline timeout monitor (background task) ──────────────────────────
    {
        let monitor = Arc::clone(&pipeline_coordinator);
        tokio::spawn(async move { monitor.timeout_monitor().await });
    }

    // ── Axum server with Socket.IO layer ──────────────────────────────────────
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/agents", get(list_agents_handler))
        .route("/pipeline/start", post(pipeline_start_handler))
        .route("/pipeline/runs", get(pipeline_list_handler))
        .route("/pipeline/runs/{run_id}", get(pipeline_detail_handler))
        // ── Dashboard API endpoints ──────────────────────────────────────────
        .route(
            "/gateway/config",
            get(gateway_config_get_handler).put(gateway_config_put_handler),
        )
        .route("/config-history", get(config_history_handler))
        .route("/tasks", get(tasks_list_handler))
        .route("/task/current", get(task_current_handler))
        .route("/task/{task_id}/logs", get(task_logs_handler))
        .route("/task/{task_id}/subtasks", get(task_subtasks_handler))
        // ── Debug endpoints ─────────────────────────────────────────────────
        .route("/debug/prompt", post(debug_prompt_handler))
        // ── Admin endpoints ──────────────────────────────────────────────────
        .route("/admin/config-sync", post(admin_config_sync_handler))
        .route("/admin/crons", get(admin_list_crons_handler))
        .route("/admin/crons/{name}/run", post(admin_run_cron_handler))
        // Static dashboard files as fallback (API routes take priority)
        .fallback_service(ServeDir::new(&dashboard_dir).append_index_html_on_directories(true))
        // Layers must come AFTER fallback so they wrap the entire router
        // (socket_layer needs to intercept /socket.io/ before route matching)
        .layer(socket_layer)
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "evo-king listening");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;

    // ── Spawn kernel agents AFTER server is listening ─────────────────────────
    {
        let registry = Arc::clone(&agent_registry);
        let root = kernel_agents_dir.clone();
        let binary = runner_binary.clone();
        let king_address = format!("http://0.0.0.0:{port}");
        tokio::spawn(async move {
            // Small delay to ensure server is fully accepting connections
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let spawned =
                agent_manager::spawn_all_kernel_agents(&registry, &root, &binary, &king_address)
                    .await;
            info!(count = spawned.len(), agents = ?spawned, "kernel agents spawned");
        });
    }

    // ── Process monitor (background task, with respawn retry) ────────────────
    {
        let monitor_registry = Arc::clone(&agent_registry);
        let monitor_db = Arc::clone(&state.db);
        let monitor_binary = runner_binary.clone();
        let monitor_addr = format!("http://0.0.0.0:{port}");
        tokio::spawn(agent_manager::monitor_processes(
            monitor_registry,
            monitor_db,
            monitor_binary,
            monitor_addr,
        ));
    }

    // ── Heartbeat watcher (background task) ───────────────────────────────────
    {
        let hb_db = Arc::clone(&state.db);
        let hb_registry = Arc::clone(&agent_registry);
        let hb_binary = runner_binary.clone();
        let hb_addr = format!("http://0.0.0.0:{port}");
        let hb_dir = kernel_agents_dir.clone();
        tokio::spawn(agent_manager::heartbeat_watch_loop(
            hb_db,
            hb_registry,
            hb_binary,
            hb_addr,
            hb_dir,
        ));
    }

    // ── Cron manager (init system jobs + start scheduler) ─────────────────────
    {
        let cron_db = Arc::clone(&state.db);
        if let Err(e) = cron_manager::init_system_crons(&cron_db).await {
            tracing::error!(err = %e, "failed to seed system cron jobs");
        }
        let cron_db2 = Arc::clone(&state.db);
        let cron_io = state.io.clone();
        tokio::spawn(cron_manager::run(cron_db2, cron_io));
    }

    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}

// ─── HTTP handlers ───────────────────────────────────────────────────────────

async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "evo-king" }))
}

/// GET /agents — list all registered agents with full metadata.
async fn list_agents_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match task_db::list_agents(&state.db).await {
        Ok(agents) => {
            let list: Vec<serde_json::Value> = agents
                .iter()
                .map(|a| {
                    json!({
                        "agent_id":       a.agent_id,
                        "role":           a.role,
                        "status":         a.status,
                        "last_heartbeat": a.last_heartbeat,
                        "capabilities":   serde_json::from_str::<serde_json::Value>(&a.capabilities)
                            .unwrap_or(json!([])),
                        "skills":         serde_json::from_str::<serde_json::Value>(&a.skills)
                            .unwrap_or(json!([])),
                        "pid":            a.pid,
                    })
                })
                .collect();
            let count = list.len();
            Json(json!({ "agents": list, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ─── Pipeline HTTP handlers ────────────────────────────────────────────────

/// POST /pipeline/start — manually trigger a new pipeline run.
async fn pipeline_start_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let trigger = body.get("trigger").cloned().unwrap_or(json!("manual"));

    match state.pipeline_coordinator.start_run(trigger).await {
        Ok(run_id) => {
            info!(run_id = %run_id, "pipeline run triggered via HTTP");
            Json(json!({ "success": true, "run_id": run_id }))
        }
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// GET /pipeline/runs — list all pipeline runs.
async fn pipeline_list_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match state.pipeline_coordinator.list_runs(100, None).await {
        Ok(rows) => {
            let runs: Vec<serde_json::Value> = rows.iter().map(pipeline_row_to_json).collect();
            let count = runs.len();
            Json(json!({ "runs": runs, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /pipeline/runs/:run_id — detailed stage history for one run.
async fn pipeline_detail_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    match state.pipeline_coordinator.get_run_stages(&run_id).await {
        Ok(stages) => {
            let list: Vec<serde_json::Value> = stages.iter().map(pipeline_row_to_json).collect();
            let count = list.len();
            Json(json!({ "run_id": run_id, "stages": list, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

fn pipeline_row_to_json(row: &task_db::PipelineRow) -> serde_json::Value {
    let result_val = row
        .result
        .as_ref()
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .unwrap_or(json!(null));

    json!({
        "id":          row.id,
        "run_id":      row.run_id,
        "stage":       row.stage,
        "artifact_id": row.artifact_id,
        "status":      row.status,
        "agent_id":    row.agent_id,
        "result":      result_val,
        "error":       row.error,
        "created_at":  row.created_at,
        "updated_at":  row.updated_at,
    })
}

// ─── Admin HTTP handlers ──────────────────────────────────────────────────────

/// POST /admin/config-sync
///
/// Broadcasts `king:config_update` to all connected agents, prompting them to
/// re-validate their gateway config.  Called by the update agent after it has
/// committed new dependency versions.
async fn admin_config_sync_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    let payload = json!({
        "trigger": "config_sync",
        "reason": "post_dependency_update",
    });

    match state
        .io
        .emit(evo_common::messages::events::KING_CONFIG_UPDATE, &payload)
    {
        Ok(_) => {
            info!("config-sync broadcast sent to all agents");
            Json(json!({ "success": true }))
        }
        Err(e) => {
            tracing::error!(err = %e, "config-sync broadcast failed");
            Json(json!({ "success": false, "error": e.to_string() }))
        }
    }
}

/// GET /admin/crons — list all cron jobs and their last run status.
async fn admin_list_crons_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match task_db::list_cron_jobs(&state.db).await {
        Ok(jobs) => {
            let list: Vec<serde_json::Value> = jobs
                .iter()
                .map(|j| {
                    json!({
                        "id":          j.id,
                        "name":        j.name,
                        "schedule":    j.schedule,
                        "enabled":     j.enabled,
                        "last_run_at": j.last_run_at,
                        "next_run_at": j.next_run_at,
                        "last_status": j.last_status,
                        "last_error":  j.last_error,
                        "created_at":  j.created_at,
                    })
                })
                .collect();
            let count = list.len();
            Json(json!({ "crons": list, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// POST /admin/crons/:name/run — manually trigger a cron job immediately.
async fn admin_run_cron_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let db = Arc::clone(&state.db);
    let io = state.io.clone();

    info!(cron = %name, "manual cron trigger via HTTP");

    match cron_manager::run_now(&name, db, io).await {
        Ok(()) => Json(json!({ "success": true, "cron": name })),
        Err(e) => {
            tracing::warn!(cron = %name, err = %e, "manual cron trigger failed");
            Json(json!({ "success": false, "error": e.to_string() }))
        }
    }
}

// ─── Dashboard API handlers ──────────────────────────────────────────────────

/// GET /gateway/config — read current gateway configuration.
async fn gateway_config_get_handler(
    State(state): State<Arc<KingState>>,
) -> Json<serde_json::Value> {
    let path = &state.gateway_config_path;
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(config) => Json(config),
            Err(e) => Json(json!({ "error": format!("invalid config JSON: {e}") })),
        },
        Err(e) => Json(json!({ "error": format!("cannot read {path}: {e}") })),
    }
}

/// PUT /gateway/config — update gateway configuration.
///
/// Writes the validated config to the gateway config file. The existing
/// config_watcher will detect the change and trigger the full lifecycle
/// (backup, DB log, broadcast to agents).
async fn gateway_config_put_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // Validate by round-tripping through GatewayConfig
    let json_str = body.to_string();
    if let Err(e) = evo_common::config::GatewayConfig::from_json(&json_str) {
        return Json(json!({ "success": false, "error": format!("invalid config: {e}") }));
    }

    // Pretty-print for readability
    let pretty = match serde_json::to_string_pretty(&body) {
        Ok(s) => s,
        Err(e) => {
            return Json(json!({ "success": false, "error": format!("serialize error: {e}") }));
        }
    };

    let path = &state.gateway_config_path;
    match std::fs::write(path, &pretty) {
        Ok(()) => {
            info!(path = %path, "gateway config updated via dashboard");
            Json(json!({ "success": true }))
        }
        Err(e) => Json(json!({ "success": false, "error": format!("write error: {e}") })),
    }
}

/// GET /config-history — list gateway config change history.
async fn config_history_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match task_db::list_config_history(&state.db, 100).await {
        Ok(history) => {
            let list: Vec<serde_json::Value> = history
                .iter()
                .map(|h| {
                    json!({
                        "id":          h.id,
                        "config_hash": h.config_hash,
                        "action":      h.action,
                        "backup_path": h.backup_path,
                        "timestamp":   h.timestamp,
                    })
                })
                .collect();
            let count = list.len();
            Json(json!({ "history": list, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// Query parameters for GET /tasks.
#[derive(Deserialize)]
struct TasksQuery {
    status: Option<String>,
    agent_id: Option<String>,
    parent_id: Option<String>,
    limit: Option<u32>,
}

// ─── Debug HTTP handlers ────────────────────────────────────────────────────

/// POST /debug/prompt — send a debug prompt to an agent role via gateway.
///
/// The agent processes the prompt through its gateway client and emits
/// `debug:response` back via Socket.IO (relayed to dashboard).
async fn debug_prompt_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let agent_role = match body["agent_role"].as_str() {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => return Json(json!({ "success": false, "error": "agent_role is required" })),
    };
    let model = body["model"].as_str().unwrap_or("gpt-4o-mini").to_string();
    let prompt = match body["prompt"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return Json(json!({ "success": false, "error": "prompt is required" })),
    };
    let provider = body["provider"].as_str().map(|s| s.to_string());
    let temperature = body["temperature"].as_f64();
    let max_tokens = body["max_tokens"].as_u64();

    let request_id = uuid::Uuid::new_v4().to_string();
    let room = format!(
        "{}{}",
        evo_common::messages::events::ROOM_ROLE_PREFIX,
        agent_role.replace('-', "_")
    );

    let payload = json!({
        "request_id": request_id,
        "model": model,
        "prompt": prompt,
        "provider": provider,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });

    info!(
        request_id = %request_id,
        role = %agent_role,
        model = %model,
        room = %room,
        "sending debug prompt to agent"
    );

    match state
        .io
        .to(room)
        .emit(evo_common::messages::events::DEBUG_PROMPT, &payload)
    {
        Ok(_) => Json(json!({ "success": true, "request_id": request_id })),
        Err(e) => {
            tracing::error!(err = %e, "failed to emit debug:prompt");
            Json(json!({ "success": false, "error": e.to_string() }))
        }
    }
}

/// Convert a TaskRow to JSON for HTTP responses.
fn task_row_to_json_http(r: &task_db::TaskRow) -> serde_json::Value {
    let payload = serde_json::from_str::<serde_json::Value>(&r.payload).unwrap_or(json!({}));
    json!({
        "id":            r.id,
        "task_type":     r.task_type,
        "status":        r.status,
        "agent_id":      r.agent_id,
        "run_id":        r.run_id,
        "current_stage": r.current_stage,
        "summary":       r.summary,
        "parent_id":     r.parent_id,
        "payload":       payload,
        "created_at":    r.created_at,
        "updated_at":    r.updated_at,
    })
}

/// GET /tasks — list tasks with optional filters.
async fn tasks_list_handler(
    State(state): State<Arc<KingState>>,
    Query(params): Query<TasksQuery>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(50).min(500);
    match task_db::list_tasks(
        &state.db,
        limit,
        params.status.as_deref(),
        params.agent_id.as_deref(),
        params.parent_id.as_deref(),
    )
    .await
    {
        Ok(rows) => {
            let tasks: Vec<serde_json::Value> = rows.iter().map(task_row_to_json_http).collect();
            let count = tasks.len();
            Json(json!({ "tasks": tasks, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /task/current — get the current (running or most recent) task.
async fn task_current_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match task_db::get_current_task(&state.db).await {
        Ok(Some(task)) => Json(json!({ "task": task_row_to_json_http(&task) })),
        Ok(None) => Json(json!({ "task": null })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// Query parameters for GET /task/:id/logs.
#[derive(Deserialize)]
struct TaskLogsQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

/// GET /task/:task_id/logs — list log entries for a task (paginated).
async fn task_logs_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Query(params): Query<TaskLogsQuery>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(100).min(500);
    let offset = params.offset.unwrap_or(0);

    let count = task_db::count_task_logs(&state.db, &task_id)
        .await
        .unwrap_or(0);

    match task_db::list_task_logs(&state.db, &task_id, limit, offset).await {
        Ok(logs) => {
            let list: Vec<serde_json::Value> = logs
                .iter()
                .map(|l| {
                    json!({
                        "id":         l.id,
                        "task_id":    l.task_id,
                        "level":      l.level,
                        "message":    l.message,
                        "detail":     l.detail,
                        "agent_id":   l.agent_id,
                        "stage":      l.stage,
                        "created_at": l.created_at,
                    })
                })
                .collect();
            Json(json!({ "logs": list, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /task/:task_id/subtasks — list subtasks of a task with progress info.
async fn task_subtasks_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Query(params): Query<TasksQuery>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(100).min(500);
    match task_db::list_subtasks(&state.db, &task_id, limit).await {
        Ok(rows) => {
            let subtasks: Vec<serde_json::Value> = rows.iter().map(task_row_to_json_http).collect();
            let (total, completed) = task_db::count_subtasks(&state.db, &task_id)
                .await
                .unwrap_or((0, 0));
            Json(json!({
                "subtasks": subtasks,
                "count": subtasks.len(),
                "progress": { "total": total, "completed": completed },
            }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
