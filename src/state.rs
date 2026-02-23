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
    #[allow(dead_code)]
    pub http_client: Arc<Client>,
    /// Filesystem path to gateway.json (watched for changes).
    #[allow(dead_code)]
    pub gateway_config_path: String,
}
