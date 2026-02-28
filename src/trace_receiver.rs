use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use std::sync::Arc;

use crate::state::KingState;
use crate::trace_db;

/// `POST /v1/traces` — OTLP HTTP receiver.
///
/// Accepts either `application/x-protobuf` (binary proto) or
/// `application/json` payloads containing an `ExportTraceServiceRequest`.
///
/// **Important:** This handler must NOT be wrapped with OpenTelemetry span
/// instrumentation because king exports traces to itself — instrumenting the
/// receiver would create an infinite feedback loop.
pub async fn otlp_traces_receiver(
    State(state): State<Arc<KingState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let request = if content_type.contains("application/json") {
        serde_json::from_slice::<ExportTraceServiceRequest>(&body).ok()
    } else {
        // Default: try protobuf (covers application/x-protobuf and missing header)
        ExportTraceServiceRequest::decode(body).ok()
    };

    let Some(req) = request else {
        return StatusCode::BAD_REQUEST;
    };

    // Fire-and-forget: write spans to DB without blocking the exporter
    let db = Arc::clone(&state.db);
    tokio::spawn(async move {
        if let Err(e) = trace_db::store_trace_data(&db, req).await {
            tracing::warn!(err = %e, "failed to store OTLP trace data");
        }
    });

    StatusCode::OK
}
