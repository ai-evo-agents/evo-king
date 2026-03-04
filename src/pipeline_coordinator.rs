use crate::{memory_db, task_db};
use anyhow::{Context, Result};
use evo_common::messages::{PipelineStage, events};
use libsql::Database;
use serde_json::Value;
use socketioxide::SocketIo;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Default stage timeout in seconds (5 minutes).
const DEFAULT_STAGE_TIMEOUT_SECS: i64 = 300;

/// Maximum retries for a failed pipeline stage.
const MAX_STAGE_RETRIES: u32 = 3;

/// Timeout in seconds for recovery/decompose requests.
#[allow(dead_code)]
const RECOVERY_TIMEOUT_SECS: u64 = 30;

/// Tracks an in-flight recovery or decompose request.
#[allow(dead_code)]
struct PendingRequest {
    run_id: String,
    task_id: String,
    request_type: String, // "recovery" or "decompose"
}

/// Coordinates the evolution pipeline across kernel agents.
///
/// Each pipeline run moves through 5 stages in order:
/// Learning → Building → PreLoad → Evaluation → SkillManage
///
/// The coordinator emits `pipeline:next` to role-specific Socket.IO rooms
/// and tracks state in the `pipeline_runs` DB table.
pub struct PipelineCoordinator {
    db: Arc<Database>,
    io: SocketIo,
    pending_requests: Mutex<HashMap<String, PendingRequest>>,
}

impl PipelineCoordinator {
    pub fn new(db: Arc<Database>, io: SocketIo) -> Self {
        Self {
            db,
            io,
            pending_requests: Mutex::new(HashMap::new()),
        }
    }

    /// Start a new pipeline run. Creates a DB record for the `learning` stage,
    /// creates a main task linked to this run, inserts the first task log,
    /// and emits `pipeline:next` to the `role:learning` room.
    pub async fn start_run(&self, trigger_metadata: Value) -> Result<String> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let first_stage = PipelineStage::Learning;
        let stage_str = stage_to_str(&first_stage);

        // Create the first stage record
        let metadata_str = serde_json::to_string(&trigger_metadata).unwrap_or_default();
        let _row = task_db::create_pipeline_stage(&self.db, &run_id, stage_str, "", &metadata_str)
            .await
            .context("create initial pipeline stage")?;

        info!(
            run_id = %run_id,
            stage = %stage_str,
            "pipeline run started"
        );

        // Create the main task linked to this pipeline run
        let summary = format!("{} — {}", stage_str, stage_summary(stage_str));
        let task_row = task_db::create_task(
            &self.db,
            "pipeline",
            "running",
            None,
            &serde_json::json!({ "trigger": trigger_metadata }).to_string(),
            &run_id,
            stage_str,
            &summary,
            "",
        )
        .await
        .context("create pipeline task")?;

        // Insert the first task log
        let log_row = task_db::create_task_log(
            &self.db,
            &task_row.id,
            "info",
            "Pipeline started",
            &serde_json::json!({ "trigger": trigger_metadata }).to_string(),
            "",
            stage_str,
        )
        .await;

        // Broadcast task:changed + task:log to dashboard
        self.broadcast_task_changed(&task_row).await;
        if let Ok(log) = &log_row {
            self.broadcast_task_log(&task_row.id, log).await;
        }

        // Emit task:invite to kernel room so all agents can join the task room
        let invite_payload = serde_json::json!({
            "task_id": task_row.id,
            "task_type": "pipeline",
            "payload": { "run_id": run_id, "stage": stage_str, "trigger": trigger_metadata },
        });
        if let Err(e) = self
            .io
            .to("kernel")
            .emit(events::TASK_INVITE, &invite_payload)
            .await
        {
            warn!(run_id = %run_id, err = %e, "failed to emit task:invite to kernel room");
        }

        // Emit pipeline:next to the role room for the first stage
        let payload = serde_json::json!({
            "run_id": run_id,
            "stage": stage_str,
            "artifact_id": "",
            "metadata": trigger_metadata,
        });

        let room = stage_to_room(&first_stage);
        if let Err(e) = self
            .io
            .to(room.clone())
            .emit(events::PIPELINE_NEXT, &payload)
            .await
        {
            error!(run_id = %run_id, room = %room, err = %e, "failed to emit pipeline:next");
        }
        // Notify dashboard clients
        let _ = self
            .io
            .to("dashboard")
            .emit(
                "dashboard:event",
                &serde_json::json!({ "event": "pipeline:next", "data": payload }),
            )
            .await;

