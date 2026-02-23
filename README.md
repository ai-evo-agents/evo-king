# evo-king

Central orchestrator for the Evo self-evolution agent system. Manages gateway config lifecycle, spawns and monitors agent runner processes, serves a Socket.IO server for bidirectional agent communication, and maintains a local task database for evolution pipeline tracking.

**Status:** Skeleton — Phase 4 pending implementation.

---

## Part of the Evo System

| Crate | Role |
|-------|------|
| evo-common | Shared types (dependency) |
| evo-gateway | API aggregator — king manages its config lifecycle |
| **evo-king** | Central orchestrator, port 3000 |
| evo-agents | Runner binary + agent folders — runners connect to king via Socket.IO |

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
    stage TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    result TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE agent_status (
    agent_id TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'starting',
    last_heartbeat TEXT NOT NULL
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

## Evolution Pipeline

The king orchestrates a multi-stage pipeline across kernel agents:

1. **Learning agent** discovers a potential skill and emits `agent:skill_report`.
2. **King** creates a task in the DB and dispatches to the **Building agent** via `pipeline:next`.
3. **Building agent** packages the skill and reports back.
4. **King** dispatches to the **Pre-load agent** for validation.
5. **Pre-load agent** health-checks the skill APIs and reports back.
6. **King** dispatches to the **Evaluation agent** for scoring.
7. **Evaluation agent** tests quality and reports a score.
8. **King** dispatches to the **Skill Manage agent** with the evaluation results.
9. **Skill Manage agent** decides: activate, hold, or discard.

---

## Source File Layout

```
src/
  main.rs              Entry point: init DB, start Socket.IO server, start watchers, start agent manager
  config_watcher.rs    Watch gateway.config via notify crate, emit change events internally
  gateway_manager.rs   Config lifecycle: test -> health check -> swap -> backup or revert
  agent_manager.rs     Spawn/stop/monitor runner processes, track PIDs, restart on crash
  socket_server.rs     socketioxide event handlers for all Socket.IO events
  task_db.rs           Turso/libSQL local database management
  logger.rs            Structured JSON logging to logs/king.log
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
evo-common = { git = "https://github.com/ai-evo-agents/evo-common" }
```

---

## Build and Run

```bash
# Build
cargo build --release

# Run
cargo run --release
```

The server starts on port 3000 by default.

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

---

## License

MIT
