mod agent_manager;
mod config_watcher;
mod cron_manager;
mod doctor;
mod error;
mod gateway_manager;
mod memory_db;
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

const DEFAULT_PORT: u16 = 3300;
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
        .route("/debug/bash", post(debug_bash_handler))
        // ── Admin endpoints ──────────────────────────────────────────────────
        .route("/admin/config-sync", post(admin_config_sync_handler))
        .route("/admin/crons", get(admin_list_crons_handler))
        .route("/admin/crons/{name}/run", post(admin_run_cron_handler))
        // ── Memory endpoints ───────────────────────────────────────────────
        .route(
            "/memories",
            get(memories_list_handler).post(memory_create_handler),
        )
        .route("/memories/search", post(memory_search_handler))
        .route("/memories/stats", get(memory_stats_handler))
        .route(
            "/memories/{memory_id}",
            get(memory_get_handler)
                .put(memory_update_handler)
                .delete(memory_delete_handler),
        )
        .route(
            "/memories/{memory_id}/tiers",
            get(memory_tiers_handler).post(memory_tier_upsert_handler),
        )
        .route(
            "/memories/{memory_id}/tiers/{tier}",
            get(memory_tier_get_handler).delete(memory_tier_delete_handler),
        )
        .route("/memories/{memory_id}/tasks", get(memory_tasks_handler))
        .route(
            "/task/{task_id}/memories",
            get(task_memories_handler).post(task_memory_bind_handler),
        )
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

    // Create a task for this debug prompt
    let task_id = match task_db::create_task(
        &state.db,
        "debug_prompt",
        "running",
        None,
        &json!({ "prompt": prompt, "model": model, "agent_role": agent_role }).to_string(),
        "", // run_id
        "", // current_stage
        "", // summary
        "", // parent_id
    )
    .await
    {
        Ok(row) => row.id,
        Err(e) => {
            tracing::warn!(err = %e, "failed to create task for debug prompt");
            String::new()
        }
    };

    // Emit task:invite to kernel room so agents can join the task room
    if !task_id.is_empty() {
        let invite = json!({ "task_id": task_id, "task_type": "debug_prompt" });
        let _ = state
            .io
            .to(evo_common::messages::events::ROOM_KERNEL)
            .emit(evo_common::messages::events::TASK_INVITE, &invite);
        // Notify dashboard of new task
        let _ = state.io.to("dashboard").emit(
            evo_common::messages::events::TASK_CHANGED,
            json!({ "action": "created", "task": { "id": task_id, "task_type": "debug_prompt", "status": "running" } }),
        );
    }

    let payload = json!({
        "request_id": request_id,
        "task_id": task_id,
        "model": model,
        "prompt": prompt,
        "provider": provider,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });

    info!(
        request_id = %request_id,
        task_id = %task_id,
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
        Ok(_) => Json(json!({ "success": true, "request_id": request_id, "task_id": task_id })),
        Err(e) => {
            tracing::error!(err = %e, "failed to emit debug:prompt");
            Json(json!({ "success": false, "error": e.to_string() }))
        }
    }
}

/// POST /debug/bash — run a bash command in a PTY and stream output to dashboard.
///
/// Body: `{ "command": "...", "request_id": "optional-uuid" }`
///
/// Returns immediately with `{ success, request_id }`. Output streams via
/// Socket.IO `debug:stream` events to the dashboard room. A final
/// `debug:response` event signals completion with the process exit code.
async fn debug_bash_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let command = match body["command"].as_str() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return Json(json!({ "success": false, "error": "command is required" })),
    };
    let request_id = body["request_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Create a task for this bash command
    let task_id = match task_db::create_task(
        &state.db,
        "debug_bash",
        "running",
        None,
        &json!({ "command": command }).to_string(),
        "", // run_id
        "", // current_stage
        "", // summary
        "", // parent_id
    )
    .await
    {
        Ok(row) => row.id,
        Err(e) => {
            tracing::warn!(err = %e, "failed to create task for debug bash");
            String::new()
        }
    };

    info!(
        request_id = %request_id,
        task_id = %task_id,
        command = %command,
        "debug bash command requested"
    );

    // Emit task:invite to kernel room
    if !task_id.is_empty() {
        let invite = json!({ "task_id": task_id, "task_type": "debug_bash" });
        let _ = state
            .io
            .to(evo_common::messages::events::ROOM_KERNEL)
            .emit(evo_common::messages::events::TASK_INVITE, &invite);
        // Notify dashboard of new task
        let _ = state.io.to("dashboard").emit(
            evo_common::messages::events::TASK_CHANGED,
            json!({ "action": "created", "task": { "id": task_id, "task_type": "debug_bash", "status": "running" } }),
        );
    }

    let io = state.io.clone();
    let db = Arc::clone(&state.db);
    let req_id = request_id.clone();
    let tid = task_id.clone();
    tokio::spawn(run_bash_pty_with_task(command, req_id, io, tid, db));

    Json(json!({ "success": true, "request_id": request_id, "task_id": task_id }))
}

