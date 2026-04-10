//! Axum handler functions for the Haystack REST API.
//!
//! Each handler sends an [`EngineCmd`] through the shared channel and awaits
//! the oneshot reply. This keeps the engine on a single thread while Axum
//! can freely dispatch handlers across its runtime.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};

use super::error::AppError;
use super::types::*;
use super::EngineHandle;
use crate::sox::sox_handlers::CHANNEL_COMP_BASE;
use crate::sox::DynSlotStoreHandle;

/// Check if the client wants Zinc wire format.
fn wants_zinc(headers: &HeaderMap) -> bool {
    headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/zinc"))
        .unwrap_or(false)
}

/// Return a Zinc grid response with the correct content type.
fn zinc_response(body: String) -> Response {
    ([(CONTENT_TYPE, "text/zinc; charset=utf-8")], body).into_response()
}

// ── Haystack Standard Ops ───────────────────────────────────

/// GET /api/about — server metadata with real boot time.
pub async fn about(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
) -> Result<Response, AppError> {
    let (boot_secs, now_secs) = handle.about_info().await.map_err(AppError::from)?;
    let resp = AboutResponse {
        server_name: "Sandstar Engine".into(),
        vendor_name: "EacIo",
        product_name: "sandstar-engine-server",
        product_version: env!("CARGO_PKG_VERSION"),
        haystack_version: "3.0",
        server_time: epoch_to_string(now_secs),
        server_boot_time: epoch_to_string(boot_secs),
        build_info: concat!(env!("CARGO_PKG_VERSION"), "+", env!("BUILD_GIT_HASH")),
    };
    if wants_zinc(&headers) {
        Ok(zinc_response(
            super::zinc_format::about_to_zinc(&resp).to_zinc(),
        ))
    } else {
        Ok(Json(resp).into_response())
    }
}

/// GET /api/ops — list available operations.
pub async fn ops(headers: HeaderMap) -> Response {
    let list = ops_list();
    if wants_zinc(&headers) {
        zinc_response(super::zinc_format::ops_to_zinc(&list).to_zinc())
    } else {
        Json(list).into_response()
    }
}

/// GET /api/read — read channel(s) by id or filter.
///
/// When the SOX component tree and DynSlotStore are available (via Extensions),
/// the Haystack filter evaluator also checks dynamic tags attached to components.
pub async fn read(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Query(params): Query<ReadParams>,
    dyn_store_ext: Option<Extension<DynSlotStoreHandle>>,
) -> Result<Response, AppError> {
    let zinc = wants_zinc(&headers);

    // Read by explicit channel id
    if let Some(id) = params.id {
        let val = handle.read_channel(id).await.map_err(AppError::from)?;
        if zinc {
            return Ok(zinc_response(
                super::zinc_format::values_to_zinc(&[val]).to_zinc(),
            ));
        }
        return Ok(Json(vec![channel_value_json(&val)]).into_response());
    }

    // Read by filter
    if let Some(ref filter) = params.filter {
        // Fast path: "channel==N"
        if let Some(id) = parse_channel_eq(filter) {
            let val = handle.read_channel(id).await.map_err(AppError::from)?;
            if zinc {
                return Ok(zinc_response(
                    super::zinc_format::values_to_zinc(&[val]).to_zinc(),
                ));
            }
            return Ok(Json(vec![channel_value_json(&val)]).into_response());
        }

        // Parse Haystack filter and apply to channel list.
        // When the DynSlotStore and component tree are available, the filter
        // evaluator also checks dynamic tags attached to channel components.
        let all = handle.list_channels().await.map_err(AppError::from)?;
        let limit = params.limit.unwrap_or(100);
        let expr = super::filter::parse(filter);

        // Build a channel-index → dyn_tags lookup if the DynSlotStore is available.
        // Channel components have comp_id = CHANNEL_COMP_BASE + index.
        let dyn_store_lock = dyn_store_ext
            .as_ref()
            .map(|Extension(ds)| ds.read().unwrap());

        let matched: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(i, ch)| match &expr {
                Ok(f) => {
                    let comp_id = CHANNEL_COMP_BASE + *i as u16;
                    let tags = dyn_store_lock.as_ref().and_then(|ds| ds.get_all(comp_id));
                    super::filter::matches_with_tags(f, ch, tags)
                }
                Err(_) => matches_simple(ch, filter),
            })
            .map(|(_, ch)| ch.clone())
            .take(limit)
            .collect();
        // Drop the lock before building the response.
        drop(dyn_store_lock);

        if zinc {
            return Ok(zinc_response(
                super::zinc_format::channels_to_zinc(&matched).to_zinc(),
            ));
        }
        let rows: Vec<serde_json::Value> = matched.iter().map(channel_info_json).collect();
        return Ok(Json(rows).into_response());
    }

    // No filter, no id — return all channels (with limit)
    let all = handle.list_channels().await.map_err(AppError::from)?;
    let limit = params.limit.unwrap_or(100);
    let matched: Vec<_> = all.into_iter().take(limit).collect();
    if zinc {
        return Ok(zinc_response(
            super::zinc_format::channels_to_zinc(&matched).to_zinc(),
        ));
    }
    let rows: Vec<serde_json::Value> = matched.iter().map(channel_info_json).collect();
    Ok(Json(rows).into_response())
}

