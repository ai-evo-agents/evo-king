//! System discovery — probes CLI tools, system info, and evo components at startup.
//!
//! Results are persisted to `~/.evo-agents/data/system_info.json` and cached in
//! [`KingState`] for sharing with agents via Socket.IO and REST.

use crate::doctor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::info;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SYSTEM_INFO_FILENAME: &str = "system_info.json";

// ─── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Available,
    NotFound,
    Suggested,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredTool {
    pub name: String,
    pub version: Option<String>,
    pub path: Option<String>,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion_reason: Option<String>,
    pub category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub hostname: String,
    pub cpu_count: usize,
    pub memory_bytes: Option<u64>,
    pub home_dir: String,
    pub evo_home: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvoComponent {
    pub name: String,
    pub binary_path: Option<String>,
    pub version: Option<String>,
    pub found: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistStatus {
    Pass,
    Fail,
    Suggested,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub name: String,
    pub status: ChecklistStatus,
    pub message: String,
}

/// Top-level discovery result saved to disk and shared with agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDiscovery {
    pub discovered_at: DateTime<Utc>,
    pub king_version: String,
    pub system: SystemInfo,
    pub tools: Vec<DiscoveredTool>,
    pub evo_components: Vec<EvoComponent>,
    pub checklist: Vec<ChecklistItem>,
}

// ─── Tool probe definitions ──────────────────────────────────────────────────

struct ToolProbe {
    name: &'static str,
    command: &'static str,
    version_args: &'static [&'static str],
    category: &'static str,
    suggested: bool,
    suggestion_reason: &'static str,
}

static TOOL_PROBES: &[ToolProbe] = &[
    // Rust toolchain
    ToolProbe {
        name: "rustc",
        command: "rustc",
        version_args: &["--version"],
        category: "rust_toolchain",
        suggested: true,
        suggestion_reason: "Required for building evo agents",
    },
    ToolProbe {
        name: "cargo",
        command: "cargo",
        version_args: &["--version"],
        category: "rust_toolchain",
        suggested: true,
        suggestion_reason: "Required for building evo agents",
    },
    ToolProbe {
        name: "rustup",
        command: "rustup",
        version_args: &["--version"],
        category: "rust_toolchain",
        suggested: true,
        suggestion_reason: "Recommended for Rust toolchain management",
    },
    ToolProbe {
        name: "cargo-clippy",
        command: "cargo",
        version_args: &["clippy", "--version"],
        category: "rust_toolchain",
        suggested: false,
        suggestion_reason: "",
    },
    ToolProbe {
        name: "cargo-fmt",
        command: "rustfmt",
        version_args: &["--version"],
        category: "rust_toolchain",
        suggested: false,
        suggestion_reason: "",
    },
    // Node/JS
    ToolProbe {
        name: "node",
        command: "node",
        version_args: &["--version"],
        category: "node_js",
        suggested: true,
        suggestion_reason: "Required for evo-dashboard",
    },
    ToolProbe {
        name: "npm",
        command: "npm",
        version_args: &["--version"],
        category: "node_js",
        suggested: true,
        suggestion_reason: "Required for evo-dashboard",
    },
    ToolProbe {
        name: "npx",
        command: "npx",
        version_args: &["--version"],
        category: "node_js",
        suggested: false,
        suggestion_reason: "",
    },
    ToolProbe {
        name: "bun",
        command: "bun",
        version_args: &["--version"],
        category: "node_js",
        suggested: false,
        suggestion_reason: "",
    },
    // System tools
    ToolProbe {
        name: "git",
        command: "git",
        version_args: &["--version"],
        category: "system",
        suggested: true,
        suggestion_reason: "Required for repo cloning and updates",
    },
    ToolProbe {
        name: "rg",
        command: "rg",
        version_args: &["--version"],
        category: "system",
        suggested: true,
        suggestion_reason: "Required for fast code search in agents",
    },
    ToolProbe {
        name: "curl",
        command: "curl",
        version_args: &["--version"],
        category: "system",
        suggested: false,
        suggestion_reason: "",
    },
    ToolProbe {
        name: "docker",
        command: "docker",
        version_args: &["--version"],
        category: "system",
        suggested: false,
        suggestion_reason: "",
    },
    ToolProbe {
        name: "python3",
        command: "python3",
        version_args: &["--version"],
        category: "system",
        suggested: false,
        suggestion_reason: "",
    },
];

// ─── Probing logic ───────────────────────────────────────────────────────────

