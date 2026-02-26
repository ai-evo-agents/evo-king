use crate::{memory_db, state::KingState, task_db};
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

        // Debug handler arcs
        let s_debug_response = Arc::clone(&state);
        let s_debug_stream = Arc::clone(&state);

        // Task management handler arcs
        let s_task_create = Arc::clone(&state);
        let s_task_update = Arc::clone(&state);
        let s_task_get = Arc::clone(&state);
        let s_task_list = Arc::clone(&state);
        let s_task_delete = Arc::clone(&state);

        // Memory handler arcs
        let s_memory_store = Arc::clone(&state);
        let s_memory_query = Arc::clone(&state);
        let s_memory_update = Arc::clone(&state);
        let s_memory_delete = Arc::clone(&state);

        // Task room handler arcs
        let s_task_join = Arc::clone(&state);
        let s_task_summary = Arc::clone(&state);

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

                    // If task_id is present, update task and emit task:evaluate
                    if let Some(task_id) = data["task_id"].as_str().filter(|s| !s.is_empty()) {
                        let _ = task_db::update_task(
                            &state.db, task_id,
                            Some("completed"), None, None, None, None,
                        ).await;

                        let _ = task_db::create_task_log(
                            &state.db, task_id, "info",
                            "debug:response completed",
                            data["response"].as_str().unwrap_or(""),
                            agent_id, "response",
                        ).await;

                        let eval_payload = serde_json::json!({
                            "task_id": task_id,
                            "request_id": request_id,
                            "agent_id": agent_id,
                        });
                        let task_room = format!("{}{}", events::ROOM_TASK_PREFIX, task_id);
                        let _ = state.io.to("role:evaluation").emit(events::TASK_EVALUATE, &eval_payload);
                        let _ = state.io.to(task_room).emit(events::TASK_EVALUATE, &eval_payload);
                    }
                }
            },
        );

        // debug:stream — agent sends streaming LLM token delta, relay to dashboard
        socket.on(
            events::DEBUG_STREAM,
            move |_s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_debug_stream);
                async move {
                    let _ = state.io.to("dashboard").emit(
                        "dashboard:event",
                        serde_json::json!({ "event": "debug:stream", "data": data }),
                    );

                    // If task_id is present, also emit task:output to the task room
                    if let Some(task_id) = data["task_id"].as_str().filter(|s| !s.is_empty()) {
                        let task_room = format!("{}{}", events::ROOM_TASK_PREFIX, task_id);
                        let _ = state.io.to(task_room).emit(events::TASK_OUTPUT, &data);
                    }
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

        // ── Memory events ────────────────────────────────────────────────────

        // memory:store — agent stores a learned memory
        socket.on(
            events::MEMORY_STORE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_memory_store);
                async move { on_memory_store(s, data, ack, state).await }
            },
        );

        // memory:query — agent queries for relevant memories
        socket.on(
            events::MEMORY_QUERY,
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_memory_query);
                async move { on_memory_query(data, ack, state).await }
            },
        );

        // memory:update — agent updates an existing memory
        socket.on(
            events::MEMORY_UPDATE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_memory_update);
                async move { on_memory_update(s, data, ack, state).await }
            },
        );

        // memory:delete — agent deletes a memory
        socket.on(
            events::MEMORY_DELETE,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_memory_delete);
                async move { on_memory_delete(s, data, ack, state).await }
            },
        );

        // ── Task room events ──────────────────────────────────────────────────

        // task:join — agent joins a task-specific room
        socket.on(
            events::TASK_JOIN,
            move |s: SocketRef, Data::<serde_json::Value>(data)| {
                let _state = Arc::clone(&s_task_join);
                let task_id = data["task_id"].as_str().unwrap_or("").to_string();
                let agent_id = data["agent_id"].as_str().unwrap_or("unknown").to_string();
                async move {
                    if task_id.is_empty() {
                        warn!("task:join without task_id");
                        return;
                    }
                    let room = format!("{}{}", events::ROOM_TASK_PREFIX, task_id);
                    let _ = s.join(room.clone());
                    info!(agent_id = %agent_id, task_id = %task_id, room = %room, "agent joined task room");
                }
            },
        );

        // task:summary — evaluation agent reports task summary
        socket.on(
            events::TASK_SUMMARY,
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_task_summary);
                async move { on_task_summary(s, data, ack, state).await }
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
        "parent_id":     row.parent_id,
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
    let parent_id = data["parent_id"].as_str().unwrap_or("");

    match task_db::create_task(
        &state.db, task_type, "pending", agent_id, &payload, "", "", "", parent_id,
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
    let parent_id_filter = data["parent_id"].as_str();

    match task_db::list_tasks(
        &state.db,
        limit,
        status_filter,
        agent_filter,
        parent_id_filter,
    )
    .await
    {
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

// ─── Memory event handlers ──────────────────────────────────────────────────

async fn on_memory_store(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let scope = data["scope"].as_str().unwrap_or("system");
    let category = data["category"].as_str().unwrap_or("general");
    let key = data["key"].as_str().unwrap_or("");
    let metadata = data
        .get("metadata")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "{}".to_string());
    let tags = match data.get("tags") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    let agent_id = data["agent_id"].as_str().unwrap_or("");
    let run_id = data["run_id"].as_str().unwrap_or("");
    let skill_id = data["skill_id"].as_str().unwrap_or("");
    let relevance_score = data["relevance_score"].as_f64().unwrap_or(0.0);

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
            if let Some(serde_json::Value::Array(tiers)) = data.get("tiers") {
                for tier_entry in tiers {
                    let tier = tier_entry["tier"].as_str().unwrap_or("l0");
                    let content = tier_entry["content"].as_str().unwrap_or("");
                    let _ = memory_db::upsert_tier(&state.db, &row.id, tier, content).await;
                }
            }
            // Bind to task if task_id provided
            if let Some(task_id) = data["task_id"].as_str().filter(|s| !s.is_empty()) {
                let _ = memory_db::bind_task_memory(&state.db, task_id, &row.id).await;
            }

            info!(memory_id = %row.id, scope = %scope, category = %category, "memory stored");
            let _ = ack.send(&serde_json::json!({ "success": true, "memory_id": row.id }));

            let broadcast = serde_json::json!({ "action": "created", "memory_id": row.id });
            let _ = socket
                .to(events::ROOM_KERNEL)
                .emit(events::MEMORY_CHANGED, &broadcast);
            let _ = socket.to("dashboard").emit(
                "dashboard:event",
                serde_json::json!({ "event": "memory:changed", "data": broadcast }),
            );
        }
        Err(e) => {
            warn!(err = %e, "failed to store memory");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_memory_query(data: serde_json::Value, ack: AckSender, state: Arc<KingState>) {
    let query = data["query"].as_str().unwrap_or("");
    let limit = data["limit"].as_u64().unwrap_or(20).min(MAX_TASK_LIMIT) as u32;
    let scope = data["scope"].as_str();
    let category = data["category"].as_str();
    let agent_id = data["agent_id"].as_str();
    let tier = data["tier"].as_str();

    match memory_db::search_memories(&state.db, query, limit, scope, category, agent_id, tier).await
    {
        Ok(rows) => {
            // Touch each result to increment access count
            for row in &rows {
                let _ = memory_db::touch_memory(&state.db, &row.id).await;
            }

            // Build response including tiers for each memory
            let mut memories = Vec::new();
            for row in &rows {
                let tiers = memory_db::get_tiers(&state.db, &row.id)
                    .await
                    .unwrap_or_default();
                let mut mem = memory_row_to_json(row);
                mem["tiers"] = tiers
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "tier": t.tier,
                            "content": t.content,
                            "created_at": t.created_at,
                            "updated_at": t.updated_at,
                        })
                    })
                    .collect::<Vec<_>>()
                    .into();
                memories.push(mem);
            }

            let count = memories.len();
            let _ = ack.send(&serde_json::json!({
                "success": true,
                "memories": memories,
                "count": count,
            }));
        }
        Err(e) => {
            warn!(err = %e, "failed to query memories");
            ack_error(ack, "internal server error");
        }
    }
}

