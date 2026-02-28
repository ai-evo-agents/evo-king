use anyhow::{Context, Result};
use libsql::Database;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use serde_json::{Value, json};

use crate::task_db::db_connect;

// ─── Store incoming OTLP trace data ──────────────────────────────────────────

pub async fn store_trace_data(db: &Database, request: ExportTraceServiceRequest) -> Result<()> {
    let conn = db_connect(db).await?;

    for resource_spans in &request.resource_spans {
        let service_name = extract_service_name(&resource_spans.resource);
        let resource_json =
            serde_json::to_string(&resource_spans.resource).unwrap_or_else(|_| "{}".into());

        for scope_spans in &resource_spans.scope_spans {
            for span in &scope_spans.spans {
                let trace_id = hex::encode(&span.trace_id);
                let span_id = hex::encode(&span.span_id);
                let parent_span_id = if span.parent_span_id.is_empty() {
                    String::new()
                } else {
                    hex::encode(&span.parent_span_id)
                };

                let start_ns = span.start_time_unix_nano as i64;
                let end_ns = span.end_time_unix_nano as i64;
                let duration_ns = end_ns.saturating_sub(start_ns);
                let status_code = span.status.as_ref().map_or(0_i32, |s| s.code);
                let status_message = span
                    .status
                    .as_ref()
                    .map(|s| s.message.clone())
                    .unwrap_or_default();
                let attributes_json =
                    serde_json::to_string(&span.attributes).unwrap_or_else(|_| "[]".into());
                let events_json =
                    serde_json::to_string(&span.events).unwrap_or_else(|_| "[]".into());
                let kind = span.kind;

                // Upsert trace (one row per trace_id, aggregated envelope)
                conn.execute(
                    "INSERT INTO traces (
                        trace_id, service_name, root_span_name,
                        start_time_ns, end_time_ns, duration_ns,
                        status_code, span_count, resource, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, datetime('now'))
                    ON CONFLICT(trace_id) DO UPDATE SET
                        start_time_ns = MIN(traces.start_time_ns, excluded.start_time_ns),
                        end_time_ns   = MAX(traces.end_time_ns,   excluded.end_time_ns),
                        duration_ns   = MAX(traces.end_time_ns, excluded.end_time_ns)
                                      - MIN(traces.start_time_ns, excluded.start_time_ns),
                        span_count    = traces.span_count + 1,
                        status_code   = CASE WHEN excluded.status_code > traces.status_code
                                        THEN excluded.status_code ELSE traces.status_code END,
                        updated_at    = datetime('now')",
                    (
                        trace_id.as_str(),
                        service_name.as_str(),
                        span.name.as_str(),
                        start_ns,
                        end_ns,
                        duration_ns,
                        status_code,
                        resource_json.as_str(),
                    ),
                )
                .await
                .context("upsert trace")?;

                // Insert span (replace if duplicate span_id within same trace)
                conn.execute(
                    "INSERT OR REPLACE INTO spans (
                        trace_id, span_id, parent_span_id, name, kind,
                        service_name, start_time_ns, end_time_ns, duration_ns,
                        status_code, status_message, attributes, events
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    (
                        trace_id.as_str(),
                        span_id.as_str(),
                        parent_span_id.as_str(),
                        span.name.as_str(),
                        kind,
                        service_name.as_str(),
                        start_ns,
                        end_ns,
                        duration_ns,
                        status_code,
                        status_message.as_str(),
                        attributes_json.as_str(),
                        events_json.as_str(),
                    ),
                )
                .await
                .context("insert span")?;
            }
        }
    }
    Ok(())
}

// ─── Query helpers for dashboard REST API ────────────────────────────────────

