use crate::task_db;
use anyhow::{Context, Result};
use evo_common::messages::{PipelineStage, events};
use libsql::Database;
use serde_json::Value;
use socketioxide::SocketIo;
use std::sync::Arc;
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

    /// Start a new pipeline run. Creates a DB record for the `learning` stage
    /// and emits `pipeline:next` to the `role:learning` room.
    pub async fn start_run(&self, trigger_metadata: Value) -> Result<String> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let first_stage = PipelineStage::Learning;
        let stage_str = stage_to_str(&first_stage);

        // Create the first stage record
        let row = task_db::create_pipeline_stage(&self.db, &run_id, stage_str, "")
            .await
            .context("create initial pipeline stage")?;

        info!(
            run_id = %run_id,
            stage = %stage_str,
            "pipeline run started"
        );

        // Emit pipeline:next to the role room for the first stage
        let payload = serde_json::json!({
            "run_id": run_id,
            "stage": stage_str,
            "artifact_id": "",
            "metadata": trigger_metadata,
        });

        let room = stage_to_room(&first_stage);
        if let Err(e) = self.io.to(room.clone()).emit(events::PIPELINE_NEXT, &payload) {
            error!(run_id = %run_id, room = %room, err = %e, "failed to emit pipeline:next");
        }

        // Also log as a task for visibility
        let _ = task_db::create_task(
            &self.db,
            "pipeline_run",
            None,
            &serde_json::json!({
                "run_id": run_id,
                "stage_id": row.id,
                "trigger": trigger_metadata,
            })
            .to_string(),
        )
        .await;

        Ok(run_id)
    }

    /// Handle a stage result reported by an agent. Advances the pipeline to the
    /// next stage, or marks the run as completed/failed.
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

        // If failed, the pipeline run stops here
        if status == "failed" {
            warn!(
                run_id = %run_id,
                stage = %stage_str,
                error = ?error_msg,
                "pipeline run failed at stage"
            );
            return Ok(());
        }

        // If completed, advance to the next stage
        if status == "completed" {
            match next_stage(stage) {
                Some(next) => {
                    let next_str = stage_to_str(&next);

                    // Create the next stage record
                    task_db::create_pipeline_stage(&self.db, run_id, next_str, artifact_id)
                        .await
                        .context("create next pipeline stage")?;

                    // Emit pipeline:next to the next role room
                    let payload = serde_json::json!({
                        "run_id": run_id,
                        "stage": next_str,
                        "artifact_id": artifact_id,
                        "metadata": output,
                    });

                    let room = stage_to_room(&next);
                    if let Err(e) = self.io.to(room.clone()).emit(events::PIPELINE_NEXT, &payload) {
                        error!(
                            run_id = %run_id,
                            next_stage = %next_str,
                            room = %room,
                            err = %e,
                            "failed to emit pipeline:next for next stage"
                        );
                    }

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
                }
            }
        }

        Ok(())
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
                    }
                }
                Err(e) => {
                    error!(err = %e, "timeout monitor: failed to query timed-out stages");
                }
            }
        }
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