/// Run full system discovery.
pub async fn run_discovery() -> SystemDiscovery {
    let system = gather_system_info().await;
    let tools = probe_all_tools().await;
    let evo_components = discover_evo_components(&system.evo_home).await;
    let checklist = build_checklist(&system, &tools, &evo_components);

    let counts = checklist
        .iter()
        .fold((0, 0, 0), |acc, item| match item.status {
            ChecklistStatus::Pass => (acc.0 + 1, acc.1, acc.2),
            ChecklistStatus::Fail => (acc.0, acc.1 + 1, acc.2),
            ChecklistStatus::Suggested => (acc.0, acc.1, acc.2 + 1),
        });
    info!(
        pass = counts.0,
        fail = counts.1,
        suggested = counts.2,
        tools = tools.len(),
        "system discovery complete"
    );

    SystemDiscovery {
        discovered_at: Utc::now(),
        king_version: env!("CARGO_PKG_VERSION").to_string(),
        system,
        tools,
        evo_components,
        checklist,
    }
}

/// Probe a single CLI tool.
async fn probe_tool(probe: &ToolProbe) -> DiscoveredTool {
    let result = tokio::time::timeout(PROBE_TIMEOUT, async {
        tokio::process::Command::new(probe.command)
            .args(probe.version_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
    })
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Some tools (e.g. rustfmt) print to stderr
            let raw = if stdout.trim().is_empty() {
                stderr.to_string()
            } else {
                stdout.to_string()
            };
            let version = parse_version_string(&raw);
            let path = which_path(probe.command).await;
            DiscoveredTool {
                name: probe.name.to_string(),
                version: Some(version),
                path,
                status: ToolStatus::Available,
                suggestion_reason: None,
                category: probe.category.to_string(),
            }
        }
        _ => {
            let status = if probe.suggested {
                ToolStatus::Suggested
            } else {
                ToolStatus::NotFound
            };
            DiscoveredTool {
                name: probe.name.to_string(),
                version: None,
                path: None,
                status,
                suggestion_reason: if probe.suggested {
                    Some(probe.suggestion_reason.to_string())
                } else {
                    None
                },
                category: probe.category.to_string(),
            }
        }
    }
}

/// Run all tool probes in parallel.
async fn probe_all_tools() -> Vec<DiscoveredTool> {
    let mut set = JoinSet::new();
    for (i, _) in TOOL_PROBES.iter().enumerate() {
        set.spawn(async move { probe_tool(&TOOL_PROBES[i]).await });
    }

    let mut results = Vec::with_capacity(TOOL_PROBES.len());
    while let Some(Ok(tool)) = set.join_next().await {
        results.push(tool);
    }
    results.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    results
}

/// Parse a version string from typical `--version` output.
///
/// Handles: `rustc 1.82.0 (hash)`, `node v20.11.0`, `git version 2.43.0`,
/// `npm 10.2.4`, `Docker version 24.0.7, build afdd53b`, etc.
fn parse_version_string(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or(raw).trim();
    first_line
        .split_whitespace()
        .find(|word| {
            let w = word.strip_prefix('v').unwrap_or(word);
            // Strip trailing comma for cases like "Docker version 24.0.7,"
            let w = w.trim_end_matches(',');
            w.chars().next().is_some_and(|c| c.is_ascii_digit()) && w.contains('.')
        })
        .map(|w| {
            let w = w.strip_prefix('v').unwrap_or(w);
            w.trim_end_matches(',').to_string()
        })
        .unwrap_or_else(|| first_line.to_string())
}

/// Resolve the full path of a command via `which`.
async fn which_path(command: &str) -> Option<String> {
    let output = tokio::process::Command::new("which")
        .arg(command)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

// ─── System info ─────────────────────────────────────────────────────────────

async fn gather_system_info() -> SystemInfo {
    let home_dir = std::env::var("HOME").unwrap_or_default();
    let evo_home = std::env::var("EVO_HOME").unwrap_or_else(|_| format!("{home_dir}/.evo-agents"));

    SystemInfo {
        os: std::env::consts::OS.to_string(),
        os_version: get_os_version().await,
        arch: std::env::consts::ARCH.to_string(),
        hostname: get_hostname().await,
        cpu_count: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        memory_bytes: get_total_memory().await,
        home_dir,
        evo_home,
    }
}

async fn get_hostname() -> String {
    tokio::process::Command::new("hostname")
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

async fn get_os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        tokio::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .await
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
    #[cfg(target_os = "linux")]
    {
        tokio::fs::read_to_string("/etc/os-release")
            .await
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("PRETTY_NAME=")).map(|l| {
                    l.trim_start_matches("PRETTY_NAME=")
                        .trim_matches('"')
                        .to_string()
                })
            })
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unknown".to_string()
    }
}

