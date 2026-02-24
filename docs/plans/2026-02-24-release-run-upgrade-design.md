# Evo System: Release + Run Script + Self-Upgrade Pipeline

## Context

The evo system is a multi-repo polyrepo (11 repos under `ai-evo-agents` GitHub org). Currently there is no unified way to install, run, or self-upgrade the system. King uses a generic runner binary. Release workflows exist but lack bundling of soul.md/skills. No orchestration script exists.

This plan covers:
1. Publish new releases for ALL repos via `gh`
2. Create `~/.evo-agents/` installation directory with `repos.json`
3. `install.sh` — bootstrap script (download binaries, clone repos)
4. `run.sh` — start the full system
5. Doctor command — validate and fix setup
6. Release workflow changes — bundle soul.md + skills in archives
7. Self-upgrade pipeline — building→pre-load→evaluation→update

---

## 1. Release Publishing

### Version Bumps

| Repo | Current | New |
|------|---------|-----|
| `evo-common` | 0.5.0 | 0.6.0 |
| `evo-agent-sdk` (in evo-agents) | 0.2.0 | 0.3.0 |
| `evo-runner` (in evo-agents) | 0.1.0 | 0.2.0 |
| `evo-gateway` | 0.2.0 | 0.3.0 |
| `evo-king` | 0.1.0 | 0.2.0 |
| All 6 kernel agents | 0.1.0 | 0.2.0 |
| `evo-user-agent-template` | 0.1.0 | 0.2.0 |

### Dependency Chain (publish order)

```
1. evo-common (cargo publish → crates.io 0.6.0)
2. evo-agents (cargo publish evo-agent-sdk 0.3.0 → crates.io, then tag v0.2.0)
3. evo-gateway (update evo-common dep to 0.6, tag v0.3.0)
4. evo-king (update evo-common dep to 0.6, tag v0.2.0)
5. All kernel agents + user-template (update evo-agent-sdk dep to 0.3 in sed, tag v0.2.0)
```

### Release Script

A `release-all.sh` script at the evo root that:
1. For each repo in dependency order:
   - `cd` into repo
   - Bump version in `Cargo.toml`
   - Commit: `release: vX.Y.Z`
   - `git tag -a vX.Y.Z -m "Release vX.Y.Z"`
   - `git push origin main --tags`
2. For crates.io publishes (evo-common, evo-agent-sdk):
   - `cargo publish` before tagging downstream
3. Wait for GitHub Actions to build release artifacts

Alternatively, use `gh release create` directly if we want to skip waiting for CI and upload local builds.

---

## 2. `~/.evo-agents/` Directory Structure

```
~/.evo-agents/
├── repos.json                          # Registry
├── run.sh                              # Startup script
├── install.sh                          # Kept for re-installs
├── bin/
│   ├── evo-king
│   ├── evo-gateway
│   └── evo-gateway-cli
├── evo-kernel-agent-learning/
│   ├── evo-agent-learning              # Binary
│   ├── soul.md
│   └── skills/
├── evo-kernel-agent-building/
│   ├── evo-agent-building
│   ├── soul.md
│   └── skills/
├── evo-kernel-agent-pre-load/
│   └── ...
├── evo-kernel-agent-evaluation/
│   └── ...
├── evo-kernel-agent-skill-manage/
│   └── ...
├── evo-kernel-agent-update/
│   └── ...
├── data/
│   ├── king.db
│   ├── gateway.db
│   ├── gateway.json
│   └── backups/
├── logs/
└── repos/                              # Git clones for source
    ├── evo-common/
    ├── evo-king/
    ├── evo-gateway/
    ├── evo-agents/
    └── evo-kernel-agent-*/
```

### repos.json Format

