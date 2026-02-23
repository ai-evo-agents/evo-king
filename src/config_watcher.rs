use crate::{gateway_manager, state::KingState};
use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ─── Config file watcher ──────────────────────────────────────────────────────

/// Watch `path` (the gateway.json file) for modifications.
///
/// When the file changes, `gateway_manager::on_config_change` is called.
/// This function runs indefinitely — wrap it in `tokio::spawn`.
pub async fn watch_config(path: &str, state: Arc<KingState>) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<notify::Result<notify::Event>>(32);

    // Canonicalize so path comparisons are exact
    let path_buf = std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path));

    let watch_dir = path_buf
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut watcher = notify::recommended_watcher(move |res| {
        // blocking_send is fine here — watcher callback runs in a thread
        let _ = tx.blocking_send(res);
    })?;

    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    info!(path = %path_buf.display(), "watching gateway config for changes");

    while let Some(event_result) = rx.recv().await {
        match event_result {
            Ok(ev)
                if matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_)
                ) =>
            {
                // Only react if it's our specific file
                if ev.paths.iter().any(|p| p == &path_buf) {
                    info!("gateway config file changed, running lifecycle");
                    if let Err(e) =
                        gateway_manager::on_config_change(path, &state).await
                    {
                        error!(err = %e, "config lifecycle failed");
                    }
                }
            }
            Err(e) => warn!(err = %e, "filesystem watcher error"),
            _ => {} // access / remove / other — ignore
        }
    }

    // Watcher drops here when channel closes
    drop(watcher);
    Ok(())
}