/// GET /api/formats — list supported MIME types.
pub async fn formats(headers: HeaderMap) -> Response {
    let list = super::types::formats_list();
    if wants_zinc(&headers) {
        zinc_response(super::zinc_format::formats_to_zinc(&list).to_zinc())
    } else {
        Json(list).into_response()
    }
}

/// POST /api/hisRead — read historical trend data for a point.
///
/// Request body: `{ "id": 1113, "range": "today" }`
/// Supported range values:
/// - "today" — from midnight today
/// - "yesterday" — from midnight yesterday to midnight today
/// - "last24h" — last 24 hours
/// - "YYYY-MM-DD,YYYY-MM-DD" — explicit date range
/// - absent — return all available history
pub async fn his_read(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Json(req): Json<super::types::HisReadRequest>,
) -> Result<Response, AppError> {
    let (since_unix, until_unix) = parse_his_range(req.range.as_deref());

    // Use a generous limit — hisRead returns all matching points within range.
    let limit = 10000;

    let mut points = handle
        .get_history(req.id, since_unix, limit)
        .await
        .map_err(AppError::from)?;

    // Apply upper bound if a date range was specified.
    if until_unix < u64::MAX {
        points.retain(|p| p.ts <= until_unix);
    }

    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::history_to_zinc(&points).to_zinc(),
        ));
    }
    Ok(Json(points).into_response())
}

/// POST /api/nav — navigate site/equip/point tree.
///
/// Flat hierarchy: one site → all channels as points.
pub async fn nav(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Json(req): Json<super::types::NavRequest>,
) -> Result<Response, AppError> {
    let zinc = wants_zinc(&headers);

    match req.nav_id.as_deref() {
        // Root level: return the single site
        None | Some("") => {
            let rows = vec![serde_json::json!({
                "navId": "site",
                "dis": "Sandstar Site",
                "nav": "M",
            })];
            if zinc {
                return Ok(zinc_response(
                    super::zinc_format::nav_to_zinc(&rows).to_zinc(),
                ));
            }
            Ok(Json(rows).into_response())
        }
        // Site level: return equipment (single equip node for all channels)
        Some("site") => {
            let rows = vec![serde_json::json!({
                "navId": "equip:default",
                "dis": "Default Equipment",
                "nav": "M",
            })];
            if zinc {
                return Ok(zinc_response(
                    super::zinc_format::nav_to_zinc(&rows).to_zinc(),
                ));
            }
            Ok(Json(rows).into_response())
        }
        // Equipment level: return all channels as points
        Some(nav_id) if nav_id.starts_with("equip:") => {
            let channels = handle.list_channels().await.map_err(AppError::from)?;
            let rows: Vec<serde_json::Value> = channels
                .iter()
                .map(|ch| {
                    serde_json::json!({
                        "navId": format!("point:{}", ch.id),
                        "dis": ch.label,
                        "point": "M",
                    })
                })
                .collect();
            if zinc {
                return Ok(zinc_response(
                    super::zinc_format::nav_to_zinc(&rows).to_zinc(),
                ));
            }
            Ok(Json(rows).into_response())
        }
        // Unknown navId
        Some(other) => Err(AppError::NotFound(format!("navId not found: {}", other))),
    }
}

