use crate::{state::KingState, task_db};
use evo_common::messages::{AgentRole, events};
use socketioxide::SocketIo;
use socketioxide::extract::{AckSender, Data, SocketRef};
use std::sync::Arc;
use tracing::{info, warn};

const MAX_TASK_LIMIT: u64 = 500;

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
        let s_status = Arc::clone(&state);
        let s_report = Arc::clone(&state);
        let s_health = Arc::clone(&state);

        // Task management handler arcs
        let s_task_create = Arc::clone(&state);
        let s_task_update = Arc::clone(&state);
        let s_task_get = Arc::clone(&state);
        let s_task_list = Arc::clone(&state);
        let s_task_delete = Arc::clone(&state);

        // agent:register — runner announces itself + its role
        socket.on(
            events::AGENT_REGISTER,
            move |s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_register);
                async move { on_register(s, data, state).await }
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

        // ── Task management events ─────────────────────────────────────────────

        // task:create — create a new task, ack the created task back
        socket.on(
            events::TASK_CREATE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_create);
                async move { on_task_create(s, data, ack, state).await }
            },
        );

        // task:update — update task status/assignment, ack the updated task
        socket.on(
            events::TASK_UPDATE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_update);
                async move { on_task_update(s, data, ack, state).await }
            },
        );

        // task:get — fetch a single task by ID
        socket.on(
            events::TASK_GET,
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_get);
                async move { on_task_get(data, ack, state).await }
            },
        );

        // task:list — list tasks with optional filters
        socket.on(
            events::TASK_LIST,
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_list);
                async move { on_task_list(data, ack, state).await }
            },
        );

        // task:delete — delete a task
        socket.on(
            events::TASK_DELETE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_delete);
                async move { on_task_delete(s, data, ack, state).await }
            },
        );

        socket.on_disconnect(
            |s: SocketRef, _reason: socketioxide::socket::DisconnectReason| {
                info!(sid = %s.id, "runner disconnected");
            },
        );
    });
}

// ─── Agent event handlers ────────────────────────────────────────────────────

async fn on_register(socket: SocketRef, data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = match data["agent_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!("agent:register received without agent_id, ignoring");
            return;
        }
    };

    let role_str = data["role"].as_str().unwrap_or("unknown");

    info!(agent_id = %agent_id, role = %role_str, "agent registered");

    // Determine kernel membership via the typed AgentRole enum.
    // All non-User roles are kernel roles.
    let is_kernel = match serde_json::from_value::<AgentRole>(data["role"].clone()) {
        Ok(AgentRole::User(_)) => false,
        Ok(_) => true,
        Err(_) => false,
    };

    if is_kernel {
        let _ = socket.join(events::ROOM_KERNEL);
        info!(role = %role_str, sid = %socket.id, "joined kernel room");
    }

    if let Err(e) = task_db::upsert_agent(&state.db, agent_id, role_str, "online").await {
        warn!(err = %e, "failed to record agent registration in DB");
    }
}

async fn on_status(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = match data["agent_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!("agent:status received without agent_id, ignoring");
            return;
        }
    };
    let status = data["status"].as_str().unwrap_or("heartbeat");

    if let Err(e) = task_db::upsert_agent(&state.db, agent_id, "", status).await {
        warn!(err = %e, "failed to update agent heartbeat in DB");
    }
}

