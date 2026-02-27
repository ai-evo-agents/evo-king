//! Doctor — validates and optionally fixes the `~/.evo-agents/` installation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// All expected kernel agent folder names.
pub(crate) const KERNEL_AGENTS: &[(&str, &str)] = &[
    ("evo-kernel-agent-learning", "evo-agent-learning"),
    ("evo-kernel-agent-building", "evo-agent-building"),
    ("evo-kernel-agent-pre-load", "evo-agent-pre-load"),
    ("evo-kernel-agent-evaluation", "evo-agent-evaluation"),
    ("evo-kernel-agent-skill-manage", "evo-agent-skill-manage"),
    ("evo-kernel-agent-update", "evo-agent-update"),
];

/// Result of a single check.
#[derive(Debug)]
struct CheckResult {
    name: String,
    passed: bool,
    message: String,
    #[allow(dead_code)]
    fixable: bool,
}

/// Run the doctor command.
pub async fn run_doctor(evo_home: &str, fix: bool) -> Result<()> {
    let home = PathBuf::from(shellexpand(evo_home));
    println!("evo-king doctor");
    println!("  EVO_HOME: {}", home.display());
    println!("  Fix mode: {fix}");
    println!();

    let mut results = Vec::new();

    // ── Directory structure ──────────────────────────────────────────────
    check_dir(&home, fix, &mut results);
    check_dir(&home.join("bin"), fix, &mut results);
    check_dir(&home.join("data"), fix, &mut results);
    check_dir(&home.join("data/backups"), fix, &mut results);
    check_dir(&home.join("logs"), fix, &mut results);
    check_dir(&home.join("repos"), fix, &mut results);

    // ── Service binaries ────────────────────────────────────────────────
    check_binary(&home.join("bin/evo-king"), &mut results);
    check_binary(&home.join("bin/evo-gateway"), &mut results);

    // ── Kernel agent folders ────────────────────────────────────────────
    for &(folder, binary) in KERNEL_AGENTS {
        let agent_dir = home.join(folder);
        check_dir(&agent_dir, fix, &mut results);
        check_binary(&agent_dir.join(binary), &mut results);
        check_file(&agent_dir.join("soul.md"), &mut results);
        check_dir(&agent_dir.join("skills"), fix, &mut results);
    }

    // ── Config files ────────────────────────────────────────────────────
    check_file(&home.join("data/gateway.json"), &mut results);
    check_file(&home.join("repos.json"), &mut results);

    // ── Source repos ────────────────────────────────────────────────────
    let repo_names = [
        "evo-common",
        "evo-king",
        "evo-gateway",
        "evo-agents",
        "evo-kernel-agent-learning",
        "evo-kernel-agent-building",
        "evo-kernel-agent-pre-load",
        "evo-kernel-agent-evaluation",
        "evo-kernel-agent-skill-manage",
        "evo-kernel-agent-update",
    ];
    for name in &repo_names {
        let repo_dir = home.join("repos").join(name);
        if repo_dir.exists() {
            results.push(CheckResult {
                name: format!("repos/{name}"),
                passed: true,
                message: "exists".into(),
                fixable: false,
            });
        } else {
            let fixed = if fix {
                clone_repo(name, &repo_dir)
            } else {
                false
            };
            results.push(CheckResult {
                name: format!("repos/{name}"),
                passed: fixed,
                message: if fixed {
                    "cloned".into()
                } else {
                    "missing (run with --fix to clone)".into()
                },
                fixable: true,
            });
        }
    }

    // ── Gateway connectivity ────────────────────────────────────────────
    let gw_ok = check_gateway_health().await;
    results.push(CheckResult {
        name: "gateway:3301/health".into(),
        passed: gw_ok,
        message: if gw_ok {
            "reachable".into()
        } else {
            "not reachable (start gateway first)".into()
        },
        fixable: false,
    });

    // ── Print results ───────────────────────────────────────────────────
    println!();
    let mut pass_count = 0;
    let mut fail_count = 0;

    for r in &results {
        let icon = if r.passed { "\u{2714}" } else { "\u{2718}" };
        let status = if r.passed { "OK" } else { "FAIL" };
        println!("  {icon} [{status:>4}] {:<45} {}", r.name, r.message);
        if r.passed {
            pass_count += 1;
        } else {
            fail_count += 1;
        }
    }

    println!();
    println!(
        "  {pass_count} passed, {fail_count} failed (total: {})",
        results.len()
    );

    if fail_count > 0 && !fix {
        println!();
        println!("  Run with --fix to attempt automatic repairs.");
    }

    if fail_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn shellexpand(s: &str) -> String {
    if s.starts_with("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &s[1..]);
    }
    s.to_string()
}

fn check_dir(path: &Path, fix: bool, results: &mut Vec<CheckResult>) {
    let name = path.display().to_string();
    if path.is_dir() {
        results.push(CheckResult {
            name,
            passed: true,
            message: "exists".into(),
            fixable: false,
        });
    } else if fix {
        let ok = std::fs::create_dir_all(path).is_ok();
        results.push(CheckResult {
            name,
            passed: ok,
            message: if ok {
                "created".into()
            } else {
                "failed to create".into()
            },
            fixable: true,
        });
    } else {
        results.push(CheckResult {
            name,
            passed: false,
            message: "missing".into(),
            fixable: true,
        });
    }
}

fn check_binary(path: &Path, results: &mut Vec<CheckResult>) {
    let name = path.display().to_string();
    if path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let is_exec = path
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            results.push(CheckResult {
                name,
                passed: is_exec,
                message: if is_exec {
                    "executable".into()
                } else {
                    "exists but not executable".into()
                },
                fixable: false,
            });
        }
        #[cfg(not(unix))]
        {
            results.push(CheckResult {
                name,
                passed: true,
                message: "exists".into(),
                fixable: false,
            });
        }
    } else {
        results.push(CheckResult {
            name,
            passed: false,
            message: "missing (run install.sh to download)".into(),
            fixable: false,
        });
    }
}

fn check_file(path: &Path, results: &mut Vec<CheckResult>) {
    let name = path.display().to_string();
    results.push(CheckResult {
        name,
        passed: path.exists(),
        message: if path.exists() {
            "exists".into()
        } else {
            "missing".into()
        },
        fixable: false,
    });
}

fn clone_repo(name: &str, dest: &Path) -> bool {
    let url = format!("https://github.com/ai-evo-agents/{name}.git");
    println!("  Cloning {url} ...");
    Command::new("git")
        .args(["clone", "--depth", "1", &url, &dest.to_string_lossy()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn check_gateway_health() -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build();

    match client {
        Ok(c) => c
            .get("http://localhost:3301/health")
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        Err(_) => false,
    }
}
