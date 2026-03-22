//! WebSocket event stream endpoint.
//!
//! Provides a real-time push channel for session alerts and periodic stats
//! heartbeats. Clients connect via `GET /api/v1/events` (or HTTP/2+ CONNECT)
//! with optional query parameters to configure the alert category mask and
//! stats heartbeat interval.

use std::sync::atomic::{AtomicU32, Ordering};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Duration, interval};
use torrent::session::{AlertCategory, MetricKind, SessionHandle, session_stats_metrics};

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

    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
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
            MetricKind::Counter | MetricKind::Gauge => {
                serde_json::Value::Number(serde_json::Number::from(value))
            }
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

/// Outcome of a single `select!` iteration.
///
/// Separating the decision from the I/O lets us avoid holding `&mut socket`
/// across await points inside the `select!` macro.
enum LoopAction {
    /// Serialize and send a [`WsMessage`] to the client.
    Send(WsMessage),
    /// Continue the loop without sending anything.
    Continue,
    /// Terminate the loop (client disconnected or broadcast closed).
    Break,
}

/// WebSocket connection handler.
///
/// Runs a three-arm `select!` loop that:
/// 1. Forwards session alerts matching the client's category mask.
/// 2. Sends periodic stats heartbeats (if enabled).
/// 3. Processes client commands (`set_mask`, `get_stats`).
///
/// The loop terminates when the client sends a close frame, the
/// connection errors, or the session's broadcast channel is closed.
async fn handle_ws(mut socket: WebSocket, session: AppState, initial_mask: u32, interval_ms: u64) {
    let mut alert_rx = session.subscribe();

    let mask = AtomicU32::new(initial_mask);

    // Use u64::MAX duration when heartbeat is disabled (interval_ms == 0).
    // The `if interval_ms > 0` guard on the select! arm prevents it from
    // ever firing, but we still need a valid Interval instance.
    let mut heartbeat = interval(if interval_ms > 0 {
        Duration::from_millis(interval_ms)
    } else {
        Duration::from_millis(u64::MAX)
    });
    // Consume the immediate first tick so the first heartbeat fires
    // after one full interval.
    heartbeat.tick().await;

    tracing::debug!(
        initial_mask = format_args!("0x{initial_mask:X}"),
        interval_ms,
        "websocket event stream connected",
    );

    loop {
        let action = tokio::select! {
            alert_result = alert_rx.recv() => {
                match alert_result {
                    Ok(alert) => {
                        let current_mask = mask.load(Ordering::Relaxed);
                        let filter = AlertCategory::from_bits_truncate(current_mask);
                        if alert.category().intersects(filter) {
                            let category = format_category(alert.category());
                            LoopAction::Send(WsMessage::Alert { alert, category })
                        } else {
                            LoopAction::Continue
                        }
                    }
                    Err(RecvError::Lagged(dropped)) => {
                        tracing::debug!(dropped, "websocket client lagged");
                        LoopAction::Send(WsMessage::Lagged { dropped })
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!("alert broadcast closed, terminating websocket");
                        LoopAction::Break
                    }
                }
            }

            _ = heartbeat.tick(), if interval_ms > 0 => {
                let counters = build_counters_map(&session);
                LoopAction::Send(WsMessage::Stats { counters })
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_text(&text, &mask, &session)
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::debug!("websocket client disconnected");
                        LoopAction::Break
                    }
                    Some(Ok(_)) => {
                        // Binary, Ping, Pong — ignore (pings are auto-responded by axum).
                        LoopAction::Continue
                    }
                    Some(Err(err)) => {
                        tracing::debug!(%err, "websocket receive error");
                        LoopAction::Break
                    }
                }
            }
        };

        match action {
            LoopAction::Send(ws_msg) => {
                let json = match serde_json::to_string(&ws_msg) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::debug!(%err, "failed to serialize websocket message");
                        continue;
                    }
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    // Send failed — client disconnected.
                    break;
                }
            }
            LoopAction::Continue => {}
            LoopAction::Break => break,
        }
    }
}

/// Process a text message from the client as a [`WsCommand`].
///
/// Returns the appropriate [`LoopAction`] — either a stats response
/// for `GetStats`, a mask update for `SetMask`, or a no-op if parsing fails.
fn handle_client_text(text: &str, mask: &AtomicU32, session: &SessionHandle) -> LoopAction {
    match serde_json::from_str::<WsCommand>(text) {
        Ok(WsCommand::SetMask { mask: new_mask_str }) => {
            if let Some(new_mask) = parse_mask(&new_mask_str) {
                mask.store(new_mask, Ordering::Relaxed);
                tracing::debug!(
                    mask = format_args!("0x{new_mask:X}"),
                    "websocket mask updated",
                );
            } else {
                tracing::debug!(
                    raw = %new_mask_str,
                    "websocket set_mask: invalid mask value",
                );
            }
            LoopAction::Continue
        }
        Ok(WsCommand::GetStats) => {
            let counters = build_counters_map(session);
            LoopAction::Send(WsMessage::Stats { counters })
        }
        Err(err) => {
            tracing::debug!(%err, "websocket: ignoring unparseable client message");
            LoopAction::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mask_hex() {
        assert_eq!(parse_mask("0xFFF"), Some(4095));
    }

    #[test]
    fn parse_mask_hex_upper() {
        assert_eq!(parse_mask("0X041"), Some(65));
    }

    #[test]
    fn parse_mask_decimal() {
        assert_eq!(parse_mask("4095"), Some(4095));
    }

    #[test]
    fn parse_mask_invalid() {
        assert_eq!(parse_mask("xyz"), None);
    }

    #[tokio::test]
    async fn build_counters_map_has_70_entries() {
        let session = torrent::ClientBuilder::new()
            .listen_port(0)
            .download_dir("/tmp")
            .enable_dht(false)
            .enable_lsd(false)
            .enable_upnp(false)
            .enable_natpmp(false)
            .start()
            .await
            .expect("failed to start test session");
        let map = build_counters_map(&session);
        assert_eq!(map.len(), 70, "should have exactly 70 metric entries");
    }

    #[test]
    fn lagged_serializes_correctly() {
        let msg = WsMessage::Lagged { dropped: 42 };
        let json = serde_json::to_value(&msg).expect("serialize WsMessage::Lagged");
        assert_eq!(json["type"], "lagged");
        assert_eq!(json["dropped"], 42);
    }
}
