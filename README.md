# evo-king

Central orchestrator for the Evo self-evolution agent system. Manages gateway config lifecycle, spawns and monitors agent runner processes, serves a Socket.IO server for bidirectional agent communication, and maintains a local task database for evolution pipeline tracking.

**Status:** Active development — agent management, pipeline coordinator, and task DB operational.

---

## Part of the Evo System

| Crate | Role |
|-------|------|
| evo-common | Shared types (dependency) |
| evo-gateway | API aggregator — king manages its config lifecycle |
| **evo-king** | Central orchestrator, port 3000 |
| evo-agents | Runner binary — runners connect to king via Socket.IO |
| evo-kernel-agent-* | 6 kernel agent repos (learning, building, pre-load, evaluation, skill-manage, update) |
| evo-user-agent-template | Template for creating user agents |

---

## Architecture

```
                    evo-king (port 3000)
                    ┌─────────────────────────────────────────┐
                    │  Socket.IO Server (socketioxide)         │
                    │  ┌──────────────┐ ┌──────────────┐      │
                    │  │Config Watcher │ │Agent Manager │      │
                    │  │  (notify)     │ │(spawn/stop)  │      │
                    │  └──────┬───────┘ └──────┬───────┘      │
                    │         │                │               │
                    │  ┌──────┴────────────────┴───────┐      │
                    │  │     Gateway Manager            │      │
                    │  │  test -> swap -> backup        │      │
                    │  └───────────────────────────────┘      │
                    │  ┌───────────────────────────────┐      │
                    │  │     Turso Local DB (libSQL)    │      │
                    │  │  tasks, pipeline_runs,         │      │
                    │  │  agent_status, config_history  │      │
                    │  └───────────────────────────────┘      │
                    └─────────────────────┬───────────────────┘
                                          │ socket.io
                    ┌─────────────────────┼─────────────┐
                    │                     │             │
               ┌────┴────┐          ┌────┴────┐  ┌────┴────┐
               │ Runner  │          │ Runner  │  │ Runner  │
               │Learning │          │Building │  │  Eval   │
               └─────────┘          └─────────┘  └─────────┘
```

---

## Config Lifecycle

The gateway manager handles the full lifecycle of gateway configuration changes:

1. `config_watcher` detects a change to `gateway.config` via the `notify` crate and emits an internal change event.
2. `gateway_manager` spawns a test gateway instance on an ephemeral port.
3. Health checks run against the test instance.
4. **Pass:** config is copied to `gateway-running.conf` and backed up as `gateway-backup-{datetime}.config`. The production gateway is swapped in gracefully.
5. **Fail:** config is reverted and the failure is logged. No swap occurs.

---

## Agent Lifecycle

At startup, king automatically discovers and spawns all kernel agents:

1. Scans `KERNEL_AGENTS_DIR` for directories matching `evo-kernel-agent-*` prefix containing a `soul.md` file.
2. Spawns an `evo-runner` process for each discovered agent, passing the agent folder as argument and `KING_ADDRESS` as an environment variable.
3. Each runner connects back to king via Socket.IO and sends an `agent:register` event with its identity, capabilities, and loaded skills.
4. King joins the agent to the `kernel` room and a role-specific room (e.g. `role:learning`) for targeted pipeline dispatch.
5. King persists the full agent metadata (role, capabilities, skills, PID) to the `agent_status` database table.
6. A background monitor watches for crashed runner processes and updates their status in the database.

Registered agents can be queried via the `GET /agents` HTTP endpoint.

---

## Socket.IO Protocol

| Event | Direction | Payload Type | Description |
|-------|-----------|--------------|-------------|
| `agent:register` | Runner -> King | `AgentRegister` | Runner registers with role and capabilities |
| `agent:status` | Runner -> King | `AgentStatus` | Heartbeat with status and metrics |
| `agent:skill_report` | Runner -> King | `AgentSkillReport` | Skill execution result and score |
| `agent:health` | Runner -> King | `AgentHealth` | API health check results |
| `king:command` | King -> Runner | `KingCommand` | Send command to specific agent |
| `king:config_update` | King -> All | `KingConfigUpdate` | Broadcast config change notification |
| `pipeline:next` | King -> Runner | `PipelineNext` | Advance to next pipeline stage |
| `pipeline:stage_result` | Runner -> King | `PipelineStageResult` | Agent reports stage completion/failure |

---

## Database Schema

Local file `king.db` (gitignored). Managed via Turso/libSQL.

```sql
CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    agent_id TEXT,
    payload TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE pipeline_runs (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL DEFAULT '',
    stage TEXT NOT NULL,
    artifact_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    agent_id TEXT NOT NULL DEFAULT '',
    result TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE agent_status (
    agent_id       TEXT PRIMARY KEY,
    role           TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL DEFAULT 'offline',
    last_heartbeat TEXT NOT NULL,
    capabilities   TEXT NOT NULL DEFAULT '[]',
    skills         TEXT NOT NULL DEFAULT '[]',
    pid            INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE config_history (
    id TEXT PRIMARY KEY,
    config_hash TEXT NOT NULL,
    action TEXT NOT NULL,
    backup_path TEXT,
    created_at TEXT NOT NULL
);
```

