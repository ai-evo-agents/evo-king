use crate::{state::KingState, task_db};
use anyhow::{Context, Result};
use chrono::Utc;
use evo_common::{config::GatewayConfig, messages::events};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::Path,
};
use tracing::{info, warn};

// ─── Config lifecycle entry point ─────────────────────────────────────────────

/// Called by the config watcher whenever `gateway.json` changes.
///
/// Steps:
///   1. Read & validate the new JSON.
///   2. Create a timestamped backup in `<config_dir>/backups/`.
///   3. Log the event to the Turso DB.
///   4. Broadcast `king:config_update` to all connected runners.
pub async fn on_config_change(config_path: &str, state: &KingState) -> Result<()> {
    let content = tokio::fs::read_to_string(config_path)
        .await
        .with_context(|| format!("Failed to read {config_path}"))?;

    // --- Step 1: Validate JSON ---
    let config = GatewayConfig::from_json(&content)
        .with_context(|| format!("Invalid JSON in {config_path}"))?;

    info!(
        providers = config.providers.len(),
        "gateway config validated"
    );

    let config_hash = quick_hash(&content);

    // --- Step 2: Backup ---
    let backup_path = backup_config(config_path, &content).await;

    let backup_path_str = match &backup_path {
        Ok(p) => {
            info!(backup = %p, "config backup created");
            p.clone()
        }
        Err(e) => {
            warn!(err = %e, "config backup failed (continuing)");
            String::new()
        }
    };

    // --- Step 3: Log to DB ---
    if let Err(e) = task_db::log_config_event(
        &state.db,
        &config_hash,
        "config_changed",
        if backup_path_str.is_empty() {
            None
        } else {
            Some(&backup_path_str)
        },
    )
    .await
    {
        warn!(err = %e, "failed to log config event to DB");
    }

    // --- Step 4: Broadcast to runners ---
    let payload = serde_json::json!({
        "config_hash": config_hash,
        "providers": config.providers.len()
    });

    if let Err(e) = state.io.emit(events::KING_CONFIG_UPDATE, &payload) {
        warn!(err = %e, "failed to broadcast config update to runners");
    }

    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn backup_config(config_path: &str, content: &str) -> Result<String> {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");

    let parent = Path::new(config_path).parent().unwrap_or(Path::new("."));

    let backup_dir = parent.join("backups");
    tokio::fs::create_dir_all(&backup_dir)
        .await
        .context("Failed to create backups/ directory")?;

    let backup_path = backup_dir.join(format!("gateway-backup-{timestamp}.json"));
    tokio::fs::write(&backup_path, content)
        .await
        .context("Failed to write backup file")?;

    Ok(backup_path.to_string_lossy().to_string())
}

fn quick_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}
