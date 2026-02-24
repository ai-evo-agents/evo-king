mod agent_manager;
mod config_watcher;
mod cron_manager;
mod error;
mod gateway_manager;
mod pipeline_coordinator;
mod socket_server;
mod state;
mod task_db;

use agent_manager::AgentRegistry;
use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::{get, post}};
use evo_common::logging::init_logging;
use pipeline_coordinator::PipelineCoordinator;
use serde_json::json;
use socketioxide::SocketIo;
use state::KingState;
use std::sync::Arc;
use tracing::info;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_DB_PATH: &str = "king.db";
const DEFAULT_GATEWAY_CONFIG: &str = "../evo-gateway/gateway.json";
const DEFAULT_KERNEL_AGENTS_DIR: &str = "..";
const DEFAULT_RUNNER_BINARY: &str = "../evo-agents/target/release/evo-runner";

#[tokio::main]
async fn main() -> Result<()> {
    // Structured JSON logging to logs/king.log (+ stdout)
    let _log_guard = init_logging("king");

    let port: u16 = std::env::var("KING_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let db_path = std::env::var("KING_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());

    let gateway_config_path =
        std::env::var("GATEWAY_CONFIG_PATH").unwrap_or_else(|_| DEFAULT_GATEWAY_CONFIG.to_string());

    let kernel_agents_dir =
        std::env::var("KERNEL_AGENTS_DIR").unwrap_or_else(|_| DEFAULT_KERNEL_AGENTS_DIR.to_string());

    let runner_binary =
        std::env::var("RUNNER_BINARY").unwrap_or_else(|_| DEFAULT_RUNNER_BINARY.to_string());

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

    let pipeline_coordinator = Arc::new(PipelineCoordinator::new(
        Arc::clone(&db_arc),
        io.clone(),
    ));

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
        // ── Admin endpoints ──────────────────────────────────────────────────
        .route("/admin/config-sync", post(admin_config_sync_handler))
        .route("/admin/crons", get(admin_list_crons_handler))
        .route("/admin/crons/{name}/run", post(admin_run_cron_handler))
        .layer(socket_layer)
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
            let spawned = agent_manager::spawn_all_kernel_agents(
                &registry,
                &root,
                &binary,
                &king_address,
            )
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
            let runs: Vec<serde_json::Value> = rows
                .iter()
                .map(pipeline_row_to_json)
                .collect();
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
            let list: Vec<serde_json::Value> = stages
                .iter()
                .map(pipeline_row_to_json)
                .collect();
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
async fn admin_config_sync_handler(
    State(state): State<Arc<KingState>>,
) -> Json<serde_json::Value> {
    let payload = json!({
        "trigger": "config_sync",
        "reason": "post_dependency_update",
    });

    match state.io.emit(
        evo_common::messages::events::KING_CONFIG_UPDATE,
        &payload,
    ) {
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
async fn admin_list_crons_handler(
    State(state): State<Arc<KingState>>,
) -> Json<serde_json::Value> {
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
