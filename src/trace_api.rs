use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::state::KingState;
use crate::trace_db;

#[derive(Deserialize)]
pub struct TraceListParams {
    pub service: Option<String>,
    pub status: Option<i32>,
    pub min_duration_ms: Option<u64>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// `GET /traces?service=&status=&min_duration_ms=&limit=&offset=`
pub async fn traces_list_handler(
    State(state): State<Arc<KingState>>,
    Query(params): Query<TraceListParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(50).min(200);
    let offset = params.offset.unwrap_or(0);

    match trace_db::list_traces(
        &state.db,
        params.service.as_deref(),
        params.status,
        params.min_duration_ms,
        limit,
        offset,
    )
    .await
    {
        Ok((traces, count)) => Json(json!({ "traces": traces, "count": count })),
        Err(e) => Json(json!({ "traces": [], "count": 0, "error": e.to_string() })),
    }
}

/// `GET /traces/{trace_id}` — returns trace metadata + all spans.
pub async fn trace_detail_handler(
    State(state): State<Arc<KingState>>,
    Path(trace_id): Path<String>,
) -> Json<Value> {
    match trace_db::get_trace_detail(&state.db, &trace_id).await {
        Ok((Some(trace), spans)) => Json(json!({
            "trace": trace,
            "spans": spans,
        })),
        Ok((None, _)) => Json(json!({ "error": "trace not found" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
