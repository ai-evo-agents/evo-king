use crate::agent_manager::AgentRegistry;
use libsql::Database;
use reqwest::Client;
use socketioxide::SocketIo;
use std::sync::Arc;

/// Shared application state for the king process.
///
/// Cloned (cheaply via `Arc`) into every async task and Socket.IO handler.
pub struct KingState {
    /// Turso/libSQL local database for task management.
    pub db: Arc<Database>,
    /// Socket.IO handle — used to broadcast events to all connected runners.
    pub io: SocketIo,
    /// HTTP client for probing gateway health.
    pub http_client: Arc<Client>,
    /// Filesystem path to gateway.json (watched for changes).
    pub gateway_config_path: String,
    /// Registry of spawned runner processes (agent_id → PID + Child handle).
    pub agent_registry: Arc<AgentRegistry>,
}
