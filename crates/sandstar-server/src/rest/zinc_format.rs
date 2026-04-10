//! Converters from REST response types → ZincGrid.
//!
//! Each function builds a typed Zinc 3.0 grid for a specific REST endpoint.
//! Used when `Accept: text/zinc` is present.

use super::types::{AboutResponse, OpEntry};
use super::zinc_grid::{ZincColumn, ZincGrid, ZincValue};
use super::{ChannelValue, WatchResponse};
use crate::history::HistoryPoint;
use sandstar_ipc::types::{ChannelInfo, PollInfo, StatusInfo};

// ── Helpers ─────────────────────────────────────────────────

fn num(v: f64) -> ZincValue {
    ZincValue::Num(v, None)
}

fn str_val(s: &str) -> ZincValue {
    ZincValue::Str(s.to_string())
}

fn bool_val(b: bool) -> ZincValue {
    ZincValue::Bool(b)
}

// ── Converters ──────────────────────────────────────────────

/// GET /api/about → Zinc grid.
pub fn about_to_zinc(resp: &AboutResponse) -> ZincGrid {
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("serverName"),
            ZincColumn::new("vendorName"),
            ZincColumn::new("productName"),
            ZincColumn::new("productVersion"),
            ZincColumn::new("haystackVersion"),
            ZincColumn::new("serverTime"),
            ZincColumn::new("serverBootTime"),
            ZincColumn::new("buildInfo"),
        ],
        rows: vec![vec![
            str_val(&resp.server_name),
            str_val(resp.vendor_name),
            str_val(resp.product_name),
            str_val(resp.product_version),
            str_val(resp.haystack_version),
            str_val(&resp.server_time),
            str_val(&resp.server_boot_time),
            str_val(resp.build_info),
        ]],
    }
}

/// GET /api/ops → Zinc grid.
pub fn ops_to_zinc(ops: &[OpEntry]) -> ZincGrid {
    let rows = ops
        .iter()
        .map(|op| vec![str_val(op.name), str_val(op.summary)])
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![ZincColumn::new("name"), ZincColumn::new("summary")],
        rows,
    }
}