```json
{
  "version": "0.2.0",
  "repos": {
    "evo-common": {
      "github": "ai-evo-agents/evo-common",
      "local_path": "~/.evo-agents/repos/evo-common",
      "installed_version": "v0.6.0",
      "type": "library"
    },
    "evo-king": {
      "github": "ai-evo-agents/evo-king",
      "local_path": "~/.evo-agents/repos/evo-king",
      "installed_version": "v0.2.0",
      "binary_path": "~/.evo-agents/bin/evo-king",
      "type": "service"
    },
    "evo-gateway": {
      "github": "ai-evo-agents/evo-gateway",
      "local_path": "~/.evo-agents/repos/evo-gateway",
      "installed_version": "v0.3.0",
      "binary_path": "~/.evo-agents/bin/evo-gateway",
      "type": "service"
    },
    "evo-kernel-agent-learning": {
      "github": "ai-evo-agents/evo-kernel-agent-learning",
      "local_path": "~/.evo-agents/repos/evo-kernel-agent-learning",
      "installed_version": "v0.2.0",
      "binary_path": "~/.evo-agents/evo-kernel-agent-learning/evo-agent-learning",
      "type": "kernel-agent"
    }
  }
}
```

---

## 3. install.sh — Bootstrap Script

Lives at: `evo-king/install.sh` (also downloadable via curl one-liner)

```
curl -fsSL https://raw.githubusercontent.com/ai-evo-agents/evo-king/main/install.sh | bash
```

### What it does:

1. **Detect platform** (Linux x86_64/ARM64, macOS Intel/Apple Silicon, Windows)
2. **Create `~/.evo-agents/`** directory structure (bin/, data/, logs/, repos/)
3. **Download binaries** from GitHub Releases:
   - `evo-king` from `ai-evo-agents/evo-king/releases/latest`
   - `evo-gateway` from `ai-evo-agents/evo-gateway/releases/latest`
   - Each kernel agent from their respective repos (archives include binary + soul.md + skills/)
4. **Extract agent archives** into `~/.evo-agents/evo-kernel-agent-*/`
5. **Clone repos** (for source/self-upgrade):
   - `git clone` all 11 repos into `~/.evo-agents/repos/`
6. **Generate `repos.json`** with paths and installed versions
7. **Generate `run.sh`** (or copy from king release)
8. **Write default `gateway.json`** into `data/`
9. **Run doctor** to validate everything
10. **Print instructions**: `Run: ~/.evo-agents/run.sh`

---

## 4. run.sh — Startup Script

Lives at: `~/.evo-agents/run.sh`

### What it does:

```bash
#!/usr/bin/env bash
set -euo pipefail

EVO_HOME="${HOME}/.evo-agents"

# 1. Run doctor check (fix mode)
"${EVO_HOME}/bin/evo-king" doctor --fix --evo-home "${EVO_HOME}"

# 2. Start gateway in background
"${EVO_HOME}/bin/evo-gateway" &
GATEWAY_PID=$!
echo "Gateway started (PID: ${GATEWAY_PID})"

# 3. Wait for gateway health
for i in {1..30}; do
  curl -sf http://localhost:8080/health > /dev/null 2>&1 && break
  sleep 1
done

# 4. Start king (auto-discovers and spawns agents)
KERNEL_AGENTS_DIR="${EVO_HOME}" \
EVO_KING_DB_PATH="${EVO_HOME}/data/king.db" \
EVO_LOG_DIR="${EVO_HOME}/logs" \
GATEWAY_CONFIG="${EVO_HOME}/data/gateway.json" \
"${EVO_HOME}/bin/evo-king"
```

**Key**: King's `KERNEL_AGENTS_DIR` points to `~/.evo-agents/` so it scans for `evo-kernel-agent-*` folders there.

**King agent spawning change**: Instead of using a single `RUNNER_BINARY`, king now looks for the per-agent binary inside each agent folder. For `evo-kernel-agent-learning/`, it runs `./evo-agent-learning` (the binary right next to soul.md). This requires a small code change in `agent_manager.rs`.

---

## 5. Doctor Command

Add a `doctor` subcommand to `evo-king` (since king already has clap CLI):

```
evo-king doctor [--fix] [--evo-home ~/.evo-agents]
```

### Checks:

| Check | Fix action |
|-------|-----------|
| `~/.evo-agents/` exists | Create it |
| `bin/evo-king` exists and is executable | Re-download from GitHub |
| `bin/evo-gateway` exists and is executable | Re-download from GitHub |
| Each `evo-kernel-agent-*/` folder exists | Re-download from GitHub |
| Each agent has binary + soul.md | Re-download release archive |
| Each agent's skills/ folder exists | Create empty |
| `data/` directory exists | Create it |
| `data/gateway.json` exists | Write default config |
| `repos.json` exists and is valid | Regenerate |
| `repos/` clones exist | `git clone` missing ones |
| Gateway reachable on :8080 | Report (no auto-fix) |

