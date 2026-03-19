//! Request/response DTOs for the REST API.

use serde::{Deserialize, Serialize};

/// GET /api/read query parameters.
#[derive(Debug, Deserialize)]
pub struct ReadParams {
    /// Haystack filter expression (e.g., "channel==1113", "point").
    pub filter: Option<String>,
    /// Read a single channel by ID.
    pub id: Option<u32>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// POST /api/pointWrite request body.
#[derive(Debug, Deserialize)]
pub struct PointWriteRequest {
    pub channel: u32,
    /// Value to write (null = relinquish this level).
    pub value: Option<f64>,
    /// Priority level 1-17 (default 17 = lowest).
    #[serde(default = "default_write_level")]
    pub level: u8,
    /// Who is writing (for audit trail).
    #[serde(default)]
    pub who: String,
    /// Duration in seconds (0 = permanent).
    #[serde(default)]
    pub duration: f64,
}

fn default_write_level() -> u8 {
    17
}

/// GET /api/pointWrite query parameters.
#[derive(Debug, Deserialize)]
pub struct PointWriteReadParams {
    pub channel: u32,
}

/// GET /api/about response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AboutResponse {
    pub server_name: String,
    pub vendor_name: &'static str,
    pub product_name: &'static str,
    pub product_version: &'static str,
    pub haystack_version: &'static str,
    pub server_time: String,
    pub server_boot_time: String,
    pub build_info: &'static str,
}

/// GET /api/ops — single operation entry.
#[derive(Debug, Serialize)]
pub struct OpEntry {
    pub name: &'static str,
    pub summary: &'static str,
}

/// GET /api/history/:channel query parameters.
#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    /// Unix epoch seconds: return points from this timestamp onward.
    pub since: Option<u64>,
    /// Duration shorthand: "1h", "24h", "7d".
    pub duration: Option<String>,
    /// Maximum points to return (default 100, max 10000).
    pub limit: Option<usize>,
}

/// POST /api/hisRead request body.
#[derive(Debug, Deserialize)]
pub struct HisReadRequest {
    /// Channel/point ID to read history for.
    pub id: u32,
    /// Optional range: "today", "yesterday", "last24h", or "YYYY-MM-DD,YYYY-MM-DD".
    pub range: Option<String>,
}

/// POST /api/nav request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavRequest {
    /// Navigation ID — None for root, "site" for equipment, "equip:X" for points.
    pub nav_id: Option<String>,
}

/// POST /api/invokeAction request body.
#[derive(Debug, Deserialize)]
pub struct InvokeActionRequest {
    /// Target record ID (ignored for server-level actions).
    pub id: Option<String>,
    /// Action name to invoke.
    pub action: String,
}

/// GET /api/formats response entry.
#[derive(Debug, Serialize)]
pub struct FormatEntry {
    pub mime: &'static str,
    pub receive: bool,
    pub send: bool,
}

/// Standard formats list.
pub fn formats_list() -> Vec<FormatEntry> {
    vec![
        FormatEntry { mime: "application/json", receive: true, send: true },
        FormatEntry { mime: "text/zinc", receive: true, send: true },
        FormatEntry { mime: "text/plain", receive: false, send: true },
    ]
}

/// Standard ops list.
pub fn ops_list() -> Vec<OpEntry> {
    vec![
        OpEntry { name: "about", summary: "Server metadata" },
        OpEntry { name: "ops", summary: "List available operations" },
        OpEntry { name: "formats", summary: "List supported MIME types" },
        OpEntry { name: "read", summary: "Read points by filter or ID" },
        OpEntry { name: "hisRead", summary: "Read historical trend data" },
        OpEntry { name: "nav", summary: "Navigate site/equip/point tree" },
        OpEntry { name: "invokeAction", summary: "Invoke action on a record" },
        OpEntry { name: "status", summary: "Engine status" },
        OpEntry { name: "channels", summary: "List all channels" },
        OpEntry { name: "polls", summary: "List polled channels" },
        OpEntry { name: "tables", summary: "List lookup tables" },
        OpEntry { name: "pointWrite", summary: "Write to output channel" },
        OpEntry { name: "pollNow", summary: "Trigger immediate poll cycle" },
        OpEntry { name: "reload", summary: "Reload configuration from disk" },
        OpEntry { name: "watchSub", summary: "Subscribe to channel changes" },
        OpEntry { name: "watchUnsub", summary: "Unsubscribe or close watch" },
        OpEntry { name: "watchPoll", summary: "Poll for changed values" },
        OpEntry { name: "history", summary: "Query channel value history" },
    ]
}

// ── Watch request DTOs ──────────────────────────────────────

/// POST /api/watchSub request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSubRequest {
    /// Existing watch ID to add channels to.
    pub watch_id: Option<String>,
    /// Display name for a new watch.
    pub dis: Option<String>,
    /// Channel IDs to subscribe.
    pub ids: Vec<u32>,
}

/// POST /api/watchUnsub request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchUnsubRequest {
    /// Watch ID to unsubscribe from.
    pub watch_id: String,
    /// Channel IDs to remove (if absent and close=true, close entire watch).
    pub ids: Option<Vec<u32>>,
    /// Close the entire watch.
    #[serde(default)]
    pub close: bool,
}

/// POST /api/watchPoll request body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchPollRequest {
    /// Watch ID to poll.
    pub watch_id: String,
    /// If true, return all subscribed channels (not just changed).
    #[serde(default)]
    pub refresh: bool,
}
