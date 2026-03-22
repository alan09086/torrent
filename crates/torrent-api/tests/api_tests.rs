//! Integration tests for the torrent HTTP REST API.
//!
//! Each test creates its own isolated session (no TCP server needed) and
//! exercises the full request-response cycle via `Router::oneshot()`.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tower::ServiceExt; // for oneshot()

use torrent_api::routes::build_router;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Create a minimal session with no network activity.
async fn test_session() -> torrent::session::SessionHandle {
    torrent::ClientBuilder::new()
        .listen_port(0)
        .download_dir("/tmp")
        .enable_dht(false)
        .enable_lsd(false)
        .enable_upnp(false)
        .enable_natpmp(false)
        .start()
        .await
        .expect("failed to start test session")
}

/// Build a router backed by a fresh test session.
async fn test_router() -> axum::Router {
    let session = test_session().await;
    build_router(session)
}

/// Send a request through the router and return (status, body bytes).
async fn request(router: &axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.clone().oneshot(req).await.expect("request failed");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body collect failed")
        .to_bytes()
        .to_vec();
    (status, body)
}

/// Parse a response body as a JSON `Value`.
fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("body is not valid JSON")
}

/// Build a synthetic `.torrent` file in v1 format.
fn make_test_torrent_bytes() -> Vec<u8> {
    use serde::Serialize;

    let data = vec![0xAB; 16384];
    let hash = torrent::core::sha1(&data);
    let mut pieces = Vec::new();
    pieces.extend_from_slice(hash.as_bytes());

    #[derive(Serialize)]
    struct Info<'a> {
        length: u64,
        name: &'a str,
        #[serde(rename = "piece length")]
        piece_length: u64,
        #[serde(with = "serde_bytes")]
        pieces: &'a [u8],
    }

    #[derive(Serialize)]
    struct Torrent<'a> {
        info: Info<'a>,
    }

    let t = Torrent {
        info: Info {
            length: data.len() as u64,
            name: "test_file.bin",
            piece_length: 16384,
            pieces: &pieces,
        },
    };

    torrent::bencode::to_bytes(&t).expect("bencode serialization failed")
}

// A 40-char hex hash that does not correspond to any real torrent.
const NONEXISTENT_HASH: &str = "0000000000000000000000000000000000000000";

// ---------------------------------------------------------------------------
// 1. Torrent CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_torrents_empty() {
    let router = test_router().await;
    let req = Request::get("/api/v1/torrents")
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;

    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test]
async fn test_add_torrent_magnet() {
    let router = test_router().await;
    let magnet = "magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test";
    let body_json = serde_json::json!({ "uri": magnet });

    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&body_json).expect("serialize"),
        ))
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    let v = json(&body);
    assert!(
        v.get("v1").is_some_and(|h| !h.is_null()),
        "response should contain non-null v1 hash"
    );
}

#[tokio::test]
async fn test_add_torrent_bytes() {
    let router = test_router().await;
    let torrent_bytes = make_test_torrent_bytes();

    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(torrent_bytes))
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    let v = json(&body);
    assert!(
        v.get("v1").is_some_and(|h| !h.is_null()),
        "response should contain non-null v1 hash"
    );
}

#[tokio::test]
async fn test_add_torrent_empty_body() {
    let router = test_router().await;
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let v = json(&body);
    assert_eq!(v["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn test_add_torrent_invalid_bytes() {
    let router = test_router().await;
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(vec![0xFF, 0xFE, 0x00, 0x01]))
        .expect("build request");

    let (status, _body) = request(&router, req).await;
    // Invalid .torrent bytes should produce a client or server error.
    assert!(
        status.is_client_error() || status.is_server_error(),
        "expected error status, got {status}"
    );
}

#[tokio::test]
async fn test_get_torrent_not_found() {
    let router = test_router().await;
    let url = format!("/api/v1/torrents/{NONEXISTENT_HASH}");
    let req = Request::get(&url)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let v = json(&body);
    assert_eq!(v["code"], "NOT_FOUND");
}

#[tokio::test]
async fn test_delete_torrent() {
    let router = test_router().await;
    let hash = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";

    // Add a torrent first via magnet.
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=test");
    let body_json = serde_json::json!({ "uri": magnet });
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&body_json).expect("serialize"),
        ))
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    // DELETE the torrent.
    let url = format!("/api/v1/torrents/{hash}");
    let req = Request::delete(&url)
        .body(Body::empty())
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // GET should now return 404.
    let req = Request::get(&url)
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let v = json(&body);
    assert_eq!(v["code"], "NOT_FOUND");
}

