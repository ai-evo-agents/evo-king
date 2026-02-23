# evo-king

Central orchestrator for the evo agent system. Manages agent lifecycle, config lifecycle, task tracking, and pipeline coordination via Socket.IO.

## Quick Commands

```bash
# Run (default port 3000)
cargo run

# Build release
cargo build --release

# Test
cargo test

# Lint
cargo clippy -- -D warnings

# Health check (once running)
curl http://localhost:3000/health
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `KING_PORT` | `3000` | HTTP + Socket.IO server port |
| `KING_DB_PATH` | `king.db` | libSQL (Turso) local database path |
| `GATEWAY_CONFIG_PATH` | `../evo-gateway/gateway.json` | Path watched for config changes |
| `AGENTS_ROOT` | — | Root dir for kernel agent discovery |
| `EVO_LOG_DIR` | `./logs` | Log output directory |
| `RUST_LOG` | `info` | Log level filter |

## Architecture

```
src/
├── main.rs              — startup: init DB, Socket.IO, HTTP server, config watcher
├── state.rs             — KingState (db, io, http_client, gateway_config_path)
├── error.rs             — KingError with thiserror
├── task_db.rs           — libSQL schema + CRUD helpers
├── socket_server.rs     — Socket.IO namespace handler + event registration
├── config_watcher.rs    — notify 8 filesystem watcher for gateway.json
├── gateway_manager.rs   — config validation, backup, DB logging, broadcast
└── agent_manager.rs     — kernel agent discovery + runner process management
```

## Socket.IO Protocol

King runs a Socket.IO server on port 3000 (namespace `/`).

### Rooms

| Room | Auto-joined by | Purpose |
|------|----------------|---------|
| `kernel` | Agents with role: learning, building, pre-load, evaluation, skill-manage | Scoped task management broadcasts |

Kernel agents are automatically joined to the `kernel` room on `agent:register`. Task change notifications (`task:changed`) are broadcast only to this room.

### Events King Receives (runners emit)

| Event | Payload | Handler |
|-------|---------|---------|
| `agent:register` | `{ agent_id, role, capabilities }` | Upserts agent in `agent_status` table, joins kernel room if kernel role |
| `agent:status` | `{ agent_id, status }` | Updates `agent_status.last_heartbeat` |
| `agent:skill_report` | `{ agent_id, skill_id, result, score }` | Creates task row, logs to `pipeline_runs` |
| `agent:health` | `{ agent_id, health_checks: [...] }` | Logs endpoint health results |
| `task:create` | `{ task_type, agent_id?, payload? }` | Creates task, acks full TaskRecord |
| `task:update` | `{ task_id, status?, agent_id?, payload? }` | Updates task, acks updated TaskRecord |
| `task:get` | `{ task_id }` | Fetches task, acks TaskRecord |
| `task:list` | `{ limit?, status?, agent_id? }` | Lists tasks, acks array of TaskRecord |
| `task:delete` | `{ task_id }` | Deletes task, acks success boolean |

### Events King Emits (runners receive)

| Event | Payload | Scope | Description |
|-------|---------|-------|-------------|
| `king:command` | `{ command, target_agent, params }` | All | Targeted instruction to a specific agent |
| `king:config_update` | `{ config_type, new_config_hash }` | All | Broadcast on gateway.json change |
| `pipeline:next` | `{ stage, artifact_id, metadata }` | All | Advance kernel pipeline |
| `task:changed` | `{ action, task?, task_id? }` | Kernel room | Broadcast on task create/update/delete |

See `evo-common/src/messages.rs` for full struct definitions.

## Database Schema (`king.db`)

Managed by libSQL (local SQLite). Tables created automatically on first run.

```sql
-- Active tasks
tasks (id TEXT, type TEXT, status TEXT, agent_id TEXT, payload TEXT, created_at TEXT, updated_at TEXT)

-- Pipeline execution history
pipeline_runs (id TEXT, stage TEXT, artifact_id TEXT, status TEXT, result TEXT, created_at TEXT, updated_at TEXT)

-- Agent status + heartbeat tracking
agent_status (agent_id TEXT PRIMARY KEY, role TEXT, status TEXT, last_heartbeat TEXT)

-- Gateway config change audit trail
config_history (id TEXT, config_hash TEXT, action TEXT, backup_path TEXT, timestamp TEXT)
```

## Config Lifecycle (gateway_manager)

When `gateway.json` is modified on disk:

1. Read and validate JSON (must parse as `GatewayConfig`)
2. Compute `DefaultHasher` hash of the file content
3. Create timestamped backup: `<config_dir>/backups/gateway-backup-YYYYMMDD_HHMMSS.json`
4. Insert into `config_history` with action `"reloaded"` or `"reload_failed"`
5. Broadcast `king:config_update` to all Socket.IO clients

The `.gitignore` excludes `backups/` and `gateway-backup-*.config` from the repo.

## Kernel Agent Discovery (`agent_manager`)

```rust
discover_kernel_agents(agents_root: &str) -> Vec<PathBuf>
// Finds all paths matching: <agents_root>/kernel/*/soul.md
// Returns parent dirs as candidate runner folders
```

```rust
spawn_runner(binary: &str, agent_folder: &Path, king_address: &str) -> Child
// Runs: <binary> <agent_folder>
// Env: KING_ADDRESS=<king_address>
```

## Logging

Logs write to `logs/king.log` (rolling daily JSON) and stdout.

```bash
RUST_LOG=debug cargo run   # verbose
RUST_LOG=info cargo run    # production
```

Log entries include: timestamp, level, module path, file:line, thread ID (in file layer).
