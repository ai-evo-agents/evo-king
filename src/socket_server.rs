use crate::{state::KingState, task_db};
use evo_common::messages::events;
use socketioxide::extract::{Data, SocketRef};
use socketioxide::SocketIo;
use std::sync::Arc;
use tracing::{info, warn};

// ─── Handler registration ─────────────────────────────────────────────────────

/// Register all Socket.IO namespace and event handlers against `io`.
///
/// Called once at startup. Handlers are per-socket closures that capture a
/// clone of `state` for async DB / broadcast operations.
pub fn register_handlers(io: SocketIo, state: Arc<KingState>) {
    io.ns("/", move |socket: SocketRef| {
        info!(sid = %socket.id, "runner connected");

        // Clone arcs for each handler so every closure is independent
        let s_register = Arc::clone(&state);
        let s_status   = Arc::clone(&state);
        let s_report   = Arc::clone(&state);
        let s_health   = Arc::clone(&state);

        // agent:register — runner announces itself + its role
        socket.on(
            events::AGENT_REGISTER,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_register);
                async move { on_register(data, state).await }
            },
        );

        // agent:status — periodic heartbeat from runner
        socket.on(
            events::AGENT_STATUS,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_status);
                async move { on_status(data, state).await }
            },
        );

        // agent:skill_report — runner reports on a discovered/built skill
        socket.on(
            events::AGENT_SKILL_REPORT,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_report);
                async move { on_skill_report(data, state).await }
            },
        );

        // agent:health — runner reports API health check results
        socket.on(
            events::AGENT_HEALTH,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_health);
                async move { on_health(data, state).await }
            },
        );

        socket.on_disconnect(|s: SocketRef, _reason: socketioxide::socket::DisconnectReason| {
            info!(sid = %s.id, "runner disconnected");
        });
    });
}

// ─── Event handlers ───────────────────────────────────────────────────────────

async fn on_register(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let role     = data["role"].as_str().unwrap_or("unknown");

    info!(agent_id = %agent_id, role = %role, "agent registered");

    if let Err(e) = task_db::upsert_agent(&state.db, agent_id, role, "online").await {
        warn!(err = %e, "failed to record agent registration in DB");
    }
}

async fn on_status(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let status   = data["status"].as_str().unwrap_or("heartbeat");

    if let Err(e) = task_db::upsert_agent(&state.db, agent_id, "", status).await {
        warn!(err = %e, "failed to update agent heartbeat in DB");
    }
}

async fn on_skill_report(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let skill_id = data["skill_id"].as_str().unwrap_or("unknown");

    info!(agent_id = %agent_id, skill_id = %skill_id, "skill report received");

    if let Err(e) = task_db::create_task(
        &state.db,
        "skill_report",
        Some(agent_id),
        &data.to_string(),
    )
    .await
    {
        warn!(err = %e, "failed to persist skill report task");
    }
}

async fn on_health(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    info!(agent_id = %agent_id, "agent health report received");

    if let Err(e) = task_db::create_task(
        &state.db,
        "health_report",
        Some(agent_id),
        &data.to_string(),
    )
    .await
    {
        warn!(err = %e, "failed to persist health report task");
    }
}