#[tokio::test]
async fn test_pause_resume_torrent() {
    let router = test_router().await;
    let hash = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d";

    // Add a magnet.
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=test");
    let body_json = serde_json::json!({ "uri": magnet });
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&body_json).expect("serialize"),
        ))
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Pause.
    let url = format!("/api/v1/torrents/{hash}/pause");
    let req = Request::post(&url)
        .body(Body::empty())
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Resume.
    let url = format!("/api/v1/torrents/{hash}/resume");
    let req = Request::post(&url)
        .body(Body::empty())
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// 2. Session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_stats() {
    let router = test_router().await;
    let req = Request::get("/api/v1/session/stats")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    assert!(
        v.get("active_torrents").is_some(),
        "response should contain active_torrents"
    );
}

#[tokio::test]
async fn test_session_counters() {
    let router = test_router().await;
    let req = Request::get("/api/v1/session/counters")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    let arr = v.as_array().expect("counters should be an array");
    assert_eq!(arr.len(), 70, "should have exactly 70 metric entries");

    // Each entry should have name, kind, and value.
    let first = &arr[0];
    assert!(first.get("name").is_some(), "entry should have name");
    assert!(first.get("kind").is_some(), "entry should have kind");
    assert!(first.get("value").is_some(), "entry should have value");
}

#[tokio::test]
async fn test_get_settings() {
    let router = test_router().await;
    let req = Request::get("/api/v1/session/settings")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    assert!(
        v.get("listen_port").is_some(),
        "settings should contain listen_port"
    );
}

#[tokio::test]
async fn test_patch_settings_no_op() {
    let router = test_router().await;

    // Get current settings.
    let req = Request::get("/api/v1/session/settings")
        .body(Body::empty())
        .expect("build request");
    let (_, before_body) = request(&router, req).await;
    let before = json(&before_body);

    // PATCH with empty object.
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/v1/session/settings")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .expect("build request");
    let (status, after_body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let after = json(&after_body);
    assert_eq!(
        before["max_peers_per_torrent"], after["max_peers_per_torrent"],
        "no-op patch should not change settings"
    );
}

#[tokio::test]
async fn test_patch_settings_update() {
    let router = test_router().await;

    let patch = serde_json::json!({ "max_peers_per_torrent": 50 });
    let req = Request::builder()
        .method("PATCH")
        .uri("/api/v1/session/settings")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&patch).expect("serialize")))
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    assert_eq!(v["max_peers_per_torrent"], 50);
}

#[tokio::test]
async fn test_shutdown() {
    let router = test_router().await;
    let req = Request::post("/api/v1/session/shutdown")
        .body(Body::empty())
        .expect("build request");

    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// 3. Extended endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_banned_peers_empty() {
    let router = test_router().await;
    let req = Request::get("/api/v1/peers/banned")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test]
async fn test_ban_unban_peer() {
    let router = test_router().await;

    // Ban a peer.
    let ban_body = serde_json::json!({ "ip": "192.168.1.100" });
    let req = Request::post("/api/v1/peers/ban")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&ban_body).expect("serialize"),
        ))
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it appears in the ban list.
    let req = Request::get("/api/v1/peers/banned")
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    let arr = v.as_array().expect("banned list should be an array");
    assert!(
        arr.iter().any(|ip| ip.as_str() == Some("192.168.1.100")),
        "banned list should contain the banned IP"
    );

    // Unban.
    let req = Request::delete("/api/v1/peers/ban/192.168.1.100")
        .body(Body::empty())
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it is gone.
    let req = Request::get("/api/v1/peers/banned")
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = json(&body);
    assert_eq!(v, serde_json::json!([]));
}

#[tokio::test]
async fn test_get_peers_not_found() {
    let router = test_router().await;
    let url = format!("/api/v1/torrents/{NONEXISTENT_HASH}/peers");
    let req = Request::get(&url)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let v = json(&body);
    assert_eq!(v["code"], "NOT_FOUND");
}

