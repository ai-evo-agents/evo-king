#![allow(dead_code)]

use anyhow::{Context, Result};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};
use tokio::process::{Child, Command};
use tracing::{info, warn};

// ─── Process registry ─────────────────────────────────────────────────────────

/// Tracks PIDs of all spawned runner processes.
pub struct AgentRegistry {
    processes: Mutex<HashMap<String, u32>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, agent_id: &str, pid: u32) {
        let mut map = self.processes.lock().unwrap();
        map.insert(agent_id.to_string(), pid);
        info!(agent_id = %agent_id, pid = pid, "runner process registered");
    }

    #[allow(dead_code)]
    pub fn unregister(&self, agent_id: &str) {
        let mut map = self.processes.lock().unwrap();
        map.remove(agent_id);
    }

    #[allow(dead_code)]
    pub fn all_pids(&self) -> Vec<(String, u32)> {
        let map = self.processes.lock().unwrap();
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

// ─── Process spawning ─────────────────────────────────────────────────────────

/// Spawn a runner binary for the given agent folder.
///
/// The runner binary is expected to:
/// - Accept `<agent-folder>` as its first argument.
/// - Read `KING_ADDRESS` env var to connect back to king.
pub async fn spawn_runner(
    runner_binary: &str,
    agent_folder: &str,
    king_address: &str,
) -> Result<Child> {
    info!(
        runner   = %runner_binary,
        folder   = %agent_folder,
        king     = %king_address,
        "spawning runner process"
    );

    let child = Command::new(runner_binary)
        .arg(agent_folder)
        .env("KING_ADDRESS", king_address)
        .spawn()
        .with_context(|| format!("Failed to spawn runner '{runner_binary}'"))?;

    Ok(child)
}

// ─── Agent folder discovery ───────────────────────────────────────────────────

/// Return all kernel agent folders that contain a `soul.md` file.
///
/// Looks under `<agents_root>/kernel/`.
pub fn discover_kernel_agents(agents_root: &str) -> Vec<PathBuf> {
    let kernel_dir = PathBuf::from(agents_root).join("kernel");

    let entries = match std::fs::read_dir(&kernel_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %kernel_dir.display(), err = %e, "cannot read kernel agents dir");
            return vec![];
        }
    };

    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.path().join("soul.md").exists())
        .map(|e| e.path())
        .collect()
}
