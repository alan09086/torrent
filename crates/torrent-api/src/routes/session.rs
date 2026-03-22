//! Session-level endpoint handlers.
//!
//! Provides handlers for querying session statistics, reading and updating
//! settings, inspecting counters, and requesting a graceful shutdown.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use torrent::session::{MetricKind, session_stats_metrics};

use crate::error::{ApiError, ApiResult};

use super::AppState;

/// JSON representation of a single session counter entry.
#[derive(serde::Serialize)]
struct CounterEntry {
    name: &'static str,
    kind: &'static str,
    value: i64,
}

/// Get aggregate session statistics.
///
/// Returns a JSON object with overall session metrics such as total
/// download/upload bytes, peer counts, and disk cache stats.
pub async fn get_stats(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    let stats = session.session_stats().await?;
    Ok(Json(stats))
}

/// Get all session counters with metric metadata.
///
/// Returns a JSON array of `{ name, kind, value }` objects where `kind`
/// is either `"counter"` (monotonically increasing) or `"gauge"` (point-
/// in-time value). The array is indexed identically to the static metric
/// descriptors.
pub async fn get_counters(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    let metrics = session_stats_metrics();
    let values = session.counters().snapshot();
    let entries: Vec<CounterEntry> = metrics
        .iter()
        .zip(values.iter())
        .map(|(m, &v)| CounterEntry {
            name: m.name,
            kind: match m.kind {
                MetricKind::Counter => "counter",
                MetricKind::Gauge => "gauge",
            },
            value: v,
        })
        .collect();
    Ok(Json(entries))
}

/// Get current session settings.
///
/// Returns the full [`Settings`](torrent::session::Settings) object as JSON.
pub async fn get_settings(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    let settings = session.settings().await?;
    Ok(Json(settings))
}

/// Update session settings via JSON Merge Patch (RFC 7396).
///
/// Accepts a partial JSON object. Null values remove keys, non-null values
/// are merged recursively. The merged result is validated before being
/// applied to the running session.
///
/// Returns the full updated settings on success.
pub async fn patch_settings(
    State(session): State<AppState>,
    Json(patch): Json<serde_json::Value>,
) -> ApiResult<impl IntoResponse> {
    // 1. Get current settings.
    let current = session.settings().await?;

    // 2. Serialize current to a JSON Value.
    let mut current_value = serde_json::to_value(&current)
        .map_err(|e| ApiError::internal(format!("failed to serialize settings: {e}")))?;

    // 3. Merge patch onto current (RFC 7396 JSON Merge Patch).
    json_merge_patch(&mut current_value, &patch);

    // 4. Deserialize back to Settings.
    let updated: torrent::session::Settings = serde_json::from_value(current_value)
        .map_err(|e| ApiError::bad_request(format!("invalid settings: {e}")))?;

    // 5. Validate the merged result.
    updated.validate()?;

    // 6. Apply to the running session.
    session.apply_settings(updated).await?;

    // 7. Return the new authoritative settings.
    let new_settings = session.settings().await?;
    Ok(Json(new_settings))
}

/// Initiate a graceful session shutdown.
///
/// Returns **204 No Content** on success. After this call, the session
/// begins tearing down all torrents and connections.
pub async fn shutdown(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    session.shutdown().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// RFC 7396 JSON Merge Patch.
///
/// Recursively merges `patch` into `target`:
/// - If `patch` is an object, each key is merged into `target` (which is
///   coerced to an object if it is not already one).
/// - Null values in `patch` remove the corresponding key from `target`.
/// - Non-object patches replace `target` wholesale.
fn json_merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let serde_json::Value::Object(patch_obj) = patch {
        if !target.is_object() {
            *target = serde_json::Value::Object(serde_json::Map::new());
        }
        // SAFETY: we just ensured target is an Object above.
        let target_obj = target
            .as_object_mut()
            .expect("target is guaranteed to be an object");
        for (key, value) in patch_obj {
            if value.is_null() {
                target_obj.remove(key);
            } else {
                let entry = target_obj
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null);
                json_merge_patch(entry, value);
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_patch_replaces_scalar() {
        let mut target = serde_json::json!({"a": 1, "b": 2});
        let patch = serde_json::json!({"a": 10});
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!({"a": 10, "b": 2}));
    }

    #[test]
    fn merge_patch_removes_null() {
        let mut target = serde_json::json!({"a": 1, "b": 2});
        let patch = serde_json::json!({"b": null});
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!({"a": 1}));
    }

    #[test]
    fn merge_patch_adds_new_key() {
        let mut target = serde_json::json!({"a": 1});
        let patch = serde_json::json!({"b": 2});
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn merge_patch_nested_object() {
        let mut target = serde_json::json!({"a": {"x": 1, "y": 2}});
        let patch = serde_json::json!({"a": {"y": 20, "z": 30}});
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!({"a": {"x": 1, "y": 20, "z": 30}}));
    }

    #[test]
    fn merge_patch_non_object_target_coerced() {
        let mut target = serde_json::json!(42);
        let patch = serde_json::json!({"a": 1});
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!({"a": 1}));
    }

    #[test]
    fn merge_patch_scalar_replaces_object() {
        let mut target = serde_json::json!({"a": 1});
        let patch = serde_json::json!(42);
        json_merge_patch(&mut target, &patch);
        assert_eq!(target, serde_json::json!(42));
    }
}