/// POST /api/invokeAction — invoke an action on a record.
///
/// Currently supported actions: "reload" (server-wide config reload).
pub async fn invoke_action(
    State(handle): State<EngineHandle>,
    Json(req): Json<super::types::InvokeActionRequest>,
) -> Result<Response, AppError> {
    match req.action.as_str() {
        "reload" => {
            let summary = handle.reload_config().await.map_err(AppError::from)?;
            Ok(Json(serde_json::json!({ "ok": true, "summary": summary })).into_response())
        }
        other => Err(AppError::BadRequest(format!("unknown action: {}", other))),
    }
}

/// Parse hisRead `range` parameter into (since_unix, until_unix) bounds.
fn parse_his_range(range: Option<&str>) -> (u64, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    match range {
        None | Some("") => (0, u64::MAX),
        Some("today") => {
            let midnight = now - (now % 86400);
            (midnight, u64::MAX)
        }
        Some("yesterday") => {
            let midnight_today = now - (now % 86400);
            let midnight_yesterday = midnight_today - 86400;
            (midnight_yesterday, midnight_today)
        }
        Some("last24h") => (now.saturating_sub(86400), u64::MAX),
        Some(range_str) => {
            // Try "YYYY-MM-DD,YYYY-MM-DD" format
            if let Some((start, end)) = range_str.split_once(',') {
                let since = parse_date_to_epoch(start.trim()).unwrap_or(0);
                // End date is inclusive: use end-of-day (+ 86400 - 1)
                let until = parse_date_to_epoch(end.trim())
                    .map(|e| e + 86400 - 1)
                    .unwrap_or(u64::MAX);
                (since, until)
            } else {
                (0, u64::MAX)
            }
        }
    }
}

/// Parse "YYYY-MM-DD" → Unix epoch seconds (midnight UTC).
fn parse_date_to_epoch(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Reverse of Howard Hinnant's civil_from_days
    let y_adj = if m <= 2 { y - 1 } else { y };
    let m_adj = if m <= 2 { m + 9 } else { m - 3 };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = (y_adj - era * 400) as u64;
    let doy = (153 * m_adj as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days * 86400)
}

// ── Engine Management ───────────────────────────────────────

/// GET /api/status — engine status info.
pub async fn status(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
) -> Result<Response, AppError> {
    let info = handle.status().await.map_err(AppError::from)?;
    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::status_to_zinc(&info).to_zinc(),
        ));
    }
    Ok(Json(serde_json::json!({
        "uptimeSecs": info.uptime_secs,
        "channelCount": info.channel_count,
        "pollCount": info.poll_count,
        "tableCount": info.table_count,
        "pollIntervalMs": info.poll_interval_ms,
    }))
    .into_response())
}

/// GET /health — lightweight health check for load balancers / systemd.
pub async fn health(State(handle): State<EngineHandle>) -> Json<serde_json::Value> {
    match handle.status().await {
        Ok(info) => Json(serde_json::json!({
            "healthy": true,
            "uptimeSecs": info.uptime_secs,
        })),
        Err(_) => Json(serde_json::json!({
            "healthy": false,
            "uptimeSecs": 0,
        })),
    }
}

/// GET /api/metrics — internal server metrics.
pub async fn metrics_endpoint() -> Json<serde_json::Value> {
    Json(crate::metrics::metrics().to_json())
}

/// GET /api/diagnostics — engine diagnostics (poll timing, channel health, I2C backoff).
pub async fn diagnostics(
    State(handle): State<EngineHandle>,
) -> Result<Json<sandstar_ipc::types::DiagnosticsInfo>, AppError> {
    let info = handle.diagnostics().await.map_err(AppError::from)?;
    Ok(Json(info))
}

/// GET /api/channels — list all channels.
pub async fn channels(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
) -> Result<Response, AppError> {
    let list = handle.list_channels().await.map_err(AppError::from)?;
    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::channels_to_zinc(&list).to_zinc(),
        ));
    }
    let rows: Vec<serde_json::Value> = list.iter().map(channel_info_json).collect();
    Ok(Json(rows).into_response())
}