/// Async wrapper: spawns PTY subprocess in a blocking thread, streams output
/// to the dashboard room via Socket.IO, then emits a final `debug:response`.
///
/// **Deprecated:** Use `run_bash_pty_with_task` instead, which adds task tracking,
/// task room output, and automatic evaluation dispatch.
#[allow(dead_code)]
async fn run_bash_pty(command: String, request_id: String, io: SocketIo) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let start = std::time::Instant::now();

    // Async emitter: forwards PTY chunks to the dashboard
    let io_emit = io.clone();
    let req_id_emit = request_id.clone();
    let emit_task = tokio::spawn(async move {
        let mut chunk_index = 0u32;
        while let Some(text) = rx.recv().await {
            let _ = io_emit.to("dashboard").emit(
                "dashboard:event",
                serde_json::json!({
                    "event": "debug:stream",
                    "data": {
                        "request_id": req_id_emit,
                        "delta": text,
                        "chunk_index": chunk_index,
                    }
                }),
            );
            chunk_index += 1;
        }
    });

    // Blocking PTY runner — tx is moved in; dropping it closes the channel
    let exit_code: i32 = tokio::task::spawn_blocking(move || pty_run_blocking(command, tx))
        .await
        .unwrap_or(1);

    // Channel is now closed (tx dropped); wait for emitter to drain
    let _ = emit_task.await;

    let elapsed = start.elapsed().as_millis() as u64;
    let _ = io.to("dashboard").emit(
        "dashboard:event",
        serde_json::json!({
            "event": "debug:response",
            "data": {
                "request_id": request_id,
                "response": format!("\n[exit {}]", exit_code),
                "latency_ms": elapsed,
                "entry_type": "bash",
            }
        }),
    );
}