#[tokio::test]
async fn test_reannounce_not_found() {
    let router = test_router().await;
    let url = format!("/api/v1/torrents/{NONEXISTENT_HASH}/reannounce");
    let req = Request::post(&url)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let v = json(&body);
    assert_eq!(v["code"], "NOT_FOUND");
}

// ---------------------------------------------------------------------------
// 4. Error handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_invalid_hash_format() {
    let router = test_router().await;
    // "not-a-hash" is not 40 or 64 hex chars.
    let req = Request::get("/api/v1/torrents/not-a-hash")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let v = json(&body);
    assert_eq!(v["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn test_invalid_hash_length() {
    let router = test_router().await;
    let req = Request::get("/api/v1/torrents/abc")
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let v = json(&body);
    assert_eq!(v["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn test_post_shutdown_rejects_requests() {
    let router = test_router().await;

    // Shutdown the session.
    let req = Request::post("/api/v1/session/shutdown")
        .body(Body::empty())
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Give the actor a moment to exit and close its command channel.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // A subsequent stats query should fail with 503 SHUTTING_DOWN because
    // the session actor is gone and the command channel is closed.
    let req = Request::get("/api/v1/session/stats")
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let v = json(&body);
    assert_eq!(v["code"], "SHUTTING_DOWN");
}

#[tokio::test]
async fn test_error_response_format() {
    let router = test_router().await;
    let url = format!("/api/v1/torrents/{NONEXISTENT_HASH}");
    let req = Request::get(&url)
        .body(Body::empty())
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let v = json(&body);
    // Verify the standard error response shape.
    assert!(
        v.get("error").is_some(),
        "error response must have 'error' field"
    );
    assert!(
        v.get("code").is_some(),
        "error response must have 'code' field"
    );
    assert_eq!(v["code"], "NOT_FOUND");
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "error message should be a non-empty string"
    );
}

// ---------------------------------------------------------------------------
// 5. .torrent bytes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_add_torrent_bytes_success() {
    let router = test_router().await;
    let torrent_bytes = make_test_torrent_bytes();

    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(torrent_bytes))
        .expect("build request");

    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    let v = json(&body);
    assert!(
        v.get("v1").is_some_and(|h| !h.is_null()),
        "should contain non-null v1 info hash"
    );
}

#[tokio::test]
async fn test_add_torrent_bytes_then_list() {
    let router = test_router().await;
    let torrent_bytes = make_test_torrent_bytes();

    // Add.
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(torrent_bytes))
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    // List should now contain one entry.
    let req = Request::get("/api/v1/torrents")
        .body(Body::empty())
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::OK);

    let v = json(&body);
    let arr = v.as_array().expect("list should be an array");
    assert_eq!(arr.len(), 1, "should have exactly one torrent");
}

#[tokio::test]
async fn test_add_torrent_bytes_duplicate() {
    let router = test_router().await;
    let torrent_bytes = make_test_torrent_bytes();

    // First add.
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(torrent_bytes.clone()))
        .expect("build request");
    let (status, _) = request(&router, req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Second add (duplicate).
    let req = Request::post("/api/v1/torrents")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(torrent_bytes))
        .expect("build request");
    let (status, body) = request(&router, req).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let v = json(&body);
    assert_eq!(v["code"], "DUPLICATE_TORRENT");
}

// ---------------------------------------------------------------------------
// 6. WebSocket event stream
// ---------------------------------------------------------------------------

/// Start a real TCP-backed test server (needed for WebSocket tests).
async fn start_test_server() -> (std::net::SocketAddr, torrent::session::SessionHandle) {
    let session = test_session().await;
    let server = torrent_api::ApiServer::bind(
        "127.0.0.1:0".parse().expect("parse bind address"),
        session.clone(),
    )
    .await
    .expect("bind test server");
    let addr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    (addr, session)
}

