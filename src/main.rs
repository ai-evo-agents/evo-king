mod agent_manager;
mod config_watcher;
mod error;
mod gateway_manager;
mod socket_server;
mod state;
mod task_db;

use agent_manager::AgentRegistry;
use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::get};
use evo_common::logging::init_logging;
use serde_json::json;
use socketioxide::SocketIo;
use state::KingState;
use std::sync::Arc;
use tracing::info;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_DB_PATH: &str = "king.db";
const DEFAULT_GATEWAY_CONFIG: &str = "../evo-gateway/gateway.json";
const DEFAULT_AGENTS_ROOT: &str = "../evo-agents";
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

    let agents_root =
        std::env::var("AGENTS_ROOT").unwrap_or_else(|_| DEFAULT_AGENTS_ROOT.to_string());

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

    let state = Arc::new(KingState {
        db: Arc::new(db),
        io: io.clone(),
        http_client,
        gateway_config_path: gateway_config_path.clone(),
        agent_registry: Arc::clone(&agent_registry),
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

    // ── Axum server with Socket.IO layer ──────────────────────────────────────
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/agents", get(list_agents_handler))
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
        let root = agents_root.clone();
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

    // ── Process monitor (background task) ────────────────────────────────────
    {
        let monitor_registry = Arc::clone(&agent_registry);
        let monitor_db = Arc::clone(&state.db);
        tokio::spawn(agent_manager::monitor_processes(
            monitor_registry,
            monitor_db,
        ));
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