fn memory_row_to_json(row: &memory_db::MemoryRow) -> serde_json::Value {
    let metadata = serde_json::from_str::<serde_json::Value>(&row.metadata)
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    let tags: Vec<&str> = if row.tags.is_empty() {
        vec![]
    } else {
        row.tags.split(',').collect()
    };

    serde_json::json!({
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

async fn on_memory_update(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let memory_id = match data["memory_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            ack_error(ack, "missing memory_id");
            return;
        }
    };

    let metadata = data.get("metadata").map(|v| v.to_string());
    let tags = match data.get("tags") {
        Some(serde_json::Value::Array(arr)) => Some(
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => None,
    };
    let relevance_score = data["relevance_score"].as_f64();

    match memory_db::update_memory(
        &state.db,
        memory_id,
        metadata.as_deref(),
        tags.as_deref(),
        relevance_score,
    )
    .await
    {
        Ok(Some(row)) => {
            // Upsert tiers if provided
            if let Some(serde_json::Value::Array(tiers)) = data.get("tiers") {
                for tier_entry in tiers {
                    let tier = tier_entry["tier"].as_str().unwrap_or("l0");
                    let content = tier_entry["content"].as_str().unwrap_or("");
                    let _ = memory_db::upsert_tier(&state.db, &row.id, tier, content).await;
                }
            }

            info!(memory_id = %memory_id, "memory updated");
            let _ = ack
                .send(&serde_json::json!({ "success": true, "memory": memory_row_to_json(&row) }));

            let broadcast = serde_json::json!({ "action": "updated", "memory_id": memory_id });
            let _ = socket
                .to(events::ROOM_KERNEL)
                .emit(events::MEMORY_CHANGED, &broadcast);
            let _ = socket.to("dashboard").emit(
                "dashboard:event",
                serde_json::json!({ "event": "memory:changed", "data": broadcast }),
            );
        }
        Ok(None) => {
            ack_error(ack, &format!("memory not found: {memory_id}"));
        }
        Err(e) => {
            warn!(err = %e, "failed to update memory");
            ack_error(ack, "internal server error");
        }
    }
}

async fn on_memory_delete(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let memory_id = match data["memory_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            ack_error(ack, "missing memory_id");
            return;
        }
    };

    match memory_db::delete_memory(&state.db, memory_id).await {
        Ok(true) => {
            info!(memory_id = %memory_id, "memory deleted");
            let _ = ack.send(&serde_json::json!({ "success": true, "memory_id": memory_id }));

            let broadcast = serde_json::json!({ "action": "deleted", "memory_id": memory_id });
            let _ = socket
                .to(events::ROOM_KERNEL)
                .emit(events::MEMORY_CHANGED, &broadcast);
            let _ = socket.to("dashboard").emit(
                "dashboard:event",
                serde_json::json!({ "event": "memory:changed", "data": broadcast }),
            );
        }
        Ok(false) => {
            ack_error(ack, &format!("memory not found: {memory_id}"));
        }
        Err(e) => {
            warn!(err = %e, "failed to delete memory");
            ack_error(ack, "internal server error");
        }
    }
}

// ─── Task summary handler ────────────────────────────────────────────────────

async fn on_task_summary(
    socket: SocketRef,
    data: serde_json::Value,
    ack: AckSender,
    state: Arc<KingState>,
) {
    let task_id = match data["task_id"].as_str() {
        Some(id) if !id.is_empty() => id,
        _ => {
            ack_error(ack, "missing task_id");
            return;
        }
    };
    let agent_id = data["agent_id"].as_str().unwrap_or("unknown");
    let summary = data["summary"].as_str().unwrap_or("");
    let score = data["score"].as_f64().unwrap_or(0.0);
    let tags = match data.get("tags") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };

    info!(
        task_id = %task_id,
        agent_id = %agent_id,
        score = %score,
        "task:summary received"
    );

    // 1. Update task summary in DB
    if let Err(e) = task_db::update_task(
        &state.db,
        task_id,
        Some("evaluated"),
        None,
        None,
        None,
        Some(summary),
    )
    .await
    {
        warn!(err = %e, "failed to update task summary");
        ack_error(ack, "failed to update task");
        return;
    }

    // 2. Create a task_log entry
    let _ = task_db::create_task_log(
        &state.db,
        task_id,
        "info",
        &format!("task evaluated: score={score}"),
        summary,
        agent_id,
        "evaluation",
    )
    .await;

    // 3. Create a memory with scope="system", category="case"
    let metadata = serde_json::json!({
        "task_id": task_id,
        "score": score,
    })
    .to_string();

    let memory_result = memory_db::create_memory(
        &state.db,
        "system",
        "case",
        &format!("task:{task_id}"),
        &metadata,
        &tags,
        agent_id,
        "", // run_id
        "", // skill_id
        score,
    )
    .await;

    if let Ok(mem) = &memory_result {
        // 4. Upsert L0 tier with summary text
        let _ = memory_db::upsert_tier(&state.db, &mem.id, "l0", summary).await;

        // 5. Bind memory to task
        let _ = memory_db::bind_task_memory(&state.db, task_id, &mem.id).await;
    }

    let _ = ack.send(&serde_json::json!({ "success": true, "task_id": task_id }));

    // 6. Broadcast task:changed to kernel + dashboard rooms
    if let Ok(Some(updated_task)) = task_db::get_task(&state.db, task_id).await {
        let task_json = task_row_to_json(&updated_task);
        let broadcast = serde_json::json!({ "action": "evaluated", "task": task_json });
        let _ = socket
            .to(events::ROOM_KERNEL)
            .emit(events::TASK_CHANGED, &broadcast);
        let _ = socket
            .to("dashboard")
            .emit(events::TASK_CHANGED, &broadcast);
        // Also send as dashboard:event so the debug page picks it up
        let dashboard_evt = serde_json::json!({
            "event": "task:changed",
            "data": { "action": "evaluated", "task": task_json },
        });
        let _ = socket
            .to("dashboard")
            .emit("dashboard:event", &dashboard_evt);
    }
}