---

## Pipeline Coordinator

The `PipelineCoordinator` manages the evolution pipeline state machine:

```
Learning → Building → PreLoad → Evaluation → SkillManage → [complete]
```

**Flow:**
1. `POST /pipeline/start` or internal trigger creates a new run and emits `pipeline:next` to `role:learning` room.
2. Learning agent processes discovery, emits `pipeline:stage_result` with status `completed` and output.
3. Coordinator advances to next stage: creates new DB record, emits `pipeline:next` to `role:building` room.
4. Process repeats through all 5 stages. Failure at any stage stops the pipeline.
5. A background timeout monitor marks stale stages as `timed_out` (default: 5 minutes).

**Role rooms:** Each agent joins a `role:{role_name}` room on registration, enabling targeted `pipeline:next` dispatch.

---

## Source File Layout

```
src/
  main.rs                  Entry point: init DB, Socket.IO, pipeline coordinator, HTTP routes
  config_watcher.rs        Watch gateway.config via notify crate, emit change events internally
  gateway_manager.rs       Config lifecycle: test -> health check -> swap -> backup or revert
  agent_manager.rs         Discover evo-kernel-agent-* repos, spawn/stop/monitor runner processes
  pipeline_coordinator.rs  Pipeline state machine: start/advance/timeout pipeline runs
  socket_server.rs         socketioxide event handlers for all Socket.IO events
  task_db.rs               Turso/libSQL database management (agents, tasks, pipeline runs)
  state.rs                 Shared application state (DB, Socket.IO, registry, coordinator)
  error.rs                 Error types
```

---

## Dependencies

```toml
[dependencies]
axum = "0.7"
socketioxide = "0.13"
tokio = { version = "1", features = ["full"] }
notify = "8.2"
libsql = "0.6"
serde = "1.0"
serde_json = "1.0"
toml = "0.8"
chrono = "0.4"
tracing = "0.1"
tracing-subscriber = "0.3"
evo-common = "0.2"
```

---

## Quick Install

The `install.sh` bootstrap script sets up the entire evo system:

```bash
curl -fsSL https://raw.githubusercontent.com/ai-evo-agents/evo-king/main/install.sh | bash
```

Or run locally:

```bash
bash install.sh
```

**What it does:**
1. Creates `~/.evo-agents/` directory structure (`bin/`, `data/`, `logs/`, `repos/`)
2. Clones all evo repos (or `git pull` if already cloned)
3. Downloads pre-built binaries from GitHub releases (evo-king, evo-gateway, kernel agents)
4. Generates `repos.json` manifest with installed versions
5. Creates a default `gateway.config`

**Smart skip logic:**
- Already-cloned repos are updated via `git pull` instead of re-cloning
- Already-installed binaries are skipped if the installed version matches the latest release (tracked via `.version` sidecar files)
- Missing GitHub releases are handled gracefully — the script warns and continues instead of failing

**Re-running is safe:** Running `install.sh` again only downloads new/updated components.

---

## Build and Run

```bash
# Build
cargo build --release

# Run
cargo run --release

# Check version
cargo run -- --version
# or after install:
evo-king --version
```

The server starts on port 3000 by default.

All evo binaries support `--version` / `-V` to print their name and version.

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `EVO_KING_PORT` | Port for the Socket.IO / HTTP server | `3000` |
| `EVO_KING_DB_PATH` | Path to the local libSQL database file | `king.db` |
| `EVO_GATEWAY_CONFIG` | Path to the gateway config file to watch | `gateway.config` |
| `EVO_GATEWAY_RUNNING_CONFIG` | Path to the active gateway config | `gateway-running.conf` |
| `EVO_GATEWAY_BACKUP_DIR` | Directory for gateway config backups | `backups/` |
| `EVO_LOG_PATH` | Path for structured JSON log output | `logs/king.log` |
| `KERNEL_AGENTS_DIR` | Directory containing `evo-kernel-agent-*` repos | `..` (parent dir) |
| `RUNNER_BINARY` | Path to the evo-runner binary | `../evo-agents/target/release/evo-runner` |

---

## HTTP Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Returns king service status |
| `GET` | `/agents` | Returns all registered agents with full metadata |
| `POST` | `/pipeline/start` | Trigger a new pipeline run (body: `{"trigger": "manual"}`) |
| `GET` | `/pipeline/runs` | List all pipeline run stages |
| `GET` | `/pipeline/runs/:run_id` | Detailed stage history for a specific run |

---

## License

MIT