/// GET /api/polls — list polled channels.
pub async fn polls(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
) -> Result<Response, AppError> {
    let list = handle.list_polls().await.map_err(AppError::from)?;
    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::polls_to_zinc(&list).to_zinc(),
        ));
    }
    let rows: Vec<serde_json::Value> = list
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "channel": p.channel,
                "lastCur": p.last_cur,
                "lastStatus": p.last_status,
            })
        })
        .collect();
    Ok(Json(rows).into_response())
}

/// GET /api/tables — list lookup tables.
pub async fn tables(State(handle): State<EngineHandle>) -> Result<Json<Vec<String>>, AppError> {
    let list = handle.list_tables().await.map_err(AppError::from)?;
    Ok(Json(list))
}

/// GET /api/history/:channel — query channel value history.
///
/// Query params:
/// - `since` (optional): Unix epoch seconds — return points from this timestamp onward
/// - `duration` (optional): "1h" | "24h" | "7d" — shorthand for since=now-duration
/// - `limit` (optional): Maximum points to return (default 100, max 10000)
pub async fn history(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Path(channel): Path<u32>,
    Query(params): Query<HistoryParams>,
) -> Result<Response, AppError> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let since_unix = if let Some(ref dur) = params.duration {
        let secs = parse_duration(dur);
        now_unix.saturating_sub(secs)
    } else {
        params.since.unwrap_or(0)
    };

    let limit = params.limit.unwrap_or(100).min(10000);

    let points = handle
        .get_history(channel, since_unix, limit)
        .await
        .map_err(AppError::from)?;

    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::history_to_zinc(&points).to_zinc(),
        ));
    }
    Ok(Json(points).into_response())
}

/// Parse duration strings like "1h", "24h", "7d", "30m" into seconds.
fn parse_duration(s: &str) -> u64 {
    let s = s.trim();
    if let Some(hours) = s.strip_suffix('h') {
        hours.parse::<u64>().unwrap_or(1) * 3600
    } else if let Some(days) = s.strip_suffix('d') {
        days.parse::<u64>().unwrap_or(1) * 86400
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>().unwrap_or(1) * 60
    } else {
        s.parse::<u64>().unwrap_or(3600) // default: 1 hour
    }
}

/// POST /api/pointWrite — write a value at a priority level.
///
/// Body: { "channel": 2001, "value": 1.0, "level": 17, "who": "rest", "duration": 0 }
/// If `value` is null, relinquishes the level.
/// Always returns the updated 17-row priority grid.
pub async fn point_write(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Json(req): Json<PointWriteRequest>,
) -> Result<Response, AppError> {
    handle
        .write_channel(req.channel, req.value, req.level, req.who, req.duration)
        .await
        .map_err(AppError::from)?;

    // Return the updated priority grid
    let levels = handle
        .get_write_levels(req.channel)
        .await
        .map_err(AppError::from)?;

    if wants_zinc(&headers) {
        Ok(zinc_response(
            super::zinc_format::write_levels_to_zinc(&levels).to_zinc(),
        ))
    } else {
        Ok(Json(levels).into_response())
    }
}

/// GET /api/pointWrite?channel=X — read the 17-level priority array.
pub async fn point_write_read(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Query(params): Query<PointWriteReadParams>,
) -> Result<Response, AppError> {
    let levels = handle
        .get_write_levels(params.channel)
        .await
        .map_err(AppError::from)?;

    if wants_zinc(&headers) {
        Ok(zinc_response(
            super::zinc_format::write_levels_to_zinc(&levels).to_zinc(),
        ))
    } else {
        Ok(Json(levels).into_response())
    }
}

/// POST /api/pollNow — trigger an immediate async poll cycle.
pub async fn poll_now(
    State(handle): State<EngineHandle>,
) -> Result<Json<serde_json::Value>, AppError> {
    let msg = handle.poll_now().await.map_err(AppError::from)?;
    Ok(Json(serde_json::json!({ "ok": true, "message": msg })))
}