### Implementation:

Add to `evo-king/src/main.rs`:
```rust
enum Commands {
    Auth { ... },
    Doctor {
        #[arg(long)]
        fix: bool,
        #[arg(long, default_value = "~/.evo-agents")]
        evo_home: String,
    },
}
```

Doctor logic in a new `src/doctor.rs` module.

---

## 6. Release Workflow Changes — Bundle soul.md + skills

Modify each kernel agent's `.github/workflows/release.yml` to include soul.md + skills/ in the release archive.

### Current packaging step (all agents):
```yaml
- name: Package
  run: |
    tar czf $ARCHIVE_NAME -C target/${{ matrix.target }}/release $BINARY_NAME
```

### New packaging step:
```yaml
- name: Package
  run: |
    mkdir -p staging/$AGENT_FOLDER_NAME
    cp target/${{ matrix.target }}/release/$BINARY_NAME staging/$AGENT_FOLDER_NAME/
    cp soul.md staging/$AGENT_FOLDER_NAME/
    cp -r skills/ staging/$AGENT_FOLDER_NAME/ 2>/dev/null || true
    tar czf $ARCHIVE_NAME -C staging $AGENT_FOLDER_NAME
```

This way extracting the archive gives you: `evo-kernel-agent-learning/evo-agent-learning` + `soul.md` + `skills/`

Also update evo-king and evo-gateway release workflows to include config files:
- evo-king: bundle `install.sh`, `run.sh` template, `update.sh`
- evo-gateway: bundle default `gateway.json`

---

## 7. King Agent Manager Changes

### Current behavior (agent_manager.rs):
```rust
// Spawns: <runner_binary> <agent_folder>
Command::new(runner_binary).arg(agent_folder)
```

### New behavior:
```rust
// Look for per-agent binary inside the agent folder
// Binary name = folder name with "evo-kernel-" → "evo-" substitution
let binary_name = agent_folder_name.replace("evo-kernel-agent-", "evo-agent-");
let binary_path = agent_folder.join(&binary_name);

if binary_path.exists() {
    // New: spawn per-agent binary (no args needed, it knows its own folder)
    Command::new(&binary_path)
        .current_dir(agent_folder)
        .env("KING_ADDRESS", king_address)
} else {
    // Fallback: generic runner (backward compat)
    Command::new(runner_binary).arg(agent_folder)
        .env("KING_ADDRESS", king_address)
}
```

Also update `AgentRunner::run()` in evo-agent-sdk to detect its own folder from `current_dir` when no CLI arg is given.

---

## 8. Self-Upgrade Pipeline

The existing 5-stage pipeline (learning→building→pre-load→evaluation→skill-manage) gets the "update" stage. The building agent gains real CI/CD capabilities.

### 8a. Building Agent — Build & Release

When building agent receives a pipeline task to build a new version of an agent:

1. Read `~/.evo-agents/repos.json` to find the source repo path
2. `cd` into `repos/<agent-name>/`
3. `git pull origin main`
4. Bump version in `Cargo.toml`
5. `cargo build --release`
6. Package: binary + soul.md + skills/ into `.tar.gz`
7. `gh release create v<new> --repo ai-evo-agents/<repo> --notes "Auto-release" <archive>`
8. Report success + new version to king via `pipeline:stage_result`

### 8b. Pre-Load Agent — Validate New Release

1. Download the new release archive from GitHub
2. Extract to a temp directory
3. Run doctor checks on the extracted folder (binary exists, soul.md valid, skills present)
4. Spawn the new binary in a sandbox and hit its health endpoint
5. Report pass/fail to king

### 8c. Evaluation Agent — Final Check

1. Compare new version output against baseline (if applicable)
2. Run any agent-specific test scenarios
3. Score the release
4. Report pass/fail + score to king

### 8d. King — update.sh

Lives at: `~/.evo-agents/data/update.sh` (also `evo-king/update.sh` in source)