/// Async wrapper with task tracking: spawns PTY subprocess, streams output to
/// both the dashboard room and the task-specific room, batches task_logs every
/// 1.5s, accumulates output (max 8KB) for evaluation, and emits `task:evaluate`
/// on completion.
async fn run_bash_pty_with_task(
    command: String,
    request_id: String,
    io: SocketIo,
    task_id: String,
    db: Arc<libsql::Database>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let start = std::time::Instant::now();
    let has_task = !task_id.is_empty();

    // Async emitter: forwards PTY chunks to dashboard + task room, batches logs
    let io_emit = io.clone();
    let req_id_emit = request_id.clone();
    let task_id_emit = task_id.clone();
    let db_emit = Arc::clone(&db);
    let emit_handle = tokio::spawn(async move {
        let mut chunk_index = 0u32;
        let mut accumulated = String::new();
        const MAX_ACCUMULATED: usize = 8192;
        let mut log_buffer = String::new();
        let mut last_log_flush = tokio::time::Instant::now();
        let log_interval = tokio::time::Duration::from_millis(1500);

        while let Some(text) = rx.recv().await {
            // Emit to dashboard
            let _ = io_emit.to("dashboard").emit(
                "dashboard:event",
                serde_json::json!({
                    "event": "debug:stream",
                    "data": {
                        "request_id": req_id_emit,
                        "task_id": task_id_emit,
                        "delta": text,
                        "chunk_index": chunk_index,
                    }
                }),
            );

            // Emit task:output to the task room
            if has_task {
                let task_room = format!(
                    "{}{}",
                    evo_common::messages::events::ROOM_TASK_PREFIX,
                    task_id_emit,
                );
                let _ = io_emit.to(task_room).emit(
                    evo_common::messages::events::TASK_OUTPUT,
                    serde_json::json!({
                        "task_id": task_id_emit,
                        "request_id": req_id_emit,
                        "delta": text,
                        "chunk_index": chunk_index,
                    }),
                );
            }

            // Accumulate output for evaluation (up to MAX_ACCUMULATED)
            if accumulated.len() < MAX_ACCUMULATED {
                let remaining = MAX_ACCUMULATED - accumulated.len();
                if text.len() <= remaining {
                    accumulated.push_str(&text);
                } else {
                    accumulated.push_str(&text[..remaining]);
                }
            }

            // Batch task logs every 1.5s
            if has_task {
                log_buffer.push_str(&text);
                if last_log_flush.elapsed() >= log_interval {
                    let _ = task_db::create_task_log(
                        &db_emit,
                        &task_id_emit,
                        "output",
                        "bash output chunk",
                        &log_buffer,
                        "",
                        "running",
                    )
                    .await;
                    log_buffer.clear();
                    last_log_flush = tokio::time::Instant::now();
                }
            }

            chunk_index += 1;
        }

        // Flush remaining log buffer
        if has_task && !log_buffer.is_empty() {
            let _ = task_db::create_task_log(
                &db_emit,
                &task_id_emit,
                "output",
                "bash output chunk",
                &log_buffer,
                "",
                "running",
            )
            .await;
        }

        accumulated
    });

    // Blocking PTY runner — tx is moved in; dropping it closes the channel
    let exit_code: i32 = tokio::task::spawn_blocking(move || pty_run_blocking(command, tx))
        .await
        .unwrap_or(1);

    // Channel is now closed (tx dropped); wait for emitter to drain
    let accumulated = emit_handle.await.unwrap_or_default();

    let elapsed = start.elapsed().as_millis() as u64;

    // Emit final debug:response to dashboard
    let _ = io.to("dashboard").emit(
        "dashboard:event",
        serde_json::json!({
            "event": "debug:response",
            "data": {
                "request_id": request_id,
                "task_id": task_id,
                "response": format!("\n[exit {}]", exit_code),
                "latency_ms": elapsed,
                "entry_type": "bash",
            }
        }),
    );

    // Update task status and emit task:evaluate
    if has_task {
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        let _ = task_db::update_task(&db, &task_id, Some(status), None, None, None, None).await;

        let _ = task_db::create_task_log(
            &db,
            &task_id,
            "info",
            &format!("bash command finished with exit code {exit_code}"),
            &accumulated,
            "",
            status,
        )
        .await;

        let eval_payload = serde_json::json!({
            "task_id": task_id,
            "request_id": request_id,
            "exit_code": exit_code,
            "latency_ms": elapsed,
            "output_preview": accumulated,
        });
        let task_room = format!(
            "{}{}",
            evo_common::messages::events::ROOM_TASK_PREFIX,
            task_id,
        );
        let _ = io
            .to("role:evaluation")
            .emit(evo_common::messages::events::TASK_EVALUATE, &eval_payload);
        let _ = io
            .to(task_room)
            .emit(evo_common::messages::events::TASK_EVALUATE, &eval_payload);

        // Notify dashboard of task completion
        let _ = io.to("dashboard").emit(
            evo_common::messages::events::TASK_CHANGED,
            serde_json::json!({ "action": "updated", "task": { "id": task_id, "status": status } }),
        );
    }
}

