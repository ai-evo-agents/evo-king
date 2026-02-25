use anyhow::{Context, Result};
use libsql::Database;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::task_db;

/// Maximum number of automatic respawn attempts before giving up.
const MAX_RESPAWN_ATTEMPTS: u32 = 3;

/// Base delay for exponential backoff between respawn attempts (seconds).
const RESPAWN_BASE_DELAY_SECS: u64 = 5;

/// Heartbeat check interval — how often the heartbeat loop runs.
const HEARTBEAT_CHECK_INTERVAL_SECS: u64 = 60;

/// Silence threshold — if an agent has not sent a heartbeat for this many
/// seconds it is considered missing and a respawn is attempted.
const HEARTBEAT_SILENCE_THRESHOLD_SECS: i64 = 90;

/// Kernel agent roles expected to be running at all times.
/// King will warn and attempt respawn if any are silent for too long.
const EXPECTED_KERNEL_ROLES: &[&str] = &[
    "learning",
    "building",
    "pre_load",
    "evaluation",
    "skill_manage",
    "update",
];

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

/// Derive the per-agent binary name from a folder name.
///
/// `evo-kernel-agent-learning` → `evo-agent-learning`
/// `evo-user-agent-my-bot`     → `evo-user-agent-my-bot` (unchanged)
fn per_agent_binary_name(folder_name: &str) -> String {
    if let Some(suffix) = folder_name.strip_prefix("evo-kernel-agent-") {
        format!("evo-agent-{suffix}")
    } else {
        // User agents keep the folder name as-is for the binary
        folder_name.to_string()
    }
}

/// Spawn a runner process for the given agent folder.
///
/// Tries the per-agent binary first (e.g. `evo-kernel-agent-learning/evo-agent-learning`).
/// Falls back to the generic runner binary if the per-agent binary does not exist.
pub async fn spawn_runner(
    runner_binary: &str,
    agent_folder: &str,
    king_address: &str,
) -> Result<Child> {
    let folder_path = Path::new(agent_folder);
    let folder_name = folder_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // Try per-agent binary first
    let agent_binary_name = per_agent_binary_name(folder_name);
    let agent_binary_path = folder_path.join(&agent_binary_name);

    if agent_binary_path.exists() {
        info!(
            binary = %agent_binary_path.display(),
            folder = %agent_folder,
            king   = %king_address,
            "spawning per-agent binary"
        );

        let child = Command::new(&agent_binary_path)
            .current_dir(agent_folder)
            .env("KING_ADDRESS", king_address)
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn per-agent binary '{}' in '{}'",
                    agent_binary_path.display(),
                    agent_folder
                )
            })?;

        return Ok(child);
    }

    // Fallback: generic runner binary
    debug!(
        per_agent = %agent_binary_path.display(),
        "per-agent binary not found, falling back to generic runner"
    );

    info!(
        runner = %runner_binary,
        folder = %agent_folder,
        king   = %king_address,
        "spawning generic runner process"
    );

    let child = Command::new(runner_binary)
        .arg(agent_folder)
        .env("KING_ADDRESS", king_address)
        .spawn()
        .with_context(|| {
            format!("Failed to spawn runner '{runner_binary}' for '{agent_folder}'")
        })?;

    Ok(child)
}

// ─── Agent folder discovery ───────────────────────────────────────────────────

/// Return all kernel agent directories that contain a `soul.md` file.
///
/// Scans `<agents_dir>` for directories matching `evo-kernel-agent-*` with a
/// `soul.md` file inside. This supports the split-repo layout where each kernel
/// agent is an independent repository cloned side-by-side.
pub fn discover_kernel_agents(agents_dir: &str) -> Vec<PathBuf> {
    discover_agents_by_prefix(agents_dir, "evo-kernel-agent-")
}

/// Return all user agent directories that contain a `soul.md` file.
///
/// Scans `<agents_dir>` for directories matching `evo-user-agent-*` with a
/// `soul.md` file inside.
#[allow(dead_code)]
pub fn discover_user_agents(agents_dir: &str) -> Vec<PathBuf> {
    discover_agents_by_prefix(agents_dir, "evo-user-agent-")
}

