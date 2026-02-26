use crate::{memory_db, task_db};
use anyhow::{Context, Result};
use evo_common::messages::{PipelineStage, events};
use libsql::Database;
use serde_json::Value;
use socketioxide::SocketIo;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{error, info, warn};

/// Default stage timeout in seconds (5 minutes).
const DEFAULT_STAGE_TIMEOUT_SECS: i64 = 300;

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
}

impl PipelineCoordinator {
    pub fn new(db: Arc<Database>, io: SocketIo) -> Self {
        Self { db, io }
    }

    /// Start a new pipeline run. Creates a DB record for the `learning` stage,
    /// creates a main task linked to this run, inserts the first task log,
    /// and emits `pipeline:next` to the `role:learning` room.
    pub async fn start_run(&self, trigger_metadata: Value) -> Result<String> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let first_stage = PipelineStage::Learning;
        let stage_str = stage_to_str(&first_stage);

        // Create the first stage record
        let _row = task_db::create_pipeline_stage(&self.db, &run_id, stage_str, "")
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
        self.broadcast_task_changed(&task_row);
        if let Ok(log) = &log_row {
            self.broadcast_task_log(&task_row.id, log);
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
        {
            error!(run_id = %run_id, room = %room, err = %e, "failed to emit pipeline:next");
        }
        // Notify dashboard clients
        let _ = self.io.to("dashboard").emit(
            "dashboard:event",
            serde_json::json!({ "event": "pipeline:next", "data": payload }),
        );

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

        // If failed, the pipeline run stops here
        if status == "failed" {
            let err_text = error_msg.unwrap_or("unknown error");
            warn!(
                run_id = %run_id,
                stage = %stage_str,
                error = %err_text,
                "pipeline run failed at stage"
            );

            // Extract memory for the failed stage
            let task_id = task.as_ref().map(|t| t.id.as_str());
            self.extract_stage_memory(run_id, stage_str, agent_id, artifact_id, output, task_id)
                .await;

            if let Some(ref t) = task {
                let fail_summary = format!("Failed at {} — {}", stage_str, err_text);
                let _ = task_db::update_task(
                    &self.db,
                    &t.id,
                    Some("failed"),
                    None,
                    None,
                    None,
                    Some(&fail_summary),
                )
                .await;
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
                // Broadcast updates
                if let Ok(updated) = task_db::get_task(&self.db, &t.id).await
                    && let Some(ref ut) = updated
                {
                    self.broadcast_task_changed(ut);
                }
                if let Ok(ref l) = log {
                    self.broadcast_task_log(&t.id, l);
                }
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
                    self.broadcast_task_log(&t.id, l);
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
                    task_db::create_pipeline_stage(&self.db, run_id, next_str, artifact_id)
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
                            self.broadcast_task_changed(ut);
                        }
                        if let Ok(ref l) = log {
                            self.broadcast_task_log(&t.id, l);
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
                    let _ = self.io.to("dashboard").emit(
                        "dashboard:event",
                        serde_json::json!({ "event": "pipeline:next", "data": payload }),
                    );

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
                            self.broadcast_task_changed(ut);
                        }
                        if let Ok(ref l) = log {
                            self.broadcast_task_log(&t.id, l);
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
    async fn create_subtasks(
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
                    self.broadcast_task_changed(&subtask);

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
                                self.broadcast_task_changed(ut);
                            }
                            if let Ok(ref l) = log {
                                self.broadcast_task_log(&task.id, l);
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
    fn broadcast_task_changed(&self, task: &task_db::TaskRow) {
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
        let _ = self.io.to("dashboard").emit("task:changed", &payload);
    }

    /// Broadcast a `task:log` event to the dashboard room and the task room.
    fn broadcast_task_log(&self, task_id: &str, log: &task_db::TaskLogRow) {
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
        let _ = self.io.to("dashboard").emit("task:log", &payload);

        // Also emit to the task-specific room so joined agents can observe
        let task_room = format!("{}{}", events::ROOM_TASK_PREFIX, task_id);
        let _ = self.io.to(task_room).emit(events::TASK_LOG, &payload);
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