/// Synchronous PTY subprocess runner (designed for `tokio::task::spawn_blocking`).
///
/// Spawns `bash -c <command>` in a pseudo-terminal so that subprocess TUIs
/// (e.g. Codex CLI, htop) emit their full terminal output.  Streams raw PTY
/// bytes (including ANSI escape sequences) via `tx`; the caller strips ANSI
/// for display or renders with a terminal widget.
///
/// Returns the child process exit code.
fn pty_run_blocking(command: String, tx: tokio::sync::mpsc::UnboundedSender<String>) -> i32 {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::Read;

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 50,
        cols: 220,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(format!("[PTY error: {e}]\n"));
            return 1;
        }
    };

    let mut cmd = CommandBuilder::new("bash");
    cmd.arg("-c");
    cmd.arg(&command);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLUMNS", "220");
    cmd.env("LINES", "50");

    // Spawn child on slave PTY
    let child_result = pair.slave.spawn_command(cmd);
    // Drop slave so we get EOF/EIO when child exits
    drop(pair.slave);

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(format!("[spawn error: {e}]\n"));
            return 1;
        }
    };

    // Clone reader from master PTY
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(format!("[reader error: {e}]\n"));
            return 1;
        }
    };

    // Stream PTY output to the async emitter
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if tx.send(text).is_err() {
                    break; // dashboard disconnected
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO on macOS when child exits, or other terminal errors
        }
    }

    // pair.master drops here, closing the master fd
    drop(pair.master);

    child
        .wait()
        .map(|s| if s.success() { 0i32 } else { 1i32 })
        .unwrap_or(1)
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

// ─── Memory HTTP handlers ────────────────────────────────────────────────────

/// Query parameters for GET /memories.
#[derive(Deserialize)]
struct MemoriesQuery {
    scope: Option<String>,
    category: Option<String>,
    agent_id: Option<String>,
    run_id: Option<String>,
    skill_id: Option<String>,
    tag: Option<String>,
    limit: Option<u32>,
}

fn memory_row_to_json(row: &memory_db::MemoryRow) -> serde_json::Value {
    let metadata = serde_json::from_str::<serde_json::Value>(&row.metadata).unwrap_or(json!({}));
    let tags: Vec<&str> = if row.tags.is_empty() {
        vec![]
    } else {
        row.tags.split(',').map(|t| t.trim()).collect()
    };
    json!({
        "id":              row.id,
        "scope":           row.scope,
        "category":        row.category,
        "key":             row.key,
        "metadata":        metadata,
        "tags":            tags,
        "agent_id":        row.agent_id,
        "run_id":          row.run_id,
        "skill_id":        row.skill_id,
        "relevance_score": row.relevance_score,
        "access_count":    row.access_count,
        "created_at":      row.created_at,
        "updated_at":      row.updated_at,
    })
}