#[tokio::test]
async fn test_ws_connect() {
    let (addr, _session) = start_test_server().await;
    let (ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=0"))
            .await
            .expect("websocket upgrade should succeed");
    drop(ws);
}

#[tokio::test]
async fn test_ws_get_stats_command() {
    let (addr, _session) = start_test_server().await;
    let (mut ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=0"))
            .await
            .expect("websocket connect");

    // Send get_stats command.
    ws.send(TungsteniteMessage::Text(
        r#"{"type":"get_stats"}"#.into(),
    ))
    .await
    .expect("send get_stats");

    // Read the stats response.
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timed out waiting for stats")
        .expect("stream ended")
        .expect("websocket error");

    let text = msg.into_text().expect("expected text message");
    let v: serde_json::Value = serde_json::from_str(&text).expect("parse JSON");
    assert_eq!(v["type"], "stats");
    let counters = v["counters"].as_object().expect("counters should be object");
    assert_eq!(counters.len(), 70, "should have exactly 70 counters");
}

#[tokio::test]
async fn test_ws_heartbeat() {
    let (addr, _session) = start_test_server().await;
    let (mut ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=100"))
            .await
            .expect("websocket connect");

    // Heartbeat should arrive within 200ms (interval=100ms + margin).
    let msg = tokio::time::timeout(Duration::from_millis(500), ws.next())
        .await
        .expect("timed out waiting for heartbeat")
        .expect("stream ended")
        .expect("websocket error");

    let text = msg.into_text().expect("expected text message");
    let v: serde_json::Value = serde_json::from_str(&text).expect("parse JSON");
    assert_eq!(v["type"], "stats", "heartbeat should be a stats message");
}

#[tokio::test]
async fn test_ws_alert_delivery() {
    let (addr, session) = start_test_server().await;
    let (mut ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=0"))
            .await
            .expect("websocket connect");

    // Add a magnet URI which triggers TorrentAdded alert (STATUS category).
    session
        .add_magnet_uri("magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test")
        .await
        .expect("add magnet");

    // Read messages until we find an alert or time out.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut found_alert = false;
    while tokio::time::Instant::now() < deadline {
        let result = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
        match result {
            Ok(Some(Ok(msg))) => {
                if let Ok(text) = msg.into_text()
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    && v["type"] == "alert"
                {
                    found_alert = true;
                    assert!(
                        v.get("alert").is_some(),
                        "alert message should have 'alert' field"
                    );
                    assert!(
                        v.get("category").is_some(),
                        "alert message should have 'category' field"
                    );
                    break;
                }
            }
            Ok(Some(Err(e))) => panic!("websocket error: {e}"),
            Ok(None) => panic!("stream ended before alert"),
            Err(_) => break, // timeout
        }
    }
    assert!(found_alert, "should have received a TorrentAdded alert");
}

#[tokio::test]
async fn test_ws_mask_filters() {
    let (addr, session) = start_test_server().await;
    // Connect with PEER-only mask (0x004). STATUS alerts should be filtered.
    let (mut ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=0&mask=0x004"))
            .await
            .expect("websocket connect");

    // Add a torrent — produces STATUS alert, which should be filtered.
    session
        .add_magnet_uri("magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test")
        .await
        .expect("add magnet");

    // Should NOT receive any message within 300ms.
    let result = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(
        result.is_err(),
        "should have timed out — STATUS alert should be filtered by PEER-only mask"
    );
}

#[tokio::test]
async fn test_ws_set_mask() {
    let (addr, session) = start_test_server().await;
    // Connect with ALL mask but disable heartbeats.
    let (mut ws, _response) =
        connect_async(format!("ws://{addr}/api/v1/events?interval=0&mask=0xFFF"))
            .await
            .expect("websocket connect");

    // Send set_mask to PEER-only (0x004), filtering out STATUS.
    ws.send(TungsteniteMessage::Text(
        r#"{"type":"set_mask","mask":"0x004"}"#.into(),
    ))
    .await
    .expect("send set_mask");

    // Give the server a moment to process the set_mask command.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Add a torrent — produces STATUS alert which should now be filtered.
    session
        .add_magnet_uri("magnet:?xt=urn:btih:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d&dn=test")
        .await
        .expect("add magnet");

    // Should NOT receive the STATUS alert.
    let result = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(
        result.is_err(),
        "should have timed out — STATUS alert should be filtered after set_mask to PEER-only"
    );
}