        Ok(run_id)
    }

    /// Handle a stage result reported by an agent. Advances the pipeline to the
    /// next stage, or marks the run as completed/failed.
    #[allow(clippy::too_many_arguments)]
    pub async fn on_stage_result(
        &self,
        run_id: &str,
        stage: &PipelineStage,
        agent_id: &str,
        status: &str,
        artifact_id: &str,
        output: &Value,
        error_msg: Option<&str>,
    ) -> Result<()> {
        let stage_str = stage_to_str(stage);

        // Find the running stage record for this run_id + stage
        let stages = task_db::get_pipeline_run_stages(&self.db, run_id)
            .await
            .context("fetch pipeline stages")?;

        let current_stage = stages
            .iter()
            .find(|s| s.stage == stage_str && s.status == "running");

        let stage_row = match current_stage {
            Some(row) => row,
            None => {
                warn!(
                    run_id = %run_id,
                    stage = %stage_str,
                    "no running stage found for this run_id + stage, ignoring"
                );
                return Ok(());
            }
        };

        // Update the stage record
        task_db::update_pipeline_stage(
            &self.db,
            &stage_row.id,
            status,
            agent_id,
            Some(&output.to_string()),
            error_msg,
        )
        .await
        .context("update pipeline stage")?;

        info!(
            run_id = %run_id,
            stage = %stage_str,
            agent_id = %agent_id,
            status = %status,
            "pipeline stage result recorded"
        );

        // Look up the task linked to this run
        let task = task_db::get_task_by_run_id(&self.db, run_id)
            .await
            .ok()
            .flatten();

        // If failed, initiate error recovery
        if status == "failed" {
            let err_text = error_msg.unwrap_or("unknown error");
            warn!(
                run_id = %run_id,
                stage = %stage_str,
                error = %err_text,
                "pipeline stage failed — initiating error recovery"
            );

            // Extract memory for the failed stage
            let task_id_ref = task.as_ref().map(|t| t.id.as_str());
            self.extract_stage_memory(
                run_id,
                stage_str,
                agent_id,
                artifact_id,
                output,
                task_id_ref,
            )
            .await;

            // Log the failure
            if let Some(ref t) = task {
                let log = task_db::create_task_log(
                    &self.db,
                    &t.id,
                    "error",
                    &format!("Stage {} failed: {}", stage_str, err_text),
                    &output.to_string(),
                    agent_id,
                    stage_str,
                )
                .await;
                if let Ok(ref l) = log {
                    self.broadcast_task_log(&t.id, l).await;
                }
            }

            // Get current retry count
            let retry_count = task_db::get_stage_retry_count(&self.db, run_id, stage_str)
                .await
                .unwrap_or(0);

            // Set task status to "recovering"
            if let Some(ref t) = task {
                let _ = task_db::update_task(
                    &self.db,
                    &t.id,
                    Some("recovering"),
                    None,
                    None,
                    None,
                    Some(&format!("Error recovery in progress for {}", stage_str)),
                )
                .await;
                if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                    && let Some(ref ut) = updated
                {
                    self.broadcast_task_changed(ut).await;
                }
            }

            let request_id = uuid::Uuid::new_v4().to_string();
            let task_id_str = task.as_ref().map(|t| t.id.clone()).unwrap_or_default();
            let task_summary = task.as_ref().map(|t| t.summary.clone()).unwrap_or_default();

            {
                let mut pending = self.pending_requests.lock().await;
                pending.insert(
                    request_id.clone(),
                    PendingRequest {
                        run_id: run_id.to_string(),
                        task_id: task_id_str.clone(),
                        request_type: "recovery".to_string(),
                    },
                );
            }

            let recovery_payload = serde_json::json!({
                "request_id": request_id,
                "run_id": run_id,
                "task_id": task_id_str,
                "failed_stage": stage_str,
                "error_message": err_text,
                "stage_output": output,
                "retry_count": retry_count,
                "task_summary": task_summary,
            });

            if let Err(e) = self
                .io
                .to("role:evaluation")
                .emit(events::ERROR_RECOVERY_REQUEST, &recovery_payload)
                .await
            {
                error!(run_id = %run_id, err = %e, "failed to emit recovery request — aborting");
                self.pending_requests.lock().await.remove(&request_id);
                self.abort_pipeline(
                    run_id,
                    task.as_ref(),
                    stage_str,
                    "No recovery agent available",
                )
                .await;
            }

            return Ok(());
        }

        // If completed, advance to the next stage
        if status == "completed" {
            // Extract memory for the completed stage
            let task_id = task.as_ref().map(|t| t.id.as_str());
            self.extract_stage_memory(run_id, stage_str, agent_id, artifact_id, output, task_id)
                .await;

            // Log stage completion
            if let Some(ref t) = task {
                let log = task_db::create_task_log(
                    &self.db,
                    &t.id,
                    "info",
                    &format!("Stage {} completed", stage_str),
                    &output.to_string(),
                    agent_id,
                    stage_str,
                )
                .await;
                if let Ok(ref l) = log {
                    self.broadcast_task_log(&t.id, l).await;
                }
            }

            // Create subtasks from evaluation output if present
            if stage == &PipelineStage::Evaluation
                && let Some(subtasks_arr) = output.get("subtasks").and_then(|v| v.as_array())
                && !subtasks_arr.is_empty()
                && let Some(ref t) = task
            {
                self.create_subtasks(t, subtasks_arr, run_id, agent_id)
                    .await;
            }

            match next_stage(stage) {
                Some(next) => {
                    let next_str = stage_to_str(&next);

                    // Create the next stage record
                    let metadata_str = output.to_string();
                    task_db::create_pipeline_stage(
                        &self.db,
                        run_id,
                        next_str,
                        artifact_id,
                        &metadata_str,
                    )
                    .await
                    .context("create next pipeline stage")?;

                    // Update the task: advance stage + summary
                    if let Some(ref t) = task {
                        let next_summary = format!("{} — {}", next_str, stage_summary(next_str));
                        let _ = task_db::update_task(
                            &self.db,
                            &t.id,
                            None,
                            None,
                            None,
                            Some(next_str),
                            Some(&next_summary),
                        )
                        .await;
                        let log = task_db::create_task_log(
                            &self.db,
                            &t.id,
                            "info",
                            &format!("Stage {} started", next_str),
                            "",
                            "",
                            next_str,
                        )
                        .await;
                        // Broadcast updates
                        if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                            && let Some(ref ut) = updated
                        {
                            self.broadcast_task_changed(ut).await;
                        }
                        if let Ok(ref l) = log {
                            self.broadcast_task_log(&t.id, l).await;
                        }
                    }

                    // Emit pipeline:next to the next role room
                    let payload = serde_json::json!({
                        "run_id": run_id,
                        "stage": next_str,
                        "artifact_id": artifact_id,
                        "metadata": output,
                    });

                    let room = stage_to_room(&next);
                    if let Err(e) = self
                        .io
                        .to(room.clone())
                        .emit(events::PIPELINE_NEXT, &payload)
                        .await
                    {
                        error!(
                            run_id = %run_id,
                            next_stage = %next_str,
                            room = %room,
                            err = %e,
                            "failed to emit pipeline:next for next stage"
                        );
                    }
                    // Notify dashboard clients
                    let _ = self
                        .io
                        .to("dashboard")
                        .emit(
                            "dashboard:event",
                            &serde_json::json!({ "event": "pipeline:next", "data": payload }),
                        )
                        .await;

                    info!(
                        run_id = %run_id,
                        from = %stage_str,
                        to = %next_str,
                        "pipeline advanced to next stage"
                    );
                }
                None => {
                    // SkillManage was the last stage — pipeline run complete
                    info!(
                        run_id = %run_id,
                        "pipeline run completed all stages"
                    );

                    // Mark task as completed
                    if let Some(ref t) = task {
                        let _ = task_db::update_task(
                            &self.db,
                            &t.id,
                            Some("completed"),
                            None,
                            None,
                            None,
                            Some("Pipeline completed successfully"),
                        )
                        .await;
                        let log = task_db::create_task_log(
                            &self.db,
                            &t.id,
                            "info",
                            "Pipeline completed successfully",
                            "",
                            agent_id,
                            stage_str,
                        )
                        .await;
                        if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                            && let Some(ref ut) = updated
                        {
                            self.broadcast_task_changed(ut).await;
                        }
                        if let Ok(ref l) = log {
                            self.broadcast_task_log(&t.id, l).await;
                        }
                    }

                    // Check if this was a self-upgrade pipeline that was approved
                    if output["build_type"].as_str() == Some("self_upgrade")
                        && output["action"].as_str() == Some("activated")
                    {
                        let component = output["component"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_string();
                        let new_version = output["new_version"]
                            .as_str()
                            .unwrap_or("v0.0.0")
                            .to_string();
                        let rid = run_id.to_string();

                        info!(
                            run_id = %rid,
                            component = %component,
                            new_version = %new_version,
                            "self-upgrade approved — triggering update.sh"
                        );

                        tokio::spawn(async move {
                            if let Err(e) = trigger_update(&component, &new_version, &rid).await {
                                error!(
                                    run_id = %rid,
                                    component = %component,
                                    err = %e,
                                    "update.sh failed"
                                );
                            }
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Create subtasks from evaluation agent output.
    ///
    /// Each entry in `subtask_defs` should have `task_type`, `summary`, and
    /// optionally `payload`. The created tasks are linked to `parent_task` via
    /// `parent_id` and share the same `run_id`.
    pub(crate) async fn create_subtasks(
        &self,
        parent_task: &task_db::TaskRow,
        subtask_defs: &[serde_json::Value],
        run_id: &str,
        agent_id: &str,
    ) {
        for (i, def) in subtask_defs.iter().enumerate() {
            let task_type = def["task_type"].as_str().unwrap_or("subtask");
            let payload = def.get("payload").cloned().unwrap_or(serde_json::json!({}));
            let default_summary = format!("Subtask {}", i + 1);
            let summary = def["summary"].as_str().unwrap_or(&default_summary);

            match task_db::create_task(
                &self.db,
                task_type,
                "pending",
                None,
                &payload.to_string(),
                run_id,
                "",
                summary,
                &parent_task.id,
            )
            .await
            {
                Ok(subtask) => {
                    info!(
                        parent_id = %parent_task.id,
                        subtask_id = %subtask.id,
                        task_type = %task_type,
                        "subtask created from evaluation output"
                    );
                    self.broadcast_task_changed(&subtask).await;

                    let _ = task_db::create_task_log(
                        &self.db,
                        &parent_task.id,
                        "info",
                        &format!("Subtask created: {}", summary),
                        &serde_json::json!({"subtask_id": subtask.id}).to_string(),
                        agent_id,
                        "evaluation",
                    )
                    .await;
                }
                Err(e) => {
                    warn!(
                        parent_id = %parent_task.id,
                        err = %e,
                        "failed to create subtask"
                    );
                }
            }
        }
    }

    /// Handle the evaluation agent's error recovery recommendation.
    pub async fn on_recovery_response(
        &self,
        request_id: &str,
        run_id: &str,
        task_id: &str,
        action: &str,
        params: &Value,
        reasoning: &str,
    ) -> Result<()> {
        // Dedup: only process the first response for a given request_id
        {
            let mut pending = self.pending_requests.lock().await;
            if pending.remove(request_id).is_none() {
                info!(request_id = %request_id, "stale or duplicate recovery response, ignoring");
                return Ok(());
            }
        }

        info!(
            run_id = %run_id,
            action = %action,
            reasoning = %reasoning,
            "error recovery response received"
        );

        let task = task_db::get_task(&self.db, task_id).await.ok().flatten();
        let stage_str = task
            .as_ref()
            .map(|t| t.current_stage.as_str())
            .unwrap_or("");

        // Log the recovery decision
        if let Some(ref t) = task {
            let _ = task_db::create_task_log(
                &self.db,
                &t.id,
                "info",
                &format!("Recovery decision: {} — {}", action, reasoning),
                &params.to_string(),
                "",
                stage_str,
            )
            .await;
        }

        match action {
            "retry" => {
                let retry_count = task_db::get_stage_retry_count(&self.db, run_id, stage_str)
                    .await
                    .unwrap_or(0);

                if retry_count >= MAX_STAGE_RETRIES {
                    warn!(run_id = %run_id, "max retries exceeded — aborting");
                    self.abort_pipeline(run_id, task.as_ref(), stage_str, "Max retries exceeded")
                        .await;
                    return Ok(());
                }

                // Find the current stage record and retry it
                let stages = task_db::get_pipeline_run_stages(&self.db, run_id).await?;
                if let Some(stage_row) = stages.iter().rev().find(|s| s.stage == stage_str) {
                    task_db::retry_pipeline_stage(&self.db, &stage_row.id).await?;

                    if let Some(ref t) = task {
                        let _ = task_db::update_task(
                            &self.db,
                            &t.id,
                            Some("running"),
                            None,
                            None,
                            None,
                            Some(&format!(
                                "Retrying {} (attempt {})",
                                stage_str,
                                retry_count + 2
                            )),
                        )
                        .await;
                        if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                            && let Some(ref ut) = updated
                        {
                            self.broadcast_task_changed(ut).await;
                        }
                    }

                    // Re-emit pipeline:next using stored input_metadata
                    let metadata: Value = serde_json::from_str(&stage_row.input_metadata)
                        .unwrap_or(serde_json::json!({}));
                    let payload = serde_json::json!({
                        "run_id": run_id,
                        "stage": stage_str,
                        "artifact_id": stage_row.artifact_id,
                        "metadata": metadata,
                        "is_retry": true,
                        "retry_count": retry_count + 1,
                    });

                    if let Some(pipeline_stage) = str_to_stage(stage_str) {
                        let room = stage_to_room(&pipeline_stage);
                        let _ = self.io.to(room).emit(events::PIPELINE_NEXT, &payload).await;
                    }

                    info!(
                        run_id = %run_id,
                        stage = %stage_str,
                        retry = retry_count + 1,
                        "pipeline stage retry emitted"
                    );
                }
            }
            "decompose" => {
                if let Some(ref t) = task {
                    if let Some(subtasks) = params.get("subtasks").and_then(|v| v.as_array()) {
                        self.create_subtasks(t, subtasks, run_id, "evaluation")
                            .await;
                    }
                    let _ = task_db::update_task(
                        &self.db,
                        &t.id,
                        Some("decomposed"),
                        None,
                        None,
                        None,
                        Some("Task decomposed into subtasks after failure"),
                    )
                    .await;
                    if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                        && let Some(ref ut) = updated
                    {
                        self.broadcast_task_changed(ut).await;
                    }
                }
            }
            "skip" => {
                if stage_str == "evaluation" || stage_str == "skill_manage" {
                    info!(run_id = %run_id, stage = %stage_str, "skipping failed stage");
                    if let Some(ref t) = task {
                        let _ = task_db::create_task_log(
                            &self.db,
                            &t.id,
                            "warn",
                            &format!("Stage {} skipped after failure", stage_str),
                            reasoning,
                            "",
                            stage_str,
                        )
                        .await;
                    }

                    if let Some(current) = str_to_stage(stage_str) {
                        match next_stage(&current) {
                            Some(next) => {
                                let next_str = stage_to_str(&next);
                                let metadata_str = params.to_string();
                                task_db::create_pipeline_stage(
                                    &self.db,
                                    run_id,
                                    next_str,
                                    "",
                                    &metadata_str,
                                )
                                .await
                                .context("create next pipeline stage after skip")?;

                                if let Some(ref t) = task {
                                    let next_summary =
                                        format!("{} — {}", next_str, stage_summary(next_str));
                                    let _ = task_db::update_task(
                                        &self.db,
                                        &t.id,
                                        Some("running"),
                                        None,
                                        None,
                                        Some(next_str),
                                        Some(&next_summary),
                                    )
                                    .await;
                                    if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                                        && let Some(ref ut) = updated
                                    {
                                        self.broadcast_task_changed(ut).await;
                                    }
                                }

                                let payload = serde_json::json!({
                                    "run_id": run_id,
                                    "stage": next_str,
                                    "artifact_id": "",
                                    "metadata": {},
                                });
                                let room = stage_to_room(&next);
                                let _ =
                                    self.io.to(room).emit(events::PIPELINE_NEXT, &payload).await;
                            }
                            None => {
                                if let Some(ref t) = task {
                                    let _ = task_db::update_task(
                                        &self.db,
                                        &t.id,
                                        Some("completed"),
                                        None,
                                        None,
                                        None,
                                        Some("Pipeline completed (with skipped stage)"),
                                    )
                                    .await;
                                    if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                                        && let Some(ref ut) = updated
                                    {
                                        self.broadcast_task_changed(ut).await;
                                    }
                                }
                            }
                        }
                    }
                } else {
                    warn!(
                        run_id = %run_id,
                        stage = %stage_str,
                        "skip not allowed for this stage — aborting"
                    );
                    self.abort_pipeline(
                        run_id,
                        task.as_ref(),
                        stage_str,
                        "Skip not allowed for critical stage",
                    )
                    .await;
                }
            }
            _ => {
                self.abort_pipeline(run_id, task.as_ref(), stage_str, reasoning)
                    .await;
            }
        }

        Ok(())
    }

    /// Request decomposition of a task by the evaluation agent.
    pub async fn request_decomposition(
        &self,
        task: &task_db::TaskRow,
        trigger: &str,
    ) -> Result<()> {
        let request_id = uuid::Uuid::new_v4().to_string();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(
                request_id.clone(),
                PendingRequest {
                    run_id: task.run_id.clone(),
                    task_id: task.id.clone(),
                    request_type: "decompose".to_string(),
                },
            );
        }

        let payload_val: Value = serde_json::from_str(&task.payload).unwrap_or_default();
        let decompose_payload = serde_json::json!({
            "request_id": request_id,
            "run_id": task.run_id,
            "task_id": task.id,
            "task_type": task.task_type,
            "summary": task.summary,
            "payload": payload_val,
            "context": {},
            "trigger": trigger,
        });

        self.io
            .to("role:evaluation")
            .emit(events::TASK_DECOMPOSE, &decompose_payload)
            .await
            .map_err(|e| anyhow::anyhow!("emit task:decompose to evaluation agent: {e}"))?;

        info!(task_id = %task.id, trigger = %trigger, "decomposition requested from evaluation agent");
        Ok(())
    }

    /// Handle the decomposition result from the evaluation agent.
    pub async fn on_decompose_result(
        &self,
        request_id: &str,
        task_id: &str,
        should_decompose: bool,
        subtasks: &[Value],
        reasoning: &str,
    ) -> Result<()> {
        // Dedup
        let pending_info = {
            let mut pending = self.pending_requests.lock().await;
            pending.remove(request_id)
        };

        if pending_info.is_none() {
            info!(request_id = %request_id, "stale decompose response, ignoring");
            return Ok(());
        }

        info!(
            task_id = %task_id,
            should_decompose = %should_decompose,
            subtask_count = subtasks.len(),
            "decompose result received"
        );

        if should_decompose
            && !subtasks.is_empty()
            && let Ok(Some(task)) = task_db::get_task(&self.db, task_id).await
        {
            self.create_subtasks(&task, subtasks, &task.run_id, "evaluation")
                .await;

            let _ = task_db::update_task(
                &self.db,
                &task.id,
                Some("decomposed"),
                None,
                None,
                None,
                Some(&format!("Task decomposed: {}", reasoning)),
            )
            .await;
            if let Ok(updated) = task_db::get_task(&self.db, &task.id).await
                && let Some(ref ut) = updated
            {
                self.broadcast_task_changed(ut).await;
            }

            let _ = task_db::create_task_log(
                &self.db,
                &task.id,
                "info",
                &format!(
                    "Task decomposed into {} subtasks: {}",
                    subtasks.len(),
                    reasoning
                ),
                "",
                "",
                "",
            )
            .await;
        }

        Ok(())
    }

    /// Abort a pipeline run — mark task as failed.
    pub(crate) async fn abort_pipeline(
        &self,
        _run_id: &str,
        task: Option<&task_db::TaskRow>,
        stage_str: &str,
        reason: &str,
    ) {
        if let Some(t) = task {
            let summary = format!("Aborted at {} — {}", stage_str, reason);
            let _ = task_db::update_task(
                &self.db,
                &t.id,
                Some("failed"),
                None,
                None,
                None,
                Some(&summary),
            )
            .await;
            let _ = task_db::create_task_log(
                &self.db,
                &t.id,
                "error",
                &format!("Pipeline aborted: {}", reason),
                "",
                "",
                stage_str,
            )
            .await;
            if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                && let Some(ref ut) = updated
            {
                self.broadcast_task_changed(ut).await;
            }
        }
    }

    /// Background task that periodically checks for timed-out pipeline stages
    /// and marks them as `timed_out`.
    pub async fn timeout_monitor(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

        loop {
            interval.tick().await;

            match task_db::get_timed_out_stages(&self.db, DEFAULT_STAGE_TIMEOUT_SECS).await {
                Ok(stale) => {
                    for stage in stale {
                        warn!(
                            run_id = %stage.run_id,
                            stage = %stage.stage,
                            id = %stage.id,
                            created_at = %stage.created_at,
                            "pipeline stage timed out"
                        );

                        if let Err(e) = task_db::update_pipeline_stage(
                            &self.db,
                            &stage.id,
                            "timed_out",
                            &stage.agent_id,
                            None,
                            Some("stage timed out"),
                        )
                        .await
                        {
                            error!(err = %e, "failed to mark stage as timed_out");
                        }

                        // Update the linked task as failed + log the timeout
                        if let Ok(Some(task)) =
                            task_db::get_task_by_run_id(&self.db, &stage.run_id).await
                        {
                            let timeout_summary = format!("Timed out at {}", stage.stage);
                            let _ = task_db::update_task(
                                &self.db,
                                &task.id,
                                Some("failed"),
                                None,
                                None,
                                None,
                                Some(&timeout_summary),
                            )
                            .await;
                            let log = task_db::create_task_log(
                                &self.db,
                                &task.id,
                                "warn",
                                &format!(
                                    "Stage {} timed out after {}s",
                                    stage.stage, DEFAULT_STAGE_TIMEOUT_SECS
                                ),
                                "",
                                &stage.agent_id,
                                &stage.stage,
                            )
                            .await;
                            if let Ok(updated) = task_db::get_task(&self.db, &task.id).await
                                && let Some(ref ut) = updated
                            {
                                self.broadcast_task_changed(ut).await;
                            }
                            if let Ok(ref l) = log {
                                self.broadcast_task_log(&task.id, l).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(err = %e, "timeout monitor: failed to query timed-out stages");
                }
            }
        }
    }

    // ── Memory extraction ─────────────────────────────────────────────────────

    /// Auto-extract a memory record for a completed or failed pipeline stage.
    ///
    /// Creates a `pipeline` scoped memory with L0/L1/L2 tiers and optionally
    /// binds it to the pipeline task.
    async fn extract_stage_memory(
        &self,
        run_id: &str,
        stage: &str,
        agent_id: &str,
        artifact_id: &str,
        output: &Value,
        task_id: Option<&str>,
    ) {
        let category = match stage {
            "learning" => "resource",
            "building" => "fact",
            "pre_load" => "fact",
            "evaluation" => "case",
            "skill_manage" => "event",
            _ => "fact",
        };

        let key = format!("memory://pipeline/{run_id}/{stage}/{artifact_id}");

        let row = match memory_db::create_memory(
            &self.db, "pipeline", category, &key, "{}", "", agent_id, run_id, "", 0.5,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(run_id = %run_id, stage = %stage, err = %e, "failed to extract pipeline memory");
                return;
            }
        };

        // L0: one-line abstract
        let l0 = format!("{stage} completed by {agent_id} for artifact '{artifact_id}'");
        // L1: first 2000 chars of pretty-printed output
        let pretty = serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string());
        let l1: String = pretty.chars().take(2000).collect();
        // L2: full output
        let l2 = output.to_string();

        let _ = memory_db::upsert_tier(&self.db, &row.id, "l0", &l0).await;
        let _ = memory_db::upsert_tier(&self.db, &row.id, "l1", &l1).await;
        let _ = memory_db::upsert_tier(&self.db, &row.id, "l2", &l2).await;

        if let Some(tid) = task_id {
            let _ = memory_db::bind_task_memory(&self.db, tid, &row.id).await;
        }

        info!(
            run_id = %run_id,
            stage = %stage,
            memory_id = %row.id,
            "pipeline stage memory extracted"
        );
    }

    // ── Broadcast helpers ─────────────────────────────────────────────────────

    /// Broadcast a `task:changed` event to the dashboard room.
    async fn broadcast_task_changed(&self, task: &task_db::TaskRow) {
        let payload = serde_json::json!({
            "action": "updated",
            "task": {
                "id":            task.id,
                "task_type":     task.task_type,
                "status":        task.status,
                "agent_id":      task.agent_id,
                "run_id":        task.run_id,
                "current_stage": task.current_stage,
                "summary":       task.summary,
                "parent_id":     task.parent_id,
                "payload":       task.payload,
                "created_at":    task.created_at,
                "updated_at":    task.updated_at,
            },
        });
        let _ = self.io.to("dashboard").emit("task:changed", &payload).await;
    }

    /// Broadcast a `task:log` event to the dashboard room and the task room.
    async fn broadcast_task_log(&self, task_id: &str, log: &task_db::TaskLogRow) {
        let payload = serde_json::json!({
            "task_id": task_id,
            "log": {
                "id":         log.id,
                "level":      log.level,
                "message":    log.message,
                "detail":     log.detail,
                "agent_id":   log.agent_id,
                "stage":      log.stage,
                "created_at": log.created_at,
            },
        });
        let _ = self.io.to("dashboard").emit("task:log", &payload).await;

        // Also emit to the task-specific room so joined agents can observe
        let task_room = format!("{}{}", events::ROOM_TASK_PREFIX, task_id);
        let _ = self.io.to(task_room).emit(events::TASK_LOG, &payload).await;
    }

    /// List all pipeline runs for HTTP API.
    pub async fn list_runs(
        &self,
        limit: u32,
        status_filter: Option<&str>,
    ) -> Result<Vec<task_db::PipelineRow>> {
        task_db::list_pipeline_runs(&self.db, limit, status_filter).await
    }

    /// Get detailed stage history for a pipeline run.
    pub async fn get_run_stages(&self, run_id: &str) -> Result<Vec<task_db::PipelineRow>> {
        task_db::get_pipeline_run_stages(&self.db, run_id).await
    }
}

// ─── Pipeline stage helpers ─────────────────────────────────────────────────

/// Returns the next stage in the pipeline, or `None` if the current stage is the last.
fn next_stage(current: &PipelineStage) -> Option<PipelineStage> {
    match current {
        PipelineStage::Learning => Some(PipelineStage::Building),
        PipelineStage::Building => Some(PipelineStage::PreLoad),
        PipelineStage::PreLoad => Some(PipelineStage::Evaluation),
        PipelineStage::Evaluation => Some(PipelineStage::SkillManage),
        PipelineStage::SkillManage => None,
    }
}

/// Map a pipeline stage to its role-specific Socket.IO room name.
fn stage_to_room(stage: &PipelineStage) -> String {
    format!("{}{}", events::ROOM_ROLE_PREFIX, stage_to_str(stage))
}

/// Serialize a PipelineStage to the snake_case string used in DB and events.
fn stage_to_str(stage: &PipelineStage) -> &'static str {
    match stage {
        PipelineStage::Learning => "learning",
        PipelineStage::Building => "building",
        PipelineStage::PreLoad => "pre_load",
        PipelineStage::Evaluation => "evaluation",
        PipelineStage::SkillManage => "skill_manage",
    }
}

/// Parse a snake_case string back into a PipelineStage.
fn str_to_stage(s: &str) -> Option<PipelineStage> {
    match s {
        "learning" => Some(PipelineStage::Learning),
        "building" => Some(PipelineStage::Building),
        "pre_load" => Some(PipelineStage::PreLoad),
        "evaluation" => Some(PipelineStage::Evaluation),
        "skill_manage" => Some(PipelineStage::SkillManage),
        _ => None,
    }
}

/// Human-readable description for each pipeline stage (used in task summary).
fn stage_summary(stage: &str) -> &'static str {
    match stage {
        "learning" => "Discovering skills",
        "building" => "Packaging skill",
        "pre_load" => "Checking endpoints",
        "evaluation" => "Evaluating quality",
        "skill_manage" => "Managing activation",
        _ => "Processing",
    }
}

// ─── Self-upgrade trigger ───────────────────────────────────────────────────

/// Resolve the EVO_HOME directory (~/.evo-agents).
fn evo_home() -> PathBuf {
    let raw = std::env::var("EVO_HOME").unwrap_or_else(|_| "~/.evo-agents".to_string());
    if raw.starts_with("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(format!("{home}{}", &raw[1..]));
    }
    PathBuf::from(raw)
}

/// Run `update.sh <component> <new_version>` to install a new release.
///
/// The update script handles: backup → download → extract → repos.json
/// update → doctor validation → rollback on failure.
async fn trigger_update(component: &str, new_version: &str, run_id: &str) -> Result<()> {
    let home = evo_home();

    // Look for update.sh in several locations
    let candidates = [
        home.join("update.sh"),
        home.join("bin/update.sh"),
        // During development, the script may live in the king repo
        PathBuf::from(std::env::var("KERNEL_AGENTS_DIR").unwrap_or_else(|_| "..".into()))
            .join("evo-king/update.sh"),
    ];

    let script = candidates.iter().find(|p| p.exists());

    let script_path = match script {
        Some(p) => p.clone(),
        None => {
            warn!(
                run_id = %run_id,
                component,
                "update.sh not found — skipping automatic update"
            );
            return Ok(());
        }
    };

    info!(
        run_id = %run_id,
        script = %script_path.display(),
        component,
        new_version,
        "executing update.sh"
    );

    let output = Command::new("bash")
        .arg(&script_path)
        .arg(component)
        .arg(new_version)
        .env("EVO_HOME", home.to_string_lossy().as_ref())
        .output()
        .await
        .context("failed to spawn update.sh")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        info!(
            run_id = %run_id,
            component,
            new_version,
            stdout = %stdout.trim(),
            "update.sh completed successfully"
        );
    } else {
        let code = output.status.code().unwrap_or(-1);
        error!(
            run_id = %run_id,
            component,
            new_version,
            exit_code = code,
            stderr = %stderr.trim(),
            "update.sh failed — rollback should have been triggered by the script"
        );
        anyhow::bail!("update.sh exited with code {code}: {stderr}");
    }

    Ok(())
}