async fn on_skill_report(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let skill_id = data["skill_id"].as_str().unwrap_or("unknown");

    info!(agent_id = %agent_id, skill_id = %skill_id, "skill report received");

    if let Err(e) =
        task_db::create_task(&state.db, "skill_report", Some(agent_id), &data.to_string()).await
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

// ─── Task event handlers ─────────────────────────────────────────────────────

fn task_row_to_json(row: &task_db::TaskRow) -> serde_json::Value {
    let payload = match serde_json::from_str::<serde_json::Value>(&row.payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(task_id = %row.id, err = %e, "corrupt payload JSON in database");
            serde_json::Value::Object(serde_json::Map::new())
        }
    };

    serde_json::json!({
        "id":         row.id,
        "task_type":  row.task_type,
        "status":     row.status,
        "agent_id":   row.agent_id,
        "payload":    payload,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

fn ack_error(ack: AckSender, msg: &str) {
    let _ = ack.send(&serde_json::json!({ "success": false, "error": msg }));
}

async fn on_task_create(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let task_type = match data["task_type"].as_str() {
        Some(t) if !t.is_empty() && t.len() <= 64 => t,
        _ => {
            ack_error(ack, "invalid or missing task_type");
            return;
        }
    };
    let agent_id = data["agent_id"].as_str();
    let payload = data
        .get("payload")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());

    match task_db::create_task(&state.db, task_type, agent_id, &payload).await {
        Ok(row) => {
            let response = task_row_to_json(&row);
            info!(task_id = %row.id, task_type = %task_type, "task created");

            if let Err(e) = ack.send(&serde_json::json!({ "success": true, "task": response })) {
                warn!(err = %e, "failed to send task:create ack");
            }

            let broadcast = serde_json::json!({ "action": "created", "task": response });
            if let Err(e) = socket
                .to(events::ROOM_KERNEL)
                .emit(events::TASK_CHANGED, &broadcast)
            {
                warn!(err = %e, "failed to broadcast task:changed");
            }
        }
        Err(e) => {
            warn!(err = %e, "failed to create task");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_task_update(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let task_id = match data["task_id"].as_str() {
        Some(id) => id,
        None => {
            ack_error(ack, "missing task_id");
            return;
        }
    };

    let status = data["status"].as_str();
    let agent_id = data["agent_id"].as_str();
    let payload = data.get("payload").map(|v| v.to_string());

    match task_db::update_task(&state.db, task_id, status, agent_id, payload.as_deref()).await {
        Ok(Some(row)) => {
            let response = task_row_to_json(&row);
            info!(task_id = %task_id, "task updated");

            if let Err(e) = ack.send(&serde_json::json!({ "success": true, "task": response })) {
                warn!(err = %e, "failed to send task:update ack");
            }

            let broadcast = serde_json::json!({ "action": "updated", "task": response });
            if let Err(e) = socket
                .to(events::ROOM_KERNEL)
                .emit(events::TASK_CHANGED, &broadcast)
            {
                warn!(err = %e, "failed to broadcast task:changed");
            }
        }
        Ok(None) => {
            ack_error(ack, &format!("task not found: {task_id}"));
        }
        Err(e) => {
            warn!(err = %e, "failed to update task");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_task_get(data: serde_json::Value, ack: AckSender, state: Arc<KingState>) {
    let task_id = match data["task_id"].as_str() {
        Some(id) => id,
        None => {
            ack_error(ack, "missing task_id");
            return;
        }
    };

    match task_db::get_task(&state.db, task_id).await {
        Ok(Some(row)) => {
            let response = task_row_to_json(&row);
            let _ = ack.send(&serde_json::json!({ "success": true, "task": response }));
        }
        Ok(None) => {
            ack_error(ack, &format!("task not found: {task_id}"));
        }
        Err(e) => {
            warn!(err = %e, "failed to get task");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_task_list(data: serde_json::Value, ack: AckSender, state: Arc<KingState>) {
    let limit = data["limit"].as_u64().unwrap_or(50).min(MAX_TASK_LIMIT) as u32;
    let status_filter = data["status"].as_str();
    let agent_filter = data["agent_id"].as_str();

    match task_db::list_tasks(&state.db, limit, status_filter, agent_filter).await {
        Ok(rows) => {
            let tasks: Vec<serde_json::Value> = rows.iter().map(task_row_to_json).collect();
            let count = tasks.len();
            let _ = ack.send(&serde_json::json!({
                "success": true,
                "tasks": tasks,
                "count": count,
            }));
        }
        Err(e) => {
            warn!(err = %e, "failed to list tasks");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_task_delete(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let task_id = match data["task_id"].as_str() {
        Some(id) => id,
        None => {
            ack_error(ack, "missing task_id");
            return;
        }
    };

    match task_db::delete_task(&state.db, task_id).await {
        Ok(true) => {
            info!(task_id = %task_id, "task deleted");
            let _ = ack.send(&serde_json::json!({ "success": true, "task_id": task_id }));

            let broadcast = serde_json::json!({ "action": "deleted", "task_id": task_id });
            if let Err(e) = socket
                .to(events::ROOM_KERNEL)
                .emit(events::TASK_CHANGED, &broadcast)
            {
                warn!(err = %e, "failed to broadcast task:changed");
            }
        }
        Ok(false) => {
            ack_error(ack, &format!("task not found: {task_id}"));
        }
        Err(e) => {
            warn!(err = %e, "failed to delete task");
            ack_error(ack, "internal server error");
        }
    }
}
