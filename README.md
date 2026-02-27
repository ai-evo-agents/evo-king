# evo-king

Central orchestrator for the Evo self-evolution agent system. Manages gateway config lifecycle, spawns and monitors agent runner processes, serves a Socket.IO server for bidirectional agent communication, coordinates the evolution pipeline, and maintains a local database for tasks, memories, and pipeline tracking.

**Status:** Active development — agent management, pipeline coordinator, memory system, cron manager, and task DB operational.

---

## Part of the Evo System

| Crate | Role |
|-------|------|
| evo-common | Shared types (dependency) |
| evo-gateway | API aggregator — king manages its config lifecycle |
| **evo-king** | Central orchestrator, port 3300 |
| evo-agents | Runner binary — runners connect to king via Socket.IO |
| evo-kernel-agent-* | 6 kernel agent repos (learning, building, pre-load, evaluation, skill-manage, update) |
| evo-user-agent-template | Template for creating user agents |

---

## Architecture

```
                    evo-king (port 3300)
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
                    │  │   Pipeline Coordinator         │      │
                    │  │   5-stage evolution pipeline    │      │
                    │  └───────────────────────────────┘      │
                    │  ┌───────────────────────────────┐      │
                    │  │   Cron Manager                 │      │
                    │  │   Scheduled system tasks        │      │
                    │  └───────────────────────────────┘      │
                    │  ┌───────────────────────────────┐      │
                    │  │     Turso Local DB (libSQL)    │      │
                    │  │  tasks, pipeline_runs,         │      │
                    │  │  agent_status, config_history, │      │
                    │  │  cron_jobs, task_logs,          │      │
                    │  │  memories, memory_tiers,        │      │
                    │  │  task_memories, agent_history,  │      │
                    │  │  socket_events                  │      │
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
5. King persists the full agent metadata (role, capabilities, skills, PID, soul content, version, binary path) to the `agent_status` database table. Registration count auto-increments on each re-register.
6. A background monitor watches for crashed runner processes, increments crash counts, and records state transitions in the `agent_history` table.
7. On disconnect, king records a history entry tracking the transition from the agent's current status to "disconnected".

Registered agents can be queried via the `GET /agents` HTTP endpoint. Full agent detail (including soul content and recent history) is available at `GET /agents/{agent_id}/detail`.

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
| `king:system_info` | King -> Runner | `SystemDiscovery` | System info sent on registration + on-demand |

---

## Database Schema

Local file `king.db` (gitignored). Managed via Turso/libSQL. Tables created automatically on first run.

### Core Tables

```sql
-- Active tasks
CREATE TABLE tasks (
    id          TEXT PRIMARY KEY,
    task_type   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    agent_id    TEXT NOT NULL DEFAULT '',
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Pipeline execution history
CREATE TABLE pipeline_runs (
    id          TEXT PRIMARY KEY,
    stage       TEXT NOT NULL,
    artifact_id TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'pending',
    result      TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Agent status + heartbeat tracking
CREATE TABLE agent_status (
    agent_id            TEXT PRIMARY KEY,
    role                TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'offline',
    last_heartbeat      TEXT NOT NULL,
    preferred_model     TEXT NOT NULL DEFAULT '',
    soul_content        TEXT NOT NULL DEFAULT '',
    binary_path         TEXT NOT NULL DEFAULT '',
    version             TEXT NOT NULL DEFAULT '',
    first_registered_at TEXT NOT NULL DEFAULT '',
    registration_count  INTEGER NOT NULL DEFAULT 0,
    crash_count         INTEGER NOT NULL DEFAULT 0,
    socket_id           TEXT NOT NULL DEFAULT ''
);

-- Gateway config change audit trail
CREATE TABLE config_history (
    id          TEXT PRIMARY KEY,
    config_hash TEXT NOT NULL,
    action      TEXT NOT NULL,
    backup_path TEXT NOT NULL DEFAULT '',
    timestamp   TEXT NOT NULL
);
```

### Cron & Logging Tables

```sql
-- Cron job registry — system and user-defined recurring tasks
CREATE TABLE cron_jobs (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    schedule    TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    last_run_at TEXT,
    next_run_at TEXT,
    last_status TEXT,
    last_error  TEXT,
    created_at  TEXT NOT NULL
);

-- Task execution logs
CREATE TABLE task_logs (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL,
    level      TEXT NOT NULL DEFAULT 'info',
    message    TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT '',
    agent_id   TEXT NOT NULL DEFAULT '',
    stage      TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
```

### Memory Tables

The memory system provides a tiered storage model (L0/L1/L2) for agents to persist and retrieve knowledge across pipeline runs.

```sql
-- Core memory records
CREATE TABLE memories (
    id              TEXT PRIMARY KEY,
    scope           TEXT NOT NULL DEFAULT 'system',
    category        TEXT NOT NULL DEFAULT 'general',
    key             TEXT NOT NULL DEFAULT '',
    metadata        TEXT NOT NULL DEFAULT '{}',
    tags            TEXT NOT NULL DEFAULT '',
    agent_id        TEXT NOT NULL DEFAULT '',
    run_id          TEXT NOT NULL DEFAULT '',
    skill_id        TEXT NOT NULL DEFAULT '',
    relevance_score REAL NOT NULL DEFAULT 0.0,
    access_count    INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Tiered content (L0 = raw, L1 = processed, L2 = compressed/summary)
CREATE TABLE memory_tiers (
    id         TEXT PRIMARY KEY,
    memory_id  TEXT NOT NULL,
    tier       TEXT NOT NULL,
    content    TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Task-memory junction — binds memories to tasks for fast filtering
CREATE TABLE task_memories (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL,
    memory_id  TEXT NOT NULL,
    created_at TEXT NOT NULL
);
```

**Indexes:** `memories.scope`, `memories.category`, `memories.agent_id`, `memory_tiers.memory_id`, `task_memories.task_id`, `task_memories.memory_id`.

### Observability Tables

```sql
-- Agent state transition history (online → crashed → respawned, etc.)
CREATE TABLE agent_history (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    prev_status TEXT NOT NULL DEFAULT '',
    new_status  TEXT NOT NULL,
    trigger     TEXT NOT NULL DEFAULT '',
    detail      TEXT NOT NULL DEFAULT '',
    pid         INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

-- Socket.IO event audit log (fire-and-forget, payloads truncated to 4KB)
CREATE TABLE socket_events (
    id          TEXT PRIMARY KEY,
    event_name  TEXT NOT NULL,
    direction   TEXT NOT NULL DEFAULT 'inbound',
    agent_id    TEXT NOT NULL DEFAULT '',
    socket_id   TEXT NOT NULL DEFAULT '',
    room        TEXT NOT NULL DEFAULT '',
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL
);
```

**Indexes:** `agent_history.agent_id`, `agent_history.created_at`, `socket_events.agent_id`, `socket_events.event_name`, `socket_events.created_at`.

**Retention:** Daily cron jobs purge `socket_events` older than 7 days and `agent_history` older than 30 days.

---

## System Discovery

At startup, king probes the host system for CLI tools, versions, and environment info. Results are cached in memory, persisted to `~/.evo-agents/data/system_info.json`, and shared with agents.

**What it discovers:**
- **System info:** OS, version, architecture, hostname, CPU count, memory
- **CLI tools:** rustc, cargo, rustup, cargo-clippy, cargo-fmt, node, npm, npx, bun, git, rg, curl, docker, python3
- **Evo components:** evo-king, evo-gateway, evo-runner, and all 6 kernel agent binaries
- **Checklist:** Pass/fail/suggested status for each tool and component

**Tool statuses:**
- `available` — tool found with version detected
- `not_found` — tool not installed
- `suggested` — not found, but recommended for the evo system

**Agent integration:**
- System info is automatically sent to each agent on registration via `king:system_info`
- Agents can request it on-demand via Socket.IO ack
- Available via `GET /system-info` and refreshable via `POST /system-info/refresh`

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

## Memory System

King provides a centralized memory store for agents to persist knowledge across pipeline runs and sessions.

**Concepts:**
- **Scope:** `system` (shared), `agent` (per-agent), `pipeline` (per-run)
- **Category:** Free-form grouping (e.g. `general`, `skill`, `config`, `observation`)
- **Tiers:** L0 (raw data), L1 (processed/structured), L2 (compressed/summary)
- **Relevance scoring:** Memories track `relevance_score` and `access_count` for retrieval ranking
- **Task binding:** Memories can be linked to specific tasks via the `task_memories` junction table

**API:** See the Memory endpoints section in HTTP Endpoints below.

---

## Source File Layout

```
src/
  main.rs                  Entry point: init DB, Socket.IO, pipeline, cron, HTTP routes, dashboard
  state.rs                 Shared application state (DB, Socket.IO, registry, coordinator, system discovery)
  system_discovery.rs      Probe CLI tools, system info, evo components; persist to system_info.json
  error.rs                 Error types
  task_db.rs               Turso/libSQL database management (schema, CRUD for all tables)
  memory_db.rs             Memory CRUD: create, get, update, delete, search, tiers, stats
  socket_server.rs         socketioxide event handlers for all Socket.IO events
  config_watcher.rs        Watch gateway.config via notify crate, emit change events internally
  gateway_manager.rs       Config lifecycle: test -> health check -> swap -> backup or revert
  agent_manager.rs         Discover evo-kernel-agent-* repos, spawn/stop/monitor runner processes
  pipeline_coordinator.rs  Pipeline state machine: start/advance/timeout pipeline runs
  cron_manager.rs          System cron job scheduling (update checks, gateway health, retention purge)
  doctor.rs                Installation validation and repair (evo-king doctor subcommand)
```

---

## Dependencies

```toml
[dependencies]
axum = "0.7"
socketioxide = "0.14"
tokio = { version = "1", features = ["full"] }
notify = "8"
libsql = "0.6"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
tracing-appender = "0.2"
anyhow = "1.0"
clap = { version = "4", features = ["derive"] }
thiserror = "2.0"
reqwest = { version = "0.12", features = ["json", "native-tls-vendored"] }
uuid = { version = "1.0", features = ["v4"] }
tower-http = { version = "0.6", features = ["cors", "fs"] }
evo-common = { path = "../evo-common" }
portable-pty = "0.8"
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

# Run doctor (validate installation)
cargo run -- doctor
cargo run -- doctor --fix   # auto-repair
```

The server starts on port 3300 by default.

All evo binaries support `--version` / `-V` to print their name and version.

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `KING_PORT` | Port for the Socket.IO / HTTP server | `3300` |
| `KING_DB_PATH` | Path to the local libSQL database file | `king.db` |
| `GATEWAY_CONFIG_PATH` | Path to the gateway config file to watch | `../evo-gateway/gateway.json` |
| `KERNEL_AGENTS_DIR` | Directory containing `evo-kernel-agent-*` repos | `..` (parent dir) |
| `RUNNER_BINARY` | Path to the evo-runner binary | `../evo-agents/target/release/evo-runner` |
| `EVO_LOG_DIR` | Log output directory | `./logs` |
| `RUST_LOG` | Log level filter | `info` |

---

## HTTP Endpoints

### Core

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Returns king service status |
| `GET` | `/agents` | Returns all registered agents with full metadata |
| `GET` | `/system-info` | Returns cached system discovery (tools, versions, evo components, checklist) |
| `POST` | `/system-info/refresh` | Re-probe system, save to disk, broadcast to agents |

### Pipeline

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/pipeline/start` | Trigger a new pipeline run (`{"trigger": "manual"}`) |
| `GET` | `/pipeline/runs` | List all pipeline run stages |
| `GET` | `/pipeline/runs/:run_id` | Detailed stage history for a specific run |

### Tasks

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/tasks` | List tasks (query: `limit`, `status`, `agent_id`) |
| `GET` | `/task/current` | Get the current active task |
| `GET` | `/task/:task_id/logs` | Logs for a specific task |
| `GET` | `/task/:task_id/subtasks` | Subtasks for a specific task |

### Memory

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/memories` | List memories (query filters: scope, category, agent_id) |
| `POST` | `/memories` | Create a new memory record |
| `POST` | `/memories/search` | Search memories by criteria |
| `GET` | `/memories/stats` | Memory usage statistics |
| `GET` | `/memories/:memory_id` | Get a specific memory |
| `PUT` | `/memories/:memory_id` | Update a memory |
| `DELETE` | `/memories/:memory_id` | Delete a memory |
| `GET` | `/memories/:memory_id/tiers` | List all tiers for a memory |
| `POST` | `/memories/:memory_id/tiers` | Upsert a tier (L0/L1/L2) |
| `GET` | `/memories/:memory_id/tiers/:tier` | Get a specific tier content |
| `DELETE` | `/memories/:memory_id/tiers/:tier` | Delete a specific tier |
| `GET` | `/memories/:memory_id/tasks` | List tasks linked to a memory |
| `GET` | `/task/:task_id/memories` | List memories linked to a task |
| `POST` | `/task/:task_id/memories` | Bind a memory to a task |

### Agents

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/agents/{agent_id}/model` | Get an agent's preferred model |
| `PUT` | `/agents/{agent_id}/model` | Set an agent's preferred model (`{"model": "..."}`) |
| `GET` | `/agents/{agent_id}/history` | Agent state transition log (query: `limit`, `offset`) |
| `GET` | `/agents/{agent_id}/detail` | Full agent info + last 20 history entries |

### Logs & Observability

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/logs/events` | Socket.IO event audit log (query: `agent_id`, `event_name`, `direction`, `since`, `until`, `limit`, `offset`) |
| `GET` | `/logs/system` | Tail a component's log file (query: `lines`, `component`) |

### Gateway & Config

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/gateway/models` | Proxy to evo-gateway's `/v1/models` — lists available LLM models |
| `GET` | `/gateway/config` | Read current gateway config |
| `PUT` | `/gateway/config` | Update gateway config |
| `GET` | `/config-history` | Gateway config change audit trail |

### Admin

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/admin/config-sync` | Force sync gateway config |
| `GET` | `/admin/crons` | List all registered cron jobs |
| `POST` | `/admin/crons/:name/run` | Manually trigger a cron job |

### Debug

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/debug/prompt` | Debug LLM prompt construction |

---

## License

MIT