/// Scan a directory for agent repos matching a name prefix and containing `soul.md`.
fn discover_agents_by_prefix(dir_path: &str, prefix: &str) -> Vec<PathBuf> {
    let dir = PathBuf::from(dir_path);

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), prefix = %prefix, err = %e, "cannot read agents directory");
            return vec![];
        }
    };

    let mut agents: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
        .filter(|e| e.path().join("soul.md").exists())
        .map(|e| e.path())
        .collect();

    agents.sort(); // deterministic ordering
    agents
}

// ─── Orchestration ───────────────────────────────────────────────────────────

/// Discover and spawn all kernel agents.
///
/// Scans `kernel_agents_dir` for `evo-kernel-agent-*` repos with `soul.md`.
/// Returns the list of folder names that were successfully spawned.
pub async fn spawn_all_kernel_agents(
    registry: &AgentRegistry,
    kernel_agents_dir: &str,
    runner_binary: &str,
    king_address: &str,
) -> Vec<String> {
    let agent_dirs = discover_kernel_agents(kernel_agents_dir);

    if agent_dirs.is_empty() {
        warn!(dir = %kernel_agents_dir, "no kernel agents discovered — looking for evo-kernel-agent-* dirs");
        return vec![];
    }

    info!(count = agent_dirs.len(), dir = %kernel_agents_dir, "discovered kernel agent folders");

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
/// alive.
///
/// When a runner exits:
/// 1. The crash is recorded in the DB.
/// 2. King attempts to respawn with exponential backoff (up to 3 retries).
/// 3. If all retries fail the agent is marked `"failed"` in the DB and the
///    operator must intervene.
pub async fn monitor_processes(
    registry: Arc<AgentRegistry>,
    db: Arc<Database>,
    runner_binary: String,
    king_address: String,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;

        // ── Check for exited processes ──────────────────────────────────────
        let exited: Vec<(String, String)> = {
            let mut map = registry.processes.lock().await;
            let mut ex = Vec::new();

            for (key, proc) in map.iter_mut() {
                match proc.child.try_wait() {
                    Ok(Some(status)) => {
                        warn!(
                            agent = %key,
                            pid = proc.pid,
                            exit_code = ?status.code(),
                            "runner process exited"
                        );
                        ex.push((key.clone(), proc.agent_folder.clone()));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(agent = %key, err = %e, "failed to check runner process status");
                    }
                }
            }

            // Remove exited entries while holding the lock
            for (key, _) in &ex {
                map.remove(key);
            }
            ex
        };

        // ── Respawn with exponential backoff ────────────────────────────────
        for (key, folder) in exited {
            // Mark as crashed first
            if let Err(e) = task_db::mark_agent_crashed_by_role(&db, &key).await {
                warn!(agent = %key, err = %e, "failed to update crashed agent status in DB");
            } else {
                info!(agent = %key, "agent marked as crashed in DB — attempting respawn");
            }

            let mut respawned = false;

            for attempt in 1..=MAX_RESPAWN_ATTEMPTS {
                let delay = Duration::from_secs(RESPAWN_BASE_DELAY_SECS * u64::from(attempt));
                warn!(agent = %key, attempt, delay_secs = delay.as_secs(), "respawn attempt");
                tokio::time::sleep(delay).await;

                match spawn_runner(&runner_binary, &folder, &king_address).await {
                    Ok(child) => {
                        let pid = child.id().unwrap_or(0);
                        info!(agent = %key, pid, attempt, "respawned successfully");
                        registry
                            .register(
                                &key,
                                AgentProcess {
                                    pid,
                                    child,
                                    agent_folder: folder.clone(),
                                },
                            )
                            .await;
                        respawned = true;
                        break;
                    }
                    Err(e) => {
                        warn!(agent = %key, attempt, err = %e, "respawn attempt failed");
                    }
                }
            }

            if !respawned {
                let err_msg = format!("all {MAX_RESPAWN_ATTEMPTS} respawn attempts failed");
                error!(agent = %key, "{err_msg}");
                if let Err(e) = task_db::mark_agent_failed_by_role(&db, &key, &err_msg).await {
                    error!(agent = %key, err = %e, "failed to mark agent as failed in DB");
                }
            }
        }
    }
}