pub async fn list_traces(
    db: &Database,
    service: Option<&str>,
    status: Option<i32>,
    min_duration_ms: Option<u64>,
    limit: u32,
    offset: u32,
) -> Result<(Vec<Value>, u32)> {
    let conn = db_connect(db).await?;

    // Build WHERE clauses dynamically
    let mut conditions: Vec<String> = Vec::new();
    if let Some(svc) = service
        && !svc.is_empty()
    {
        conditions.push(format!("service_name = '{}'", svc.replace('\'', "''")));
    }
    if let Some(s) = status {
        conditions.push(format!("status_code = {s}"));
    }
    if let Some(ms) = min_duration_ms {
        let ns = ms * 1_000_000;
        conditions.push(format!("duration_ns >= {ns}"));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    // Count query
    let count_sql = format!("SELECT COUNT(*) FROM traces {where_clause}");
    let mut count_rows = conn.query(&count_sql, ()).await.context("count traces")?;
    let total: u32 = if let Some(row) = count_rows.next().await? {
        row.get::<i64>(0)? as u32
    } else {
        0
    };

    // Data query
    let data_sql = format!(
        "SELECT trace_id, service_name, root_span_name,
                start_time_ns, end_time_ns, duration_ns,
                status_code, span_count, resource, updated_at
         FROM traces {where_clause}
         ORDER BY start_time_ns DESC
         LIMIT {limit} OFFSET {offset}"
    );

    let mut rows = conn.query(&data_sql, ()).await.context("list traces")?;
    let mut traces = Vec::new();

    while let Some(row) = rows.next().await? {
        traces.push(json!({
            "trace_id":       row.get::<String>(0)?,
            "service_name":   row.get::<String>(1)?,
            "root_span_name": row.get::<String>(2)?,
            "start_time_ns":  row.get::<i64>(3)?,
            "end_time_ns":    row.get::<i64>(4)?,
            "duration_ns":    row.get::<i64>(5)?,
            "status_code":    row.get::<i64>(6)?,
            "span_count":     row.get::<i64>(7)?,
            "resource":       serde_json::from_str::<Value>(&row.get::<String>(8)?).unwrap_or(json!({})),
            "updated_at":     row.get::<String>(9)?,
        }));
    }

    Ok((traces, total))
}

pub async fn get_trace_detail(
    db: &Database,
    trace_id: &str,
) -> Result<(Option<Value>, Vec<Value>)> {
    let conn = db_connect(db).await?;

    // Fetch trace metadata
    let mut trace_rows = conn
        .query(
            "SELECT trace_id, service_name, root_span_name,
                    start_time_ns, end_time_ns, duration_ns,
                    status_code, span_count, resource, updated_at
             FROM traces WHERE trace_id = ?1",
            [trace_id],
        )
        .await
        .context("get trace")?;

    let trace = if let Some(row) = trace_rows.next().await? {
        Some(json!({
            "trace_id":       row.get::<String>(0)?,
            "service_name":   row.get::<String>(1)?,
            "root_span_name": row.get::<String>(2)?,
            "start_time_ns":  row.get::<i64>(3)?,
            "end_time_ns":    row.get::<i64>(4)?,
            "duration_ns":    row.get::<i64>(5)?,
            "status_code":    row.get::<i64>(6)?,
            "span_count":     row.get::<i64>(7)?,
            "resource":       serde_json::from_str::<Value>(&row.get::<String>(8)?).unwrap_or(json!({})),
            "updated_at":     row.get::<String>(9)?,
        }))
    } else {
        None
    };

    // Fetch all spans for this trace
    let mut span_rows = conn
        .query(
            "SELECT span_id, trace_id, parent_span_id, name, kind,
                    service_name, start_time_ns, end_time_ns, duration_ns,
                    status_code, status_message, attributes, events
             FROM spans WHERE trace_id = ?1
             ORDER BY start_time_ns ASC",
            [trace_id],
        )
        .await
        .context("get trace spans")?;

    let mut spans = Vec::new();
    while let Some(row) = span_rows.next().await? {
        spans.push(json!({
            "span_id":        row.get::<String>(0)?,
            "trace_id":       row.get::<String>(1)?,
            "parent_span_id": row.get::<String>(2)?,
            "name":           row.get::<String>(3)?,
            "kind":           row.get::<i64>(4)?,
            "service_name":   row.get::<String>(5)?,
            "start_time_ns":  row.get::<i64>(6)?,
            "end_time_ns":    row.get::<i64>(7)?,
            "duration_ns":    row.get::<i64>(8)?,
            "status_code":    row.get::<i64>(9)?,
            "status_message": row.get::<String>(10)?,
            "attributes":     serde_json::from_str::<Value>(&row.get::<String>(11)?).unwrap_or(json!([])),
            "events":         serde_json::from_str::<Value>(&row.get::<String>(12)?).unwrap_or(json!([])),
        }));
    }

    Ok((trace, spans))
}

#[allow(dead_code)]
pub async fn cleanup_old_traces(db: &Database, retention_days: u32) -> Result<u64> {
    let conn = db_connect(db).await?;
    let cutoff_ns = (chrono::Utc::now() - chrono::Duration::days(retention_days as i64))
        .timestamp_nanos_opt()
        .unwrap_or(0);

    let deleted_spans = conn
        .execute("DELETE FROM spans WHERE start_time_ns < ?1", [cutoff_ns])
        .await
        .context("cleanup old spans")?;

    conn.execute("DELETE FROM traces WHERE start_time_ns < ?1", [cutoff_ns])
        .await
        .context("cleanup old traces")?;

    Ok(deleted_spans)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn extract_service_name(
    resource: &Option<opentelemetry_proto::tonic::resource::v1::Resource>,
) -> String {
    let Some(res) = resource else {
        return String::new();
    };
    for attr in &res.attributes {
        if attr.key == "service.name"
            && let Some(ref value) = attr.value
            && let Some(ref val) = value.value
        {
            return match val {
                opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s) => {
                    s.clone()
                }
                _ => format!("{val:?}"),
            };
        }
    }
    String::new()
}
