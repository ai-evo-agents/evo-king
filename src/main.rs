mod agent_manager;
mod config_watcher;
mod error;
mod gateway_manager;
mod socket_server;
mod state;
mod task_db;

use anyhow::{Context, Result};
use axum::{routing::get, Json, Router};
use evo_common::logging::init_logging;
use serde_json::json;
use socketioxide::SocketIo;
use state::KingState;
use std::sync::Arc;
use tracing::info;

const DEFAULT_PORT: u16 = 3000;
const DEFAULT_DB_PATH: &str = "king.db";
const DEFAULT_GATEWAY_CONFIG: &str = "../evo-gateway/gateway.json";

#[tokio::main]
async fn main() -> Result<()> {
    // Structured JSON logging to logs/king.log (+ stdout)
    let _log_guard = init_logging("king");

    let port: u16 = std::env::var("KING_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let db_path = std::env::var("KING_DB_PATH")
        .unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());

    let gateway_config_path = std::env::var("GATEWAY_CONFIG_PATH")
        .unwrap_or_else(|_| DEFAULT_GATEWAY_CONFIG.to_string());

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

    let state = Arc::new(KingState {
        db: Arc::new(db),
        io: io.clone(),
        http_client,
        gateway_config_path: gateway_config_path.clone(),
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
        .layer(socket_layer);

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "evo-king listening");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;

    axum::serve(listener, app)
        .await
        .context("Server error")?;

    Ok(())
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "evo-king" }))
}