fn tier_row_to_json(row: &memory_db::MemoryTierRow) -> serde_json::Value {
    json!({
        "id":         row.id,
        "memory_id":  row.memory_id,
        "tier":       row.tier,
        "content":    row.content,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

/// GET /memories — list memories with optional filters.
async fn memories_list_handler(
    State(state): State<Arc<KingState>>,
    Query(params): Query<MemoriesQuery>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(50).min(500);
    match memory_db::list_memories(
        &state.db,
        limit,
        params.scope.as_deref(),
        params.category.as_deref(),
        params.agent_id.as_deref(),
        params.run_id.as_deref(),
        params.skill_id.as_deref(),
        params.tag.as_deref(),
    )
    .await
    {
        Ok(rows) => {
            let memories: Vec<serde_json::Value> = rows.iter().map(memory_row_to_json).collect();
            let count = memories.len();
            Json(json!({ "memories": memories, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// POST /memories — create a memory with optional tiers.
async fn memory_create_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let scope = body["scope"].as_str().unwrap_or("system");
    let category = body["category"].as_str().unwrap_or("general");
    let key = body["key"].as_str().unwrap_or("");
    let metadata = body
        .get("metadata")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());
    let tags_val = body.get("tags");
    let tags = match tags_val {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    let agent_id = body["agent_id"].as_str().unwrap_or("");
    let run_id = body["run_id"].as_str().unwrap_or("");
    let skill_id = body["skill_id"].as_str().unwrap_or("");
    let relevance_score = body["relevance_score"].as_f64().unwrap_or(0.0);

    match memory_db::create_memory(
        &state.db,
        scope,
        category,
        key,
        &metadata,
        &tags,
        agent_id,
        run_id,
        skill_id,
        relevance_score,
    )
    .await
    {
        Ok(row) => {
            // Upsert tiers if provided
            if let Some(serde_json::Value::Array(tiers)) = body.get("tiers") {
                for tier_entry in tiers {
                    let tier = tier_entry["tier"].as_str().unwrap_or("l0");
                    let content = tier_entry["content"].as_str().unwrap_or("");
                    if let Err(e) = memory_db::upsert_tier(&state.db, &row.id, tier, content).await
                    {
                        tracing::warn!(err = %e, tier = %tier, "failed to create tier");
                    }
                }
            }
            // Bind to task if task_id provided
            if let Some(task_id) = body["task_id"].as_str().filter(|s| !s.is_empty()) {
                let _ = memory_db::bind_task_memory(&state.db, task_id, &row.id).await;
            }
            // Broadcast memory:changed
            let _ = state.io.to("kernel").emit(
                evo_common::messages::events::MEMORY_CHANGED,
                json!({ "action": "created", "memory_id": row.id }),
            );
            let _ = state.io.to("dashboard").emit(
                "dashboard:event",
                json!({ "event": "memory:changed", "data": { "action": "created", "memory_id": row.id } }),
            );
            Json(json!({ "success": true, "memory": memory_row_to_json(&row) }))
        }
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// POST /memories/search — text search across memory tiers.
async fn memory_search_handler(
    State(state): State<Arc<KingState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let query = body["query"].as_str().unwrap_or("");
    let limit = body["limit"].as_u64().unwrap_or(20) as u32;
    let scope = body["scope"].as_str();
    let category = body["category"].as_str();
    let agent_id = body["agent_id"].as_str();
    let tier = body["tier"].as_str();

    match memory_db::search_memories(&state.db, query, limit, scope, category, agent_id, tier).await
    {
        Ok(rows) => {
            let memories: Vec<serde_json::Value> = rows.iter().map(memory_row_to_json).collect();
            let count = memories.len();
            Json(json!({ "memories": memories, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /memories/stats — count memories by scope.
async fn memory_stats_handler(State(state): State<Arc<KingState>>) -> Json<serde_json::Value> {
    match memory_db::count_memories_by_scope(&state.db).await {
        Ok(counts) => {
            let stats: serde_json::Map<String, serde_json::Value> = counts
                .into_iter()
                .map(|(scope, count)| (scope, json!(count)))
                .collect();
            Json(json!({ "stats": stats }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /memories/:memory_id — get a single memory with all tiers.
async fn memory_get_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    match memory_db::get_memory(&state.db, &memory_id).await {
        Ok(Some(row)) => {
            let mut mem = memory_row_to_json(&row);
            if let Ok(tiers) = memory_db::get_tiers(&state.db, &memory_id).await {
                mem["tiers"] = json!(tiers.iter().map(tier_row_to_json).collect::<Vec<_>>());
            }
            if let Ok(task_ids) = memory_db::list_tasks_for_memory(&state.db, &memory_id).await {
                mem["task_ids"] = json!(task_ids);
            }
            Json(json!({ "memory": mem }))
        }
        Ok(None) => Json(json!({ "error": "not found" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// PUT /memories/:memory_id — update a memory (partial).
async fn memory_update_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let metadata = body.get("metadata").map(|v| v.to_string());
    let tags = body.get("tags").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    });
    let relevance_score = body["relevance_score"].as_f64();

    match memory_db::update_memory(
        &state.db,
        &memory_id,
        metadata.as_deref(),
        tags.as_deref(),
        relevance_score,
    )
    .await
    {
        Ok(Some(row)) => {
            // Upsert tiers if provided
            if let Some(serde_json::Value::Array(tiers)) = body.get("tiers") {
                for tier_entry in tiers {
                    let tier = tier_entry["tier"].as_str().unwrap_or("l0");
                    let content = tier_entry["content"].as_str().unwrap_or("");
                    let _ = memory_db::upsert_tier(&state.db, &memory_id, tier, content).await;
                }
            }
            let _ = state.io.to("kernel").emit(
                evo_common::messages::events::MEMORY_CHANGED,
                json!({ "action": "updated", "memory_id": memory_id }),
            );
            let _ = state.io.to("dashboard").emit(
                "dashboard:event",
                json!({ "event": "memory:changed", "data": { "action": "updated", "memory_id": memory_id } }),
            );
            Json(json!({ "success": true, "memory": memory_row_to_json(&row) }))
        }
        Ok(None) => Json(json!({ "success": false, "error": "not found" })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// DELETE /memories/:memory_id — delete a memory and its tiers/bindings.
async fn memory_delete_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    match memory_db::delete_memory(&state.db, &memory_id).await {
        Ok(true) => {
            let _ = state.io.to("kernel").emit(
                evo_common::messages::events::MEMORY_CHANGED,
                json!({ "action": "deleted", "memory_id": memory_id }),
            );
            let _ = state.io.to("dashboard").emit(
                "dashboard:event",
                json!({ "event": "memory:changed", "data": { "action": "deleted", "memory_id": memory_id } }),
            );
            Json(json!({ "success": true }))
        }
        Ok(false) => Json(json!({ "success": false, "error": "not found" })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// GET /memories/:memory_id/tiers — list all tiers for a memory.
async fn memory_tiers_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    match memory_db::get_tiers(&state.db, &memory_id).await {
        Ok(tiers) => {
            let list: Vec<serde_json::Value> = tiers.iter().map(tier_row_to_json).collect();
            Json(json!({ "tiers": list, "count": list.len() }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// POST /memories/:memory_id/tiers — upsert a tier for a memory.
async fn memory_tier_upsert_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let tier = match body["tier"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return Json(json!({ "success": false, "error": "tier is required (l0, l1, or l2)" })),
    };
    let content = body["content"].as_str().unwrap_or("");

    match memory_db::upsert_tier(&state.db, &memory_id, tier, content).await {
        Ok(row) => Json(json!({ "success": true, "tier": tier_row_to_json(&row) })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// GET /memories/:memory_id/tiers/:tier — get a specific tier.
async fn memory_tier_get_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path((memory_id, tier)): axum::extract::Path<(String, String)>,
) -> Json<serde_json::Value> {
    match memory_db::get_tier(&state.db, &memory_id, &tier).await {
        Ok(Some(row)) => Json(json!({ "tier": tier_row_to_json(&row) })),
        Ok(None) => Json(json!({ "error": "not found" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// DELETE /memories/:memory_id/tiers/:tier — delete a specific tier.
async fn memory_tier_delete_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path((memory_id, tier)): axum::extract::Path<(String, String)>,
) -> Json<serde_json::Value> {
    match memory_db::delete_tier(&state.db, &memory_id, &tier).await {
        Ok(true) => Json(json!({ "success": true })),
        Ok(false) => Json(json!({ "success": false, "error": "not found" })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}

/// GET /memories/:memory_id/tasks — list tasks bound to a memory.
async fn memory_tasks_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(memory_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    match memory_db::list_tasks_for_memory(&state.db, &memory_id).await {
        Ok(task_ids) => {
            let count = task_ids.len();
            Json(json!({ "task_ids": task_ids, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /task/:task_id/memories — list memories bound to a task.
async fn task_memories_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let limit = 100u32;
    match memory_db::list_memories_for_task(&state.db, &task_id, limit).await {
        Ok(rows) => {
            let memories: Vec<serde_json::Value> = rows.iter().map(memory_row_to_json).collect();
            let count = memories.len();
            Json(json!({ "memories": memories, "count": count }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// POST /task/:task_id/memories — bind a memory to a task.
async fn task_memory_bind_handler(
    State(state): State<Arc<KingState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let memory_id = match body["memory_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => return Json(json!({ "success": false, "error": "memory_id is required" })),
    };

    match memory_db::bind_task_memory(&state.db, &task_id, memory_id).await {
        Ok(_) => Json(json!({ "success": true })),
        Err(e) => Json(json!({ "success": false, "error": e.to_string() })),
    }
}
