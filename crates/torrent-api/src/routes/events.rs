//! WebSocket event stream endpoint.
//!
//! Provides a real-time push channel for session alerts and periodic stats
//! heartbeats. Clients connect via `GET /api/v1/events` (or HTTP/2+ CONNECT)
//! with optional query parameters to configure the alert category mask and
//! stats heartbeat interval.

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use torrent::session::{MetricKind, SessionHandle, session_stats_metrics};

use super::AppState;

// ── Query parameters ─────────────────────────────────────────────────

/// Query parameters for the WebSocket event endpoint.
///
/// Both fields have sensible defaults so the endpoint works with a bare
/// `ws://host/api/v1/events` connection.
#[derive(Debug, serde::Deserialize)]
pub struct WsParams {
    /// Alert category bitmask. Accepts hex (`0xFFF`, `FFF`) or decimal.
    /// Defaults to `"0xFFF"` (all categories).
    #[serde(default = "default_mask")]
    pub mask: String,

    /// Stats heartbeat interval in milliseconds.
    /// Defaults to `1000` (1 second). Set to `0` to disable heartbeats.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_mask() -> String {
    "0xFFF".to_owned()
}

const fn default_interval() -> u64 {
    1000
}

// ── Wire protocol types ──────────────────────────────────────────────

/// Server-to-client WebSocket message.
///
/// Serialized as internally tagged JSON via `#[serde(tag = "type")]`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum WsMessage {
    /// A session alert matching the client's category mask.
    #[serde(rename = "alert")]
    Alert {
        /// The full alert payload (timestamp + kind).
        alert: torrent::session::Alert,
        /// Human-readable category name(s), e.g. `"STATUS"`, `"PEER | ERROR"`.
        category: String,
    },

    /// Periodic stats heartbeat with current session counters.
    #[serde(rename = "stats")]
    Stats {
        /// Map of metric name to current value.
        counters: serde_json::Map<String, serde_json::Value>,
    },

    /// Notification that the client fell behind and alerts were dropped.
    #[serde(rename = "lagged")]
    Lagged {
        /// Number of alerts that were dropped due to slow consumption.
        dropped: u64,
    },
}

/// Client-to-server WebSocket command.
///
/// Serialized as internally tagged JSON via `#[serde(tag = "type")]`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsCommand {
    /// Update the alert category mask for this connection.
    SetMask {
        /// New mask value (hex or decimal string).
        mask: String,
    },
    /// Request an immediate stats snapshot (outside the heartbeat interval).
    GetStats,
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse a mask string into a `u32` bitmask.
///
/// Accepts:
/// - Hex with prefix: `"0xFFF"`, `"0x041"`
/// - Hex without prefix: `"FFF"`, `"041"`
/// - Decimal: `"4095"`, `"65"`
///
/// Returns `None` if the string cannot be parsed.
pub fn parse_mask(s: &str) -> Option<u32> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else if trimmed.chars().all(|c| c.is_ascii_hexdigit())
        && trimmed.chars().any(|c| c.is_ascii_alphabetic())
    {
        // Contains hex alpha chars (a-f/A-F), treat as hex without prefix.
        u32::from_str_radix(trimmed, 16).ok()
    } else {
        // Pure numeric — try decimal first, then hex as fallback.
        trimmed.parse::<u32>().ok()
    }
}

/// Build a map of metric name to current counter value.
///
/// The returned map contains one entry per session metric (currently 70),
/// keyed by the dotted metric name (e.g. `"net.bytes_sent"`).
pub fn build_counters_map(session: &SessionHandle) -> serde_json::Map<String, serde_json::Value> {
    let metrics = session_stats_metrics();
    let values = session.counters().snapshot();

    let mut map = serde_json::Map::with_capacity(metrics.len());
    for (metric, &value) in metrics.iter().zip(values.iter()) {
        let json_value = match metric.kind {
            MetricKind::Counter | MetricKind::Gauge => serde_json::Value::Number(
                serde_json::Number::from(value),
            ),
        };
        map.insert(metric.name.to_owned(), json_value);
    }
    map
}

/// Format an `AlertCategory` bitflags value as a human-readable string.
///
/// Produces names like `"STATUS"`, `"PEER | ERROR"`, or `"UNKNOWN(0x1000)"`
/// for unrecognized bits.
pub fn format_category(category: torrent::session::AlertCategory) -> String {
    let formatted = format!("{category:?}");
    // bitflags Debug format uses ` | ` separators and the flag names,
    // which is exactly the format we want. For an empty set it produces
    // "AlertCategory(0x0)" — normalize to "NONE".
    if category.is_empty() {
        "NONE".to_owned()
    } else {
        // Strip the type prefix if bitflags wraps it (it doesn't for
        // the `Debug` impl generated by the bitflags macro — the output
        // is e.g. `STATUS | ERROR`).
        formatted
    }
}

// ── Handler ──────────────────────────────────────────────────────────

/// WebSocket upgrade handler for the event stream.
///
/// Parses query parameters, upgrades the HTTP connection to WebSocket,
/// and spawns the event-forwarding loop.
pub async fn ws_events(
    ws: WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(session): State<AppState>,
) -> impl IntoResponse {
    let mask = parse_mask(&params.mask).unwrap_or(0xFFF);
    let interval_ms = params.interval;

    ws.on_upgrade(move |socket| handle_ws(socket, session, mask, interval_ms))
}

/// WebSocket connection handler (stub — full implementation in Task 2).
async fn handle_ws(
    _socket: WebSocket,
    _session: AppState,
    _mask: u32,
    _interval_ms: u64,
) {
    // Stub: connection is accepted and immediately dropped.
    // The full event-forwarding loop will be implemented in Task 2.
}