/// POST /api/reload — reload configuration from disk.
pub async fn reload(
    State(handle): State<EngineHandle>,
) -> Result<Json<serde_json::Value>, AppError> {
    let summary = handle.reload_config().await.map_err(AppError::from)?;
    Ok(Json(serde_json::json!({ "ok": true, "summary": summary })))
}

// ── Watch Subscriptions ─────────────────────────────────────

/// POST /api/watchSub — create or extend a watch subscription.
pub async fn watch_sub(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Json(req): Json<WatchSubRequest>,
) -> Result<Response, AppError> {
    let resp = handle
        .watch_sub(req.watch_id, req.dis, req.ids)
        .await
        .map_err(AppError::from)?;
    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::watch_to_zinc(&resp).to_zinc(),
        ));
    }
    Ok(Json(resp).into_response())
}

/// POST /api/watchUnsub — unsubscribe channels or close a watch.
pub async fn watch_unsub(
    State(handle): State<EngineHandle>,
    Json(req): Json<WatchUnsubRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    handle
        .watch_unsub(req.watch_id, req.close, req.ids.unwrap_or_default())
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/watchPoll — poll for changed values since last poll.
pub async fn watch_poll(
    headers: HeaderMap,
    State(handle): State<EngineHandle>,
    Json(req): Json<WatchPollRequest>,
) -> Result<Response, AppError> {
    let resp = handle
        .watch_poll(req.watch_id, req.refresh)
        .await
        .map_err(AppError::from)?;
    if wants_zinc(&headers) {
        return Ok(zinc_response(
            super::zinc_format::watch_to_zinc(&resp).to_zinc(),
        ));
    }
    Ok(Json(resp).into_response())
}

// ── Helpers ─────────────────────────────────────────────────

/// Parse "channel==N" fast-path filter.
fn parse_channel_eq(filter: &str) -> Option<u32> {
    let f = filter.trim();
    f.strip_prefix("channel==")
        .or_else(|| f.strip_prefix("channel =="))
        .and_then(|rest| rest.trim().parse().ok())
}

/// Fallback substring matching when filter parsing fails.
fn matches_simple(ch: &sandstar_ipc::types::ChannelInfo, filter: &str) -> bool {
    let f = filter.to_lowercase();
    ch.label.to_lowercase().contains(&f)
        || ch.channel_type.to_lowercase().contains(&f)
        || ch.direction.to_lowercase().contains(&f)
}

/// Format ChannelValue as JSON.
fn channel_value_json(val: &super::ChannelValue) -> serde_json::Value {
    serde_json::json!({
        "channel": val.channel,
        "status": val.status,
        "raw": val.raw,
        "cur": val.cur,
    })
}

/// Format ChannelInfo as JSON.
fn channel_info_json(ch: &sandstar_ipc::types::ChannelInfo) -> serde_json::Value {
    serde_json::json!({
        "id": ch.id,
        "label": ch.label,
        "type": ch.channel_type,
        "direction": ch.direction,
        "enabled": ch.enabled,
        "status": ch.status,
        "cur": ch.cur,
        "raw": ch.raw,
    })
}

/// Convert Unix epoch seconds to ISO 8601 UTC string.
pub fn epoch_to_string(secs: u64) -> String {
    const SECS_PER_DAY: u64 = 86400;
    let day_secs = secs % SECS_PER_DAY;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;

    // Howard Hinnant's civil_from_days: days since 1970-01-01 → (y, m, d)
    // Shift epoch from 1970-01-01 to 0000-03-01 by adding 719468 days
    let z = secs / SECS_PER_DAY + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", yr, mo, d, h, m, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_date_to_epoch_valid() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let epoch = parse_date_to_epoch("2024-01-01").unwrap();
        assert_eq!(epoch, 1704067200);
    }

    #[test]
    fn parse_date_to_epoch_another() {
        // 1970-01-01 = 0
        let epoch = parse_date_to_epoch("1970-01-01").unwrap();
        assert_eq!(epoch, 0);
    }

    #[test]
    fn parse_date_to_epoch_invalid() {
        assert!(parse_date_to_epoch("not-a-date").is_none());
        assert!(parse_date_to_epoch("2024-13-01").is_none()); // month 13
        assert!(parse_date_to_epoch("2024-00-01").is_none()); // month 0
    }

    #[test]
    fn his_range_none_returns_full() {
        let (since, until) = parse_his_range(None);
        assert_eq!(since, 0);
        assert_eq!(until, u64::MAX);
    }

    #[test]
    fn his_range_last24h() {
        let (since, until) = parse_his_range(Some("last24h"));
        assert!(since > 0);
        assert_eq!(until, u64::MAX);
        // since should be roughly now - 86400
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!((now - since).abs_diff(86400) < 2);
    }

    #[test]
    fn his_range_date_range() {
        let (since, until) = parse_his_range(Some("2024-01-01,2024-01-31"));
        assert_eq!(since, 1704067200); // 2024-01-01
                                       // until should be end of 2024-01-31
        let expected_end = parse_date_to_epoch("2024-01-31").unwrap() + 86400 - 1;
        assert_eq!(until, expected_end);
    }

    // ════════════════════════════════════════════════════════════
    // Phase 5.8i — Handler edge case unit tests
    // ════════════════════════════════════════════════════════════

    #[test]
    fn parse_channel_eq_valid() {
        assert_eq!(parse_channel_eq("channel==1113"), Some(1113));
        assert_eq!(parse_channel_eq("channel == 1113"), Some(1113));
        assert_eq!(parse_channel_eq("  channel==612  "), Some(612));
    }

    #[test]
    fn parse_channel_eq_invalid() {
        // Not a channel== filter
        assert_eq!(parse_channel_eq("analog"), None);
        assert_eq!(parse_channel_eq("channel > 100"), None);
        // Non-numeric value
        assert_eq!(parse_channel_eq("channel==abc"), None);
    }

    #[test]
    fn matches_simple_label() {
        let ch = sandstar_ipc::types::ChannelInfo {
            id: 1113,
            label: "AI1 Thermistor 10K".into(),
            channel_type: "Analog".into(),
            direction: "In".into(),
            enabled: true,
            status: "Ok".into(),
            cur: 72.5,
            raw: 2048.0,
        };
        assert!(matches_simple(&ch, "thermistor"));
        assert!(matches_simple(&ch, "Analog"));
        assert!(matches_simple(&ch, "in"));
        assert!(!matches_simple(&ch, "humidity"));
    }

    #[test]
    fn his_range_yesterday() {
        let (since, until) = parse_his_range(Some("yesterday"));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let midnight_today = now - (now % 86400);
        let midnight_yesterday = midnight_today - 86400;
        assert_eq!(since, midnight_yesterday);
        assert_eq!(until, midnight_today);
    }

    #[test]
    fn his_range_empty_string_returns_full() {
        let (since, until) = parse_his_range(Some(""));
        assert_eq!(since, 0);
        assert_eq!(until, u64::MAX);
    }

    #[test]
    fn his_range_invalid_date_range_returns_full() {
        // Single date (no comma) — falls through to default
        let (since, until) = parse_his_range(Some("2024-01-01"));
        assert_eq!(since, 0);
        assert_eq!(until, u64::MAX);
    }

    #[test]
    fn his_range_malformed_dates_in_range() {
        // Both dates invalid
        let (since, until) = parse_his_range(Some("bogus,bogus"));
        assert_eq!(since, 0);
        assert_eq!(until, u64::MAX);
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("1h"), 3600);
        assert_eq!(parse_duration("24h"), 86400);
        assert_eq!(parse_duration("7d"), 604800);
        assert_eq!(parse_duration("30m"), 1800);
        // Plain number = seconds (fallback to 1h if not parseable)
        assert_eq!(parse_duration("3600"), 3600);
        // Invalid suffix = fallback
        assert_eq!(parse_duration("xyz"), 3600);
    }

    #[test]
    fn epoch_to_string_format() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let s = epoch_to_string(1704067200);
        assert_eq!(s, "2024-01-01T00:00:00Z");

        // Unix epoch
        let s = epoch_to_string(0);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn parse_date_to_epoch_day_32_rejected() {
        assert!(parse_date_to_epoch("2024-01-32").is_none());
    }

    #[test]
    fn parse_date_to_epoch_day_0_rejected() {
        assert!(parse_date_to_epoch("2024-01-00").is_none());
    }
}