```bash
#!/usr/bin/env bash
# Usage: update.sh <component> <new-version>
# Example: update.sh evo-kernel-agent-learning v0.2.1
# Example: update.sh evo-king v0.2.1

EVO_HOME="${HOME}/.evo-agents"
COMPONENT="$1"
VERSION="$2"

# 1. Backup current version
if [[ "$COMPONENT" == "evo-king" || "$COMPONENT" == "evo-gateway" ]]; then
    cp "${EVO_HOME}/bin/${COMPONENT}" "${EVO_HOME}/bin/${COMPONENT}.backup"
else
    cp -r "${EVO_HOME}/${COMPONENT}" "${EVO_HOME}/${COMPONENT}.backup"
fi

# 2. Download new version
# (platform detection + download from GitHub Releases)

# 3. Extract / install
# For agents: extract archive into agent folder
# For king/gateway: replace binary in bin/

# 4. Update repos.json with new version

# 5. Run doctor to validate

# 6. If doctor fails, rollback from backup
```

King calls update.sh via `std::process::Command` when the pipeline completes successfully.

For **king self-update**: king writes a small updater script, spawns it, and exits. The updater replaces the king binary, then restarts king. (Same pattern as many auto-updaters.)

---

## 9. Implementation Order

Due to dependencies, implement in this order:

### Phase A: Release Workflow Changes + Publish (immediate)
1. Modify all kernel agent `release.yml` to bundle soul.md + skills/
2. Modify evo-king + evo-gateway `release.yml` to bundle config files
3. Bump versions in all Cargo.toml files
4. Update dependency versions (evo-common dep in evo-king, evo-gateway; evo-agent-sdk dep in agents)
5. Publish evo-common to crates.io
6. Publish evo-agent-sdk to crates.io
7. Tag + push all repos → triggers GitHub Actions release builds

### Phase B: King Changes
1. Add `doctor` subcommand to evo-king CLI
2. Implement `src/doctor.rs`
3. Modify `agent_manager.rs` to use per-agent binaries
4. Create `install.sh` in evo-king repo
5. Create `run.sh` template in evo-king repo
6. Create `update.sh` in evo-king repo

### Phase C: Agent Pipeline Handlers
1. Update building handler — read repos.json, cargo build, gh release
2. Update pre-load handler — download + doctor + health check
3. Update evaluation handler — final validation
4. Wire update.sh into king's pipeline completion flow

### Phase D: Testing
1. Run install.sh on clean machine
2. Run run.sh → verify full system starts
3. Trigger a pipeline run → verify building→pre-load→evaluation→update flow
4. Test rollback on failed update

---

## Files to Create/Modify

### New files:
- `evo-king/install.sh` — bootstrap installer
- `evo-king/run.sh` — startup script template
- `evo-king/update.sh` — update script
- `evo-king/src/doctor.rs` — doctor validation logic
- Root `release-all.sh` — publish all repos script

### Modified files:
- `evo-king/src/main.rs` — add Doctor subcommand, update agent spawning
- `evo-king/src/agent_manager.rs` — per-agent binary spawning
- All `evo-kernel-agent-*/.github/workflows/release.yml` — bundle soul.md + skills
- `evo-king/.github/workflows/release.yml` — bundle scripts
- `evo-gateway/.github/workflows/release.yml` — bundle gateway.json
- All `Cargo.toml` files — version bumps
- `evo-agents/publish.sh` — update for new version scheme

### Agent handler changes (in evo-agent-sdk or individual agents):
- `evo-kernel-agent-building/src/main.rs` — real build+release logic
- `evo-kernel-agent-pre-load/src/main.rs` — download+doctor+health logic
- `evo-kernel-agent-evaluation/src/main.rs` — final validation logic

---

## Verification

```bash
# After Phase A:
gh release list --repo ai-evo-agents/evo-king    # should show v0.2.0
gh release list --repo ai-evo-agents/evo-gateway  # should show v0.3.0

# After Phase B:
~/.evo-agents/bin/evo-king doctor --evo-home ~/.evo-agents  # all checks pass
~/.evo-agents/run.sh  # full system starts, all agents connect

# After Phase C:
curl -X POST http://localhost:3000/pipeline/start  # triggers pipeline
# Monitor: building → pre-load → evaluation → update
curl http://localhost:3000/pipeline/runs            # shows completed run
```
