use crate::{state::KingState, task_db};
use evo_common::messages::{AgentRole, PipelineStage, events};
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

        // Pipeline stage result handler arc
        let s_stage_result = Arc::clone(&state);

        // Debug handler arc
        let s_debug_response = Arc::clone(&state);

        // Task management handler arcs
        let s_task_create = Arc::clone(&state);
        let s_task_update = Arc::clone(&state);
        let s_task_get = Arc::clone(&state);
        let s_task_list = Arc::clone(&state);
        let s_task_delete = Arc::clone(&state);

        // dashboard:subscribe — dashboard clients join a read-only room
        socket.on(
            "dashboard:subscribe",
            move |s: SocketRef, _data: Data<serde_json::Value>| {
                let _ = s.join("dashboard");
                info!(sid = %s.id, "dashboard client subscribed");
            },
        );

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

        // pipeline:stage_result — agent reports pipeline stage completion
        socket.on(
            events::PIPELINE_STAGE_RESULT,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_stage_result);
                async move { on_pipeline_stage_result(data, state).await }
            },
        );

        // ── Debug events ─────────────────────────────────────────────────────────

        // debug:response — agent returns LLM response, relay to dashboard
        socket.on(
            events::DEBUG_RESPONSE,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_debug_response);
                async move {
                    let request_id = data["request_id"].as_str().unwrap_or("unknown");
                    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
                    info!(
                        request_id = %request_id,
                        agent_id = %agent_id,
                        "debug:response received, relaying to dashboard"
                    );
                    let _ = state.io.to("dashboard").emit(
                        "dashboard:event",
                        serde_json::json!({ "event": "debug:response", "data": data }),
                    );
                }
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

    // Extract capabilities and skills arrays as JSON strings for DB storage
    let capabilities = data
        .get("capabilities")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "[]".to_string());

    let skills = data
        .get("skills")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "[]".to_string());

    info!(
        agent_id     = %agent_id,
        role         = %role_str,
        capabilities = %capabilities,
        skills       = %skills,
        "agent registered"
    );

    // Determine kernel membership via the typed AgentRole enum.
    // All non-User roles are kernel roles.
    let is_kernel = match serde_json::from_value::<AgentRole>(data["role"].clone()) {
        Ok(AgentRole::User(_)) => false,
        Ok(_) => true,
        Err(_) => false,
    };

    if is_kernel {
        let _ = socket.join(events::ROOM_KERNEL);

        // Also join role-specific room for targeted pipeline:next dispatch
        let role_room = format!("{}{}", events::ROOM_ROLE_PREFIX, role_str.replace('-', "_"));
        let _ = socket.join(role_room.clone());
        info!(
            role = %role_str,
            sid = %socket.id,
            rooms = %format!("{}, {}", events::ROOM_KERNEL, role_room),
            "joined kernel + role rooms"
        );
    }

    // Look up PID from registry if this agent was spawned by king
    let pid = state
        .agent_registry
        .pid_by_folder_hint(role_str)
        .await
        .unwrap_or(0);

    if let Err(e) = task_db::upsert_agent(
        &state.db,
        agent_id,
        role_str,
        "online",
        Some(&capabilities),
        Some(&skills),
        Some(pid),
    )
    .await
    {
        warn!(err = %e, "failed to record agent registration in DB");
    }

    // Notify dashboard clients
    let _ = state.io.to("dashboard").emit(
        "dashboard:event",
        serde_json::json!({ "event": "agent:register", "data": data }),
    );
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

    if let Err(e) = task_db::upsert_agent(&state.db, agent_id, "", status, None, None, None).await {
        warn!(err = %e, "failed to update agent heartbeat in DB");
    }

    // Notify dashboard clients
    let _ = state.io.to("dashboard").emit(
        "dashboard:event",
        serde_json::json!({ "event": "agent:status", "data": data }),
    );
}

async fn on_skill_report(data: serde_json::Value, state: Arc<KingState>) {
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let skill_id = data["skill_id"].as_str().unwrap_or("unknown");

    info!(agent_id = %agent_id, skill_id = %skill_id, "skill report received");

    if let Err(e) = task_db::create_task(
        &state.db,
        "skill_report",
        "completed",
        Some(agent_id),
        &data.to_string(),
        "",
        "",
        "",
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
        "completed",
        Some(agent_id),
        &data.to_string(),
        "",
        "",
        "",
    )
    .await
    {
        warn!(err = %e, "failed to persist health report task");
    }
}

// ─── Pipeline event handlers ────────────────────────────────────────────────

async fn on_pipeline_stage_result(data: serde_json::Value, state: Arc<KingState>) {
    let run_id = match data["run_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            warn!("pipeline:stage_result received without run_id, ignoring");
            return;
        }
    };

    let stage_str = match data["stage"].as_str() {
        Some(s) => s,
        None => {
            warn!(run_id = %run_id, "pipeline:stage_result missing stage");
            return;
        }
    };

    let stage: PipelineStage = match serde_json::from_value(data["stage"].clone()) {
        Ok(s) => s,
        Err(e) => {
            warn!(run_id = %run_id, stage = %stage_str, err = %e, "invalid pipeline stage");
            return;
        }
    };

    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let status = data["status"].as_str().unwrap_or("completed");
    let artifact_id = data["artifact_id"].as_str().unwrap_or("");
    let output = data
        .get("output")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let error_msg = data["error"].as_str();

    info!(
        run_id = %run_id,
        stage = %stage_str,
        agent_id = %agent_id,
        status = %status,
        "pipeline stage result received"
    );

    if let Err(e) = state
        .pipeline_coordinator
        .on_stage_result(
            run_id,
            &stage,
            agent_id,
            status,
            artifact_id,
            &output,
            error_msg,
        )
        .await
    {
        warn!(err = %e, run_id = %run_id, "failed to process pipeline stage result");
    }

    // Notify dashboard clients
    let _ = state.io.to("dashboard").emit(
        "dashboard:event",
        serde_json::json!({ "event": "pipeline:stage_result", "data": data }),
    );
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
        "id":            row.id,
        "task_type":     row.task_type,
        "status":        row.status,
        "agent_id":      row.agent_id,
        "run_id":        row.run_id,
        "current_stage": row.current_stage,
        "summary":       row.summary,
        "payload":       payload,
        "created_at":    row.created_at,
        "updated_at":    row.updated_at,
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

    match task_db::create_task(
        &state.db, task_type, "pending", agent_id, &payload, "", "", "",
    )
    .await
    {
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
            let _ = socket
                .to("dashboard")
                .emit(events::TASK_CHANGED, &broadcast);
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
    let current_stage = data["current_stage"].as_str();
    let summary = data["summary"].as_str();

    match task_db::update_task(
        &state.db,
        task_id,
        status,
        agent_id,
        payload.as_deref(),
        current_stage,
        summary,
    )
    .await
    {
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
            let _ = socket
                .to("dashboard")
                .emit(events::TASK_CHANGED, &broadcast);
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
            let _ = socket
                .to("dashboard")
                .emit(events::TASK_CHANGED, &broadcast);
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
