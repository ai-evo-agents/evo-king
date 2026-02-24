use anyhow::{Context, Result};
use libsql::Database;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::task_db;

// ─── Spawned process metadata ────────────────────────────────────────────────

/// Metadata for a runner process spawned by king.
pub struct AgentProcess {
    pub pid: u32,
    pub child: Child,
    pub agent_folder: String,
}

// ─── Process registry ─────────────────────────────────────────────────────────

/// Tracks all spawned runner processes.
///
/// Uses a `tokio::sync::Mutex` (instead of `std::sync::Mutex`) so the
/// `monitor_processes` task can hold the lock across `.await` points.
pub struct AgentRegistry {
    processes: Mutex<HashMap<String, AgentProcess>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }

    /// Register a spawned runner process.
    pub async fn register(&self, key: &str, process: AgentProcess) {
        let mut map = self.processes.lock().await;
        info!(key = %key, pid = process.pid, folder = %process.agent_folder, "runner process registered");
        map.insert(key.to_string(), process);
    }

    /// Remove and return a process entry.
    #[allow(dead_code)]
    pub async fn unregister(&self, key: &str) -> Option<AgentProcess> {
        let mut map = self.processes.lock().await;
        map.remove(key)
    }

    /// List all registered (key, pid, folder) tuples.
    #[allow(dead_code)]
    pub async fn all_agents(&self) -> Vec<(String, u32, String)> {
        let map = self.processes.lock().await;
        map.iter()
            .map(|(k, v)| (k.clone(), v.pid, v.agent_folder.clone()))
            .collect()
    }

    /// Find a PID by matching the role name against stored folder paths.
    ///
    /// Kernel agent folders are named after their role (e.g. `kernel/learning`),
    /// so a role like `"learning"` will match a folder containing `"learning"`.
    pub async fn pid_by_folder_hint(&self, role: &str) -> Option<u32> {
        let map = self.processes.lock().await;
        map.values()
            .find(|p| p.agent_folder.contains(role))
            .map(|p| p.pid)
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
        .with_context(|| format!("Failed to spawn runner '{runner_binary}' for '{agent_folder}'"))?;

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

    let mut agents: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.path().join("soul.md").exists())
        .map(|e| e.path())
        .collect();

    agents.sort(); // deterministic ordering
    agents
}

// ─── Orchestration ───────────────────────────────────────────────────────────

/// Discover and spawn all kernel agents.
///
/// Returns the list of folder names that were successfully spawned.
pub async fn spawn_all_kernel_agents(
    registry: &AgentRegistry,
    agents_root: &str,
    runner_binary: &str,
    king_address: &str,
) -> Vec<String> {
    let agent_dirs = discover_kernel_agents(agents_root);

    if agent_dirs.is_empty() {
        warn!(root = %agents_root, "no kernel agents discovered");
        return vec![];
    }

    info!(count = agent_dirs.len(), root = %agents_root, "discovered kernel agent folders");

    let mut spawned = Vec::new();

    for dir in &agent_dirs {
        let folder_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let folder_str = dir.to_string_lossy().to_string();

        match spawn_runner(runner_binary, &folder_str, king_address).await {
            Ok(child) => {
                let pid = child.id().unwrap_or(0);
                info!(agent = %folder_name, pid = pid, "kernel agent spawned");
                registry
                    .register(
                        &folder_name,
                        AgentProcess {
                            pid,
                            child,
                            agent_folder: folder_str,
                        },
                    )
                    .await;
                spawned.push(folder_name);
            }
            Err(e) => {
                error!(agent = %folder_name, err = %e, "failed to spawn kernel agent");
            }
        }
    }

    spawned
}

// ─── Process monitoring ──────────────────────────────────────────────────────

/// Background task that periodically checks whether spawned runners are still
/// alive. If a runner exits, it logs the exit code and marks the agent as
/// `"crashed"` in the database.
pub async fn monitor_processes(registry: Arc<AgentRegistry>, db: Arc<Database>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut map = registry.processes.lock().await;

        // Collect keys of exited processes
        let mut exited = Vec::new();

        for (key, proc) in map.iter_mut() {
            match proc.child.try_wait() {
                Ok(Some(status)) => {
                    warn!(
                        agent = %key,
                        pid = proc.pid,
                        exit_code = ?status.code(),
                        "runner process exited"
                    );
                    exited.push(key.clone());
                }
                Ok(None) => {
                    // still running — nothing to do
                }
                Err(e) => {
                    warn!(agent = %key, err = %e, "failed to check runner process status");
                }
            }
        }

        // Remove exited processes and update DB
        for key in exited {
            if let Some(proc) = map.remove(&key) {
                // Try to mark the agent as crashed in the DB.
                // We match by folder hint since we don't know the Socket.IO agent_id.
                if let Err(e) = task_db::mark_agent_crashed_by_role(&db, &key).await {
                    warn!(agent = %key, err = %e, "failed to update crashed agent status in DB");
                } else {
                    info!(agent = %key, pid = proc.pid, "agent marked as crashed in DB");
                }
            }
        }
    }
}