// ─── Heartbeat watcher ────────────────────────────────────────────────────────

/// Background task that monitors expected kernel agents by their heartbeat
/// timestamps rather than process exit codes.
///
/// Runs every 60 seconds.  For each expected kernel role, queries the
/// `agent_status` table and:
/// - Warns if no record exists (agent never registered).
/// - Attempts respawn via the process registry if the heartbeat is stale
///   (> `HEARTBEAT_SILENCE_THRESHOLD_SECS` seconds old).
pub async fn heartbeat_watch_loop(
    db: Arc<Database>,
    registry: Arc<AgentRegistry>,
    runner_binary: String,
    king_address: String,
    kernel_agents_dir: String,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(HEARTBEAT_CHECK_INTERVAL_SECS)).await;

        let agents = match task_db::list_agents(&db).await {
            Ok(a) => a,
            Err(e) => {
                error!(err = %e, "heartbeat_watch_loop: failed to list agents");
                continue;
            }
        };

        let now = chrono::Utc::now();

        for &role in EXPECTED_KERNEL_ROLES {
            // Find the agent record(s) matching this role
            let record = agents.iter().find(|a| a.role.contains(role));

            match record {
                None => {
                    warn!(role = %role, "expected kernel agent has no DB record — may not have started yet");
                }
                Some(agent) => {
                    // Check if heartbeat is stale
                    let last_hb = match chrono::DateTime::parse_from_rfc3339(&agent.last_heartbeat)
                    {
                        Ok(ts) => ts.with_timezone(&chrono::Utc),
                        Err(_) => {
                            warn!(role = %role, heartbeat = %agent.last_heartbeat, "cannot parse heartbeat timestamp");
                            continue;
                        }
                    };

                    let elapsed_secs = (now - last_hb).num_seconds();

                    if elapsed_secs <= HEARTBEAT_SILENCE_THRESHOLD_SECS {
                        // Healthy — nothing to do
                        continue;
                    }

                    warn!(
                        role = %role,
                        agent_id = %agent.agent_id,
                        elapsed_secs,
                        threshold = HEARTBEAT_SILENCE_THRESHOLD_SECS,
                        "kernel agent has not sent heartbeat — checking process"
                    );

                    // If the process is already in the registry it might just be slow
                    if registry.pid_by_folder_hint(role).await.is_some() {
                        info!(role = %role, "process still in registry; waiting for next cycle");
                        continue;
                    }

                    // Process missing — find its folder and attempt a spawn
                    let agent_dirs = discover_kernel_agents(&kernel_agents_dir);
                    let folder = agent_dirs
                        .iter()
                        .find(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.contains(role))
                                .unwrap_or(false)
                        })
                        .map(|p| p.to_string_lossy().into_owned());

                    let Some(folder_str) = folder else {
                        error!(role = %role, dir = %kernel_agents_dir, "cannot find agent folder for missing role");
                        continue;
                    };

                    let folder_name = PathBuf::from(&folder_str)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(role)
                        .to_string();

                    match spawn_runner(&runner_binary, &folder_str, &king_address).await {
                        Ok(child) => {
                            let pid = child.id().unwrap_or(0);
                            info!(role = %role, pid, "respawned missing kernel agent via heartbeat watcher");
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
                        }
                        Err(e) => {
                            error!(role = %role, err = %e, "heartbeat watcher failed to respawn agent");
                            let _ = task_db::mark_agent_failed_by_role(
                                &db,
                                role,
                                &format!("heartbeat watcher respawn failed: {e}"),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }
}