/// GET /api/channels → Zinc grid.
pub fn channels_to_zinc(channels: &[ChannelInfo]) -> ZincGrid {
    let rows = channels
        .iter()
        .map(|ch| {
            vec![
                num(ch.id as f64),
                str_val(&ch.label),
                str_val(&ch.channel_type),
                str_val(&ch.direction),
                bool_val(ch.enabled),
                str_val(&ch.status),
                num(ch.cur),
                num(ch.raw),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("id"),
            ZincColumn::new("dis"),
            ZincColumn::new("type"),
            ZincColumn::new("direction"),
            ZincColumn::new("enabled"),
            ZincColumn::new("status"),
            ZincColumn::new("cur"),
            ZincColumn::new("raw"),
        ],
        rows,
    }
}

/// GET /api/read, GET /api/channels (values) → Zinc grid.
pub fn values_to_zinc(values: &[ChannelValue]) -> ZincGrid {
    let rows = values
        .iter()
        .map(|v| {
            vec![
                num(v.channel as f64),
                str_val(&v.status),
                num(v.raw),
                num(v.cur),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("channel"),
            ZincColumn::new("status"),
            ZincColumn::new("raw"),
            ZincColumn::new("cur"),
        ],
        rows,
    }
}

/// GET /api/polls → Zinc grid.
pub fn polls_to_zinc(polls: &[PollInfo]) -> ZincGrid {
    let rows = polls
        .iter()
        .map(|p| {
            vec![
                num(p.channel as f64),
                num(p.last_cur),
                str_val(&p.last_status),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("channel"),
            ZincColumn::new("lastCur"),
            ZincColumn::new("lastStatus"),
        ],
        rows,
    }
}

/// GET /api/status → Zinc grid (single row).
pub fn status_to_zinc(info: &StatusInfo) -> ZincGrid {
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("uptimeSecs"),
            ZincColumn::new("channelCount"),
            ZincColumn::new("pollCount"),
            ZincColumn::new("tableCount"),
            ZincColumn::new("pollIntervalMs"),
        ],
        rows: vec![vec![
            num(info.uptime_secs as f64),
            num(info.channel_count as f64),
            num(info.poll_count as f64),
            num(info.table_count as f64),
            num(info.poll_interval_ms as f64),
        ]],
    }
}

/// POST /api/watchSub, POST /api/watchPoll → Zinc grid.
pub fn watch_to_zinc(resp: &WatchResponse) -> ZincGrid {
    let rows = resp
        .rows
        .iter()
        .map(|v| {
            vec![
                num(v.channel as f64),
                str_val(&v.status),
                num(v.raw),
                num(v.cur),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![
            ("watchId", str_val(&resp.watch_id)),
            ("lease", ZincValue::Num(resp.lease as f64, Some("s"))),
        ],
        columns: vec![
            ZincColumn::new("channel"),
            ZincColumn::new("status"),
            ZincColumn::new("raw"),
            ZincColumn::new("cur"),
        ],
        rows,
    }
}

/// GET /api/history/:channel → Zinc grid.
pub fn history_to_zinc(points: &[HistoryPoint]) -> ZincGrid {
    let rows = points
        .iter()
        .map(|p| {
            vec![
                ZincValue::DateTime(crate::rest::handlers::epoch_to_string(p.ts)),
                num(p.cur),
                num(p.raw),
                str_val(p.status.as_str()),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("ts"),
            ZincColumn::new("cur"),
            ZincColumn::new("raw"),
            ZincColumn::new("status"),
        ],
        rows,
    }
}

/// GET /api/formats → Zinc grid.
pub fn formats_to_zinc(formats: &[super::types::FormatEntry]) -> ZincGrid {
    let rows = formats
        .iter()
        .map(|f| vec![str_val(f.mime), bool_val(f.receive), bool_val(f.send)])
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("mime"),
            ZincColumn::new("receive"),
            ZincColumn::new("send"),
        ],
        rows,
    }
}

/// POST /api/nav → Zinc grid.
///
/// Takes pre-built JSON rows and converts to Zinc. Each row has navId, dis,
/// and optionally nav (Marker) or point (Marker).
pub fn nav_to_zinc(rows: &[serde_json::Value]) -> ZincGrid {
    let zinc_rows = rows
        .iter()
        .map(|row| {
            let nav_id = row.get("navId").and_then(|v| v.as_str()).unwrap_or("");
            let dis = row.get("dis").and_then(|v| v.as_str()).unwrap_or("");
            let has_nav = row.get("nav").is_some();
            vec![
                str_val(nav_id),
                str_val(dis),
                if has_nav {
                    ZincValue::Marker
                } else {
                    ZincValue::Null
                },
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("navId"),
            ZincColumn::new("dis"),
            ZincColumn::new("nav"),
        ],
        rows: zinc_rows,
    }
}

/// pointWrite priority array → Zinc grid (4 columns: level, levelDis, val, who).
pub fn write_levels_to_zinc(levels: &[sandstar_ipc::types::WriteLevelInfo]) -> ZincGrid {
    let rows = levels
        .iter()
        .map(|l| {
            vec![
                num(l.level as f64),
                str_val(&l.level_dis),
                match l.val {
                    Some(v) => num(v),
                    None => ZincValue::Null,
                },
                str_val(&l.who),
            ]
        })
        .collect();
    ZincGrid {
        meta: vec![],
        columns: vec![
            ZincColumn::new("level"),
            ZincColumn::new("levelDis"),
            ZincColumn::new("val"),
            ZincColumn::new("who"),
        ],
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandstar_engine::EngineStatus;

    #[test]
    fn about_grid_has_correct_columns() {
        let resp = AboutResponse {
            server_name: "Test".into(),
            vendor_name: "EacIo",
            product_name: "sandstar-engine-server",
            product_version: "0.1.0",
            haystack_version: "3.0",
            server_time: "2024-01-01T00:00:00Z".into(),
            server_boot_time: "2024-01-01T00:00:00Z".into(),
            build_info: "0.1.0+abc1234",
        };
        let grid = about_to_zinc(&resp);
        assert_eq!(grid.columns.len(), 8);
        assert_eq!(grid.rows.len(), 1);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("serverName"));
        assert!(zinc.contains("\"Test\""));
    }

    #[test]
    fn ops_grid() {
        let ops = vec![
            OpEntry {
                name: "about",
                summary: "Server metadata",
            },
            OpEntry {
                name: "read",
                summary: "Read points",
            },
        ];
        let grid = ops_to_zinc(&ops);
        assert_eq!(grid.rows.len(), 2);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("\"about\""));
        assert!(zinc.contains("\"Read points\""));
    }

    #[test]
    fn channels_grid() {
        let channels = vec![ChannelInfo {
            id: 1113,
            label: "AI1 Therm".into(),
            channel_type: "Analog".into(),
            direction: "In".into(),
            enabled: true,
            status: "Ok".into(),
            cur: 72.5,
            raw: 2048.0,
        }];
        let grid = channels_to_zinc(&channels);
        assert_eq!(grid.columns.len(), 8);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("1113"));
        assert!(zinc.contains("72.5"));
    }

    #[test]
    fn values_grid() {
        let values = vec![ChannelValue {
            channel: 1113,
            status: "Ok".into(),
            raw: 2048.0,
            cur: 72.5,
        }];
        let grid = values_to_zinc(&values);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("channel,status,raw,cur\n"));
        assert!(zinc.contains("1113"));
    }

    #[test]
    fn polls_grid() {
        let polls = vec![PollInfo {
            channel: 1113,
            last_cur: 72.5,
            last_status: "Ok".into(),
        }];
        let grid = polls_to_zinc(&polls);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("1113"));
        assert!(zinc.contains("\"Ok\""));
    }

    #[test]
    fn status_grid() {
        let info = StatusInfo {
            uptime_secs: 3600,
            channel_count: 138,
            poll_count: 50,
            table_count: 16,
            poll_interval_ms: 1000,
        };
        let grid = status_to_zinc(&info);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("3600"));
        assert!(zinc.contains("138"));
    }

    #[test]
    fn watch_grid_has_meta() {
        let resp = WatchResponse {
            watch_id: "w-001".into(),
            lease: 120,
            rows: vec![],
        };
        let grid = watch_to_zinc(&resp);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("watchId:\"w-001\""));
        assert!(zinc.contains("lease:120s"));
    }

    #[test]
    fn history_grid() {
        let points = vec![
            HistoryPoint {
                ts: 1704067200,
                cur: 72.0,
                raw: 2048.0,
                status: EngineStatus::Ok,
            },
            HistoryPoint {
                ts: 1704067260,
                cur: 72.5,
                raw: 2050.0,
                status: EngineStatus::Ok,
            },
        ];
        let grid = history_to_zinc(&points);
        assert_eq!(grid.rows.len(), 2);
        let zinc = grid.to_zinc();
        assert!(zinc.contains("ts,cur,raw,status\n"));
        assert!(zinc.contains("72.5"));
    }
}
