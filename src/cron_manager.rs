use anyhow::Result;
use evo_common::messages::events;
use libsql::Database;
use serde_json::json;
use socketioxide::SocketIo;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::task_db;

// ─── System cron definitions ─────────────────────────────────────────────────

/// Well-known system cron job names.
pub mod names {
    pub const DAILY_UPDATE_CHECK: &str = "daily_update_check";
    pub const GATEWAY_HEALTH_CHECK: &str = "gateway_health_check";
}

/// (name, schedule_str, poll_interval) — schedule_str is stored in the DB,
/// poll_interval is the actual `Duration` used by the polling loop.
const SYSTEM_CRONS: &[(&str, &str, Duration)] = &[
    (
        names::DAILY_UPDATE_CHECK,
        "0 0 * * *",
        Duration::from_secs(24 * 60 * 60),
    ),
    (
        names::GATEWAY_HEALTH_CHECK,
        "0 * * * *",
        Duration::from_secs(60 * 60),
    ),
];

// ─── Init ─────────────────────────────────────────────────────────────────────

/// Seed the system cron jobs into the `cron_jobs` DB table.
///
/// Called once at king startup, after `init_db()`.  Uses `ON CONFLICT DO
/// UPDATE` so existing rows are preserved (schedule, enabled flag, etc.).
pub async fn init_system_crons(db: &Database) -> Result<()> {
    for (name, schedule, _) in SYSTEM_CRONS {
        let id = task_db::upsert_cron_job(db, name, schedule, true).await?;
        info!(name = %name, id = %id, schedule = %schedule, "system cron job seeded");
    }
    Ok(())
}

// ─── Cron runner ─────────────────────────────────────────────────────────────

/// Background task that drives all enabled cron jobs.
///
/// Spawns one `tokio::task` per system cron, each running an independent
/// interval loop.  The task never exits voluntarily; king relies on process
/// supervision if it panics.
pub async fn run(db: Arc<Database>, io: SocketIo) {
    // Spawn a watcher loop per system cron
    for &(name, _schedule, interval) in SYSTEM_CRONS {
        let db2 = Arc::clone(&db);
        let io2 = io.clone();

        tokio::spawn(async move {
            cron_loop(name, interval, db2, io2).await;
        });
    }

    // Keep this future alive — the spawned tasks run independently
    std::future::pending::<()>().await;
}

/// Single cron loop for a named job.
async fn cron_loop(name: &'static str, interval: Duration, db: Arc<Database>, io: SocketIo) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Check if still enabled in DB
        let jobs = match task_db::list_cron_jobs(&db).await {
            Ok(jobs) => jobs,
            Err(e) => {
                error!(name = %name, error = %e, "cron: failed to query job status");
                continue;
            }
        };

        let enabled = jobs
            .iter()
            .find(|j| j.name == name)
            .map(|j| j.enabled)
            .unwrap_or(false);

        if !enabled {
            info!(name = %name, "cron job disabled — skipping");
            continue;
        }

        info!(name = %name, "cron job firing");

        let (status, err_msg) = match dispatch_cron_job(name, &db, &io).await {
            Ok(()) => ("ok", None),
            Err(e) => {
                warn!(name = %name, error = %e, "cron job failed");
                ("error", Some(e.to_string()))
            }
        };

        if let Err(e) = task_db::update_cron_run(&db, name, status, err_msg.as_deref(), None).await
        {
            error!(name = %name, error = %e, "cron: failed to record run result");
        }
    }
}

/// Execute a single cron job by name.
async fn dispatch_cron_job(name: &str, db: &Database, io: &SocketIo) -> Result<()> {
    match name {
        names::DAILY_UPDATE_CHECK => dispatch_update_check(db, io).await,
        names::GATEWAY_HEALTH_CHECK => dispatch_gateway_health(db, io).await,
        other => {
            warn!(name = %other, "unknown cron job name — no-op");
            Ok(())
        }
    }
}

// ─── Job implementations ──────────────────────────────────────────────────────

/// Dispatch a pipeline:next event to the `role:update` Socket.IO room,
/// triggering the update agent to check crates.io and bump deps if needed.
async fn dispatch_update_check(db: &Database, io: &SocketIo) -> Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let stage = "update";
    let room = format!("{}{}", events::ROOM_ROLE_PREFIX, stage);

    let payload = json!({
        "run_id": run_id,
        "stage": stage,
        "artifact_id": "",
        "metadata": {
            "trigger": "cron",
            "cron_job": names::DAILY_UPDATE_CHECK,
        }
    });

    if let Err(e) = io.to(room.clone()).emit(events::PIPELINE_NEXT, &payload) {
        anyhow::bail!("failed to emit pipeline:next to {room}: {e}");
    }

    // Record in pipeline_runs for visibility in GET /pipeline/runs
    let _ = task_db::create_pipeline_stage(db, &run_id, stage, "").await;

    info!(run_id = %run_id, room = %room, "dispatched update check via cron");
    Ok(())
}

/// Broadcast a `king:config_update` event so all agents recheck gateway health.
async fn dispatch_gateway_health(_db: &Database, io: &SocketIo) -> Result<()> {
    let payload = json!({
        "trigger": "cron",
        "reason": "hourly_health_check",
    });

    if let Err(e) = io.emit(evo_common::messages::events::KING_CONFIG_UPDATE, &payload) {
        anyhow::bail!("failed to broadcast king:config_update: {e}");
    }

    info!("dispatched gateway health check broadcast via cron");
    Ok(())
}

// ─── Manual trigger ───────────────────────────────────────────────────────────

/// Immediately run a cron job by name, regardless of its schedule.
///
/// Used by `POST /admin/crons/:name/run`.
pub async fn run_now(name: &str, db: Arc<Database>, io: SocketIo) -> Result<()> {
    info!(name = %name, "manually triggering cron job");

    let (status, err_msg) = match dispatch_cron_job(name, &db, &io).await {
        Ok(()) => ("ok", None),
        Err(e) => {
            warn!(name = %name, error = %e, "manual cron trigger failed");
            ("error", Some(e.to_string()))
        }
    };

    task_db::update_cron_run(&db, name, status, err_msg.as_deref(), None).await?;

    if let Some(err) = err_msg {
        anyhow::bail!("{err}");
    }
    Ok(())
}