async fn get_total_memory() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        tokio::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .await
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8_lossy(&o.stdout).trim().parse().ok()
                } else {
                    None
                }
            })
    }
    #[cfg(target_os = "linux")]
    {
        tokio::fs::read_to_string("/proc/meminfo")
            .await
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

// ─── Evo component discovery ─────────────────────────────────────────────────

async fn discover_evo_components(evo_home: &str) -> Vec<EvoComponent> {
    let mut components = Vec::new();

    // Core service binaries
    for (name, bin_name) in [
        ("evo-king", "evo-king"),
        ("evo-gateway", "evo-gateway"),
        ("evo-runner", "evo-runner"),
    ] {
        let bin_path = format!("{evo_home}/bin/{bin_name}");
        let (found, version) = probe_binary_version(&bin_path).await;
        components.push(EvoComponent {
            name: name.to_string(),
            binary_path: if found { Some(bin_path) } else { None },
            version,
            found,
        });
    }

    // Kernel agent binaries
    for &(folder, binary) in doctor::KERNEL_AGENTS {
        let bin_path = format!("{evo_home}/{folder}/{binary}");
        let (found, version) = probe_binary_version(&bin_path).await;
        components.push(EvoComponent {
            name: folder.to_string(),
            binary_path: if found { Some(bin_path) } else { None },
            version,
            found,
        });
    }

    components
}

async fn probe_binary_version(path: &str) -> (bool, Option<String>) {
    if !Path::new(path).exists() {
        return (false, None);
    }
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(path)
            .arg("--version")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout);
            (true, Some(parse_version_string(&raw)))
        }
        _ => (true, None), // exists but couldn't get version
    }
}

// ─── Checklist generation ────────────────────────────────────────────────────

fn build_checklist(
    system: &SystemInfo,
    tools: &[DiscoveredTool],
    evo_components: &[EvoComponent],
) -> Vec<ChecklistItem> {
    let mut items = Vec::new();

    items.push(ChecklistItem {
        name: "evo_home_exists".into(),
        status: if Path::new(&system.evo_home).is_dir() {
            ChecklistStatus::Pass
        } else {
            ChecklistStatus::Fail
        },
        message: system.evo_home.clone(),
    });

    for tool in tools {
        items.push(ChecklistItem {
            name: format!("tool:{}", tool.name),
            status: match tool.status {
                ToolStatus::Available => ChecklistStatus::Pass,
                ToolStatus::NotFound => ChecklistStatus::Fail,
                ToolStatus::Suggested => ChecklistStatus::Suggested,
            },
            message: tool.version.clone().unwrap_or_else(|| "not found".into()),
        });
    }

    for comp in evo_components {
        items.push(ChecklistItem {
            name: format!("evo:{}", comp.name),
            status: if comp.found {
                ChecklistStatus::Pass
            } else {
                ChecklistStatus::Fail
            },
            message: comp.version.clone().unwrap_or_else(|| {
                if comp.found {
                    "found (version unknown)".into()
                } else {
                    "missing".into()
                }
            }),
        });
    }

    items
}

// ─── File persistence ────────────────────────────────────────────────────────

/// Save discovery results to `{evo_home}/data/system_info.json`.
pub async fn save_to_disk(discovery: &SystemDiscovery) -> anyhow::Result<String> {
    let data_dir = Path::new(&discovery.system.evo_home).join("data");
    tokio::fs::create_dir_all(&data_dir).await?;

    let file_path = data_dir.join(SYSTEM_INFO_FILENAME);
    let json = serde_json::to_string_pretty(discovery)?;
    tokio::fs::write(&file_path, &json).await?;

    Ok(file_path.to_string_lossy().to_string())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_rustc() {
        assert_eq!(
            parse_version_string("rustc 1.76.0 (07dca489a 2024-02-04)"),
            "1.76.0"
        );
    }

    #[test]
    fn parse_version_node() {
        assert_eq!(parse_version_string("v20.11.0"), "20.11.0");
    }

    #[test]
    fn parse_version_git() {
        assert_eq!(parse_version_string("git version 2.43.0"), "2.43.0");
    }

    #[test]
    fn parse_version_npm() {
        assert_eq!(parse_version_string("10.2.4"), "10.2.4");
    }

    #[test]
    fn parse_version_docker() {
        assert_eq!(
            parse_version_string("Docker version 24.0.7, build afdd53b"),
            "24.0.7"
        );
    }

    #[test]
    fn parse_version_plain_text() {
        // When no semver-like token is found, return the first line
        assert_eq!(parse_version_string("some tool"), "some tool");
    }

    #[test]
    fn serialize_tool_status() {
        let status = ToolStatus::Suggested;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""suggested""#);
    }

    #[test]
    fn serialize_checklist_status() {
        let status = ChecklistStatus::Pass;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""pass""#);
    }

    #[tokio::test]
    async fn run_discovery_returns_valid_data() {
        let discovery = run_discovery().await;
        assert!(!discovery.system.os.is_empty());
        assert!(!discovery.tools.is_empty());
        assert!(discovery.system.cpu_count > 0);
    }

    #[tokio::test]
    async fn roundtrip_json_serialization() {
        let discovery = run_discovery().await;
        let json = serde_json::to_string_pretty(&discovery).unwrap();
        let parsed: SystemDiscovery = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.system.os, discovery.system.os);
        assert_eq!(parsed.tools.len(), discovery.tools.len());
    }
}
