//! API integration tests for strata-control.
//!
//! These tests exercise the REST API through axum's tower service interface
//! (no TCP). They require a running PostgreSQL instance.
//!
//! Set `TEST_DATABASE_URL` to run these tests:
//!   TEST_DATABASE_URL=postgres://strata:dev-only-password@localhost/strata_test cargo test -p strata-control
//!
//! The tests create a temporary schema per test run to avoid collisions.

use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use strata_common::auth::JwtContext;

/// Build a fresh `AppState` against a clean test database, or `None` if no
/// test database is reachable (skips the caller's test).
async fn test_state() -> Option<strata_control::state::AppState> {
    let db_url = match std::env::var("TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            // Fall back to Docker Compose default
            let default = "postgres://strata:dev-only-password@localhost:5432/strata_test";
            // Quick check if reachable
            if std::net::TcpStream::connect("127.0.0.1:5432").is_err() {
                eprintln!("skipping integration test: no PostgreSQL at localhost:5432");
                return None;
            }
            default.to_string()
        }
    };

    // Connect and run migrations
    let pool = match strata_control::db::connect(&db_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping integration test: DB connect failed: {e}");
            return None;
        }
    };

    if let Err(e) = strata_control::db::migrate(&pool).await {
        eprintln!("skipping integration test: migration failed: {e}");
        return None;
    }

    // Clean tables for a fresh slate (order matters due to FK constraints)
    let _ = sqlx::query("DELETE FROM streams").execute(&pool).await;
    let _ = sqlx::query("DELETE FROM destinations").execute(&pool).await;
    let _ = sqlx::query("DELETE FROM senders").execute(&pool).await;
    let _ = sqlx::query("DELETE FROM users").execute(&pool).await;

    let (jwt, _seed) = JwtContext::generate();
    Some(strata_control::state::AppState::new(pool, jwt))
}

/// Build a test app with a fresh database pool and return the Router.
async fn test_app() -> Option<Router> {
    let state = test_state().await?;
    Some(
        Router::new()
            .nest("/api", strata_control::api::router())
            .with_state(state),
    )
}

/// Like `test_app`, but also returns the underlying `AppState` (needed to
/// drive the dashboard broadcast channel directly, and to mount `/ws` for
/// WebSocket tests).
async fn test_app_with_state() -> Option<(Router, strata_control::state::AppState)> {
    let state = test_state().await?;
    let app = Router::new()
        .nest("/api", strata_control::api::router())
        .route(
            "/ws",
            axum::routing::get(strata_control::ws_dashboard::handler),
        )
        .with_state(state.clone());
    Some((app, state))
}

/// Helper: parse JSON response body.
async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        let text = String::from_utf8_lossy(&bytes);
        panic!("not valid JSON: {text}");
    })
}

/// Helper: build a JSON POST request.
fn json_post(uri: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Helper: build a GET request with auth header.
fn auth_get(uri: &str, token: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .method("GET")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Helper: build a POST request with auth header and JSON body.
fn auth_post(uri: &str, token: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Helper: build a DELETE request with auth header.
fn auth_delete(uri: &str, token: &str) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .uri(uri)
        .method("DELETE")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ── Auth Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn register_creates_user() {
    let Some(app) = test_app().await else {
        return;
    };

    let resp = app
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": "test@example.com",
                "password": "password123"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body = json_body(resp).await;
    assert_eq!(body["email"], "test@example.com");
    assert!(body["user_id"].as_str().unwrap().starts_with("usr_"));
}

#[tokio::test]
async fn register_rejects_short_password() {
    let Some(app) = test_app().await else {
        return;
    };

    let resp = app
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": "bad@example.com",
                "password": "short"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn register_rejects_invalid_email() {
    let Some(app) = test_app().await else {
        return;
    };

    let resp = app
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": "not-an-email",
                "password": "password123"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn login_returns_jwt() {
    let Some(app) = test_app().await else {
        return;
    };

    // Register first
    let resp = app
        .clone()
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": "login@example.com",
                "password": "password123"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Login
    let resp = app
        .oneshot(json_post(
            "/api/auth/login",
            serde_json::json!({
                "email": "login@example.com",
                "password": "password123"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    assert!(!body["token"].as_str().unwrap().is_empty());
    assert_eq!(body["role"], "operator");
}

#[tokio::test]
async fn login_rejects_wrong_password() {
    let Some(app) = test_app().await else {
        return;
    };

    // Register
    let _ = app
        .clone()
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": "wrongpw@example.com",
                "password": "password123"
            }),
        ))
        .await;

    // Login with wrong password
    let resp = app
        .oneshot(json_post(
            "/api/auth/login",
            serde_json::json!({
                "email": "wrongpw@example.com",
                "password": "wrong_password"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

// ── Helper: Register + Login → token ────────────────────────────────

async fn register_and_login(app: &Router) -> String {
    let email = format!("user-{}@test.com", uuid::Uuid::now_v7());

    let _ = app
        .clone()
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": email,
                "password": "password123"
            }),
        ))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(json_post(
            "/api/auth/login",
            serde_json::json!({
                "email": email,
                "password": "password123"
            }),
        ))
        .await
        .unwrap();

    let body = json_body(resp).await;
    body["token"].as_str().unwrap().to_string()
}

/// Like `register_and_login`, but also returns the new user's ID (needed to
/// drive `AppState::broadcast_dashboard` directly in WS tests).
async fn register_and_login_with_id(app: &Router) -> (String, String) {
    let email = format!("user-{}@test.com", uuid::Uuid::now_v7());

    let resp = app
        .clone()
        .oneshot(json_post(
            "/api/auth/register",
            serde_json::json!({
                "email": email,
                "password": "password123"
            }),
        ))
        .await
        .unwrap();
    let user_id = json_body(resp).await["user_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = app
        .clone()
        .oneshot(json_post(
            "/api/auth/login",
            serde_json::json!({
                "email": email,
                "password": "password123"
            }),
        ))
        .await
        .unwrap();
    let token = json_body(resp).await["token"].as_str().unwrap().to_string();

    (user_id, token)
}

// ── Sender CRUD Tests ───────────────────────────────────────────────

#[tokio::test]
async fn create_and_list_senders() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    // Create a sender
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            &token,
            serde_json::json!({ "name": "Test Sender" }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap().to_string();
    assert!(sender_id.starts_with("snd_"));
    // Enrollment token should be in XXXX-XXXX format
    let enrollment_token = body["enrollment_token"].as_str().unwrap();
    assert!(
        enrollment_token.len() >= 8,
        "enrollment token too short: {enrollment_token}"
    );

    // List senders
    let resp = app
        .clone()
        .oneshot(auth_get("/api/senders", &token))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    let senders = body.as_array().unwrap();
    assert!(senders.iter().any(|s| s["id"] == sender_id));
}

#[tokio::test]
async fn get_sender_detail() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            &token,
            serde_json::json!({ "name": "Detail Test" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap();

    // Get detail
    let resp = app
        .clone()
        .oneshot(auth_get(&format!("/api/senders/{sender_id}"), &token))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    assert_eq!(body["id"], sender_id);
    assert_eq!(body["name"], "Detail Test");
    assert_eq!(body["online"], false);
    assert_eq!(body["enrolled"], false);
}

#[tokio::test]
async fn delete_sender() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            &token,
            serde_json::json!({ "name": "To Delete" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap();

    // Delete
    let resp = app
        .clone()
        .oneshot(auth_delete(&format!("/api/senders/{sender_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Verify gone
    let resp = app
        .oneshot(auth_get(&format!("/api/senders/{sender_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn sender_not_found_returns_404() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    let resp = app
        .oneshot(auth_get("/api/senders/snd_nonexistent", &token))
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

// ── Destination CRUD Tests ──────────────────────────────────────────

#[tokio::test]
async fn create_and_list_destinations() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/destinations",
            &token,
            serde_json::json!({
                "platform": "youtube",
                "name": "My Channel",
                "url": "rtmp://a.rtmp.youtube.com/live2",
                "stream_key": "xxxx-xxxx-xxxx"
            }),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body = json_body(resp).await;
    let dest_id = body["id"].as_str().unwrap().to_string();
    assert!(dest_id.starts_with("dst_"));

    // List
    let resp = app
        .clone()
        .oneshot(auth_get("/api/destinations", &token))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    let dests = body.as_array().unwrap();
    assert!(dests.iter().any(|d| d["id"] == dest_id));
}

#[tokio::test]
async fn delete_destination() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    // Create
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/destinations",
            &token,
            serde_json::json!({
                "platform": "twitch",
                "name": "Twitch",
                "url": "rtmp://live.twitch.tv/app"
            }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let dest_id = body["id"].as_str().unwrap();

    // Delete
    let resp = app
        .clone()
        .oneshot(auth_delete(&format!("/api/destinations/{dest_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Verify gone
    let resp = app
        .oneshot(auth_get("/api/destinations", &token))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let dests = body.as_array().unwrap();
    assert!(!dests.iter().any(|d| d["id"] == dest_id));
}

// ── Auth Guard Tests ────────────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let Some(app) = test_app().await else {
        return;
    };

    // No auth header
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/senders")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn invalid_token_is_rejected() {
    let Some(app) = test_app().await else {
        return;
    };

    let resp = app
        .oneshot(auth_get("/api/senders", "invalid.jwt.token"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

// ── Stream Tests ────────────────────────────────────────────────────

#[tokio::test]
async fn list_streams_empty() {
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    let resp = app.oneshot(auth_get("/api/streams", &token)).await.unwrap();

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn start_stream_concurrent_guard_query_does_not_error() {
    // Regression for E7's SQL bind bug: the concurrent-stream guard query
    // (`SELECT EXISTS(... WHERE sender_id = $1 ...)`) has exactly one
    // placeholder but the handler used to call `.bind()` twice, which
    // sqlx/Postgres rejects at execution time. That query runs before the
    // "sender is offline" check, so pre-fix this request would 500 with a
    // DB parameter-count error; post-fix it reaches the offline check and
    // returns a clean 400.
    let Some(app) = test_app().await else {
        return;
    };

    let token = register_and_login(&app).await;

    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            &token,
            serde_json::json!({ "name": "Never Connected" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(auth_post(
            &format!("/api/streams/start/{sender_id}"),
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        400,
        "expected a clean 400 (sender offline) past the concurrent-stream \
         guard query, not a 500 from the bind-count bug"
    );
    let body = json_body(resp).await;
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("offline"),
        "expected the offline-sender error, got: {msg}"
    );
}

// ── Cross-User Isolation Tests ──────────────────────────────────────

#[tokio::test]
async fn users_cannot_see_each_others_senders() {
    let Some(app) = test_app().await else {
        return;
    };

    let token_a = register_and_login(&app).await;
    let token_b = register_and_login(&app).await;

    // User A creates a sender
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            &token_a,
            serde_json::json!({ "name": "User A Sender" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap().to_string();

    // User B cannot see it in list
    let resp = app
        .clone()
        .oneshot(auth_get("/api/senders", &token_b))
        .await
        .unwrap();
    let body = json_body(resp).await;
    assert!(
        !body
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == sender_id)
    );

    // User B cannot access it directly
    let resp = app
        .oneshot(auth_get(&format!("/api/senders/{sender_id}"), &token_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ── Dashboard WebSocket: auth + owner scoping (E3) ───────────────────

/// Send the `auth.login` handshake envelope a real dashboard client sends
/// as its first WS message (see `strata-dashboard/src/ws.rs::
/// build_auth_message` and `ws_dashboard.rs::authenticate`).
async fn ws_send_auth(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    token: &str,
) {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let envelope = serde_json::json!({
        "id": "test-auth",
        "type": "auth.login",
        "ts": chrono::Utc::now().to_rfc3339(),
        "payload": { "token": token },
    });
    ws.send(Message::Text(envelope.to_string().into()))
        .await
        .unwrap();
}

/// Receive the next text message, or `None` if nothing arrives within
/// `timeout` (used to assert a filtered-out event never shows up).
async fn ws_recv_json(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    timeout: std::time::Duration,
) -> Option<serde_json::Value> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        match tokio::time::timeout(timeout, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                return Some(serde_json::from_str(&text).unwrap());
            }
            Ok(Some(Ok(_))) => continue, // ignore ping/pong/binary
            Ok(Some(Err(_))) | Ok(None) => return None,
            Err(_) => return None, // timed out
        }
    }
}

#[tokio::test]
async fn dashboard_ws_scopes_events_to_owner() {
    let Some((app, state)) = test_app_with_state().await else {
        return;
    };

    let (user_a_id, token_a) = register_and_login_with_id(&app).await;
    let (user_b_id, _token_b) = register_and_login_with_id(&app).await;

    // Spin up a real TCP listener — WebSocket upgrades need an actual
    // bidirectional connection, not axum's oneshot tower-service testing.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (mut ws_a, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect dashboard WS");
    ws_send_auth(&mut ws_a, &token_a).await;
    let auth_response = ws_recv_json(&mut ws_a, std::time::Duration::from_secs(2))
        .await
        .expect("auth.login.response");
    assert_eq!(auth_response["type"], "auth.login.response");
    assert_eq!(auth_response["payload"]["success"], true);

    // An event owned by user B must never reach user A's socket.
    state.broadcast_dashboard(
        user_b_id.clone(),
        strata_protocol::DashboardEvent::SenderStatus {
            sender_id: "sender-b".into(),
            online: true,
            status: None,
        },
    );
    // An event owned by user A must reach it.
    state.broadcast_dashboard(
        user_a_id.clone(),
        strata_protocol::DashboardEvent::SenderStatus {
            sender_id: "sender-a".into(),
            online: true,
            status: None,
        },
    );

    let received = ws_recv_json(&mut ws_a, std::time::Duration::from_secs(2))
        .await
        .expect("user A's own event");
    assert_eq!(received["sender_id"], "sender-a");

    // Nothing else should arrive — user B's event must have been filtered,
    // not merely delayed.
    let leaked = ws_recv_json(&mut ws_a, std::time::Duration::from_millis(300)).await;
    assert!(
        leaked.is_none(),
        "another owner's event leaked onto this socket: {leaked:?}"
    );
}

#[tokio::test]
async fn dashboard_ws_rejects_invalid_token() {
    let Some((app, _state)) = test_app_with_state().await else {
        return;
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect dashboard WS");
    ws_send_auth(&mut ws, "not-a-real-token").await;

    let response = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("auth.login.response");
    assert_eq!(response["type"], "auth.login.response");
    assert_eq!(response["payload"]["success"], false);

    // The server closes the connection after rejecting auth.
    let after = ws_recv_json(&mut ws, std::time::Duration::from_secs(2)).await;
    assert!(
        after.is_none(),
        "expected the socket to close after auth rejection"
    );
}

// ── Stream state machine + reconciliation (E2) ──────────────────────

/// Create a sender via the API and connect an agent WebSocket for it,
/// completing the enrollment handshake. Returns the authenticated socket
/// and the sender id.
async fn connect_agent_ws(
    app: &Router,
    addr: std::net::SocketAddr,
    user_token: &str,
) -> (
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
) {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            user_token,
            serde_json::json!({ "name": "Reconcile Test" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap().to_string();
    let enrollment_token = body["enrollment_token"].as_str().unwrap().to_string();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
        .await
        .expect("connect agent WS");
    let auth = serde_json::json!({
        "id": "test-agent-auth",
        "type": "auth.login",
        "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "enrollment_token": enrollment_token,
            "device_key": null,
            "agent_version": "test",
            "hostname": "test-agent",
            "arch": "x86_64",
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();
    let resp = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("agent auth response");
    assert_eq!(
        resp["payload"]["success"], true,
        "agent auth failed: {resp}"
    );

    (ws, sender_id)
}

/// Send a device.status heartbeat listing `running` stream ids.
async fn send_heartbeat(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    running: &[&str],
) {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let hb = serde_json::json!({
        "id": "test-heartbeat",
        "type": "device.status",
        "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "network_interfaces": [],
            "media_inputs": [],
            "stream_state": if running.is_empty() { "idle" } else { "live" },
            "cpu_percent": 1.0,
            "mem_used_mb": 64,
            "uptime_s": 10,
            "running_streams": running,
        },
    });
    ws.send(Message::Text(hb.to_string().into())).await.unwrap();
}

/// Poll the DB until the stream reaches `expected` or the timeout expires.
async fn wait_for_state(
    state: &strata_control::state::AppState,
    stream_id: &str,
    expected: &str,
) -> String {
    for _ in 0..40 {
        let current: String = sqlx::query_scalar("SELECT state FROM streams WHERE id = $1")
            .bind(stream_id)
            .fetch_one(state.pool())
            .await
            .unwrap();
        if current == expected {
            return current;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    sqlx::query_scalar("SELECT state FROM streams WHERE id = $1")
        .bind(stream_id)
        .fetch_one(state.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn ws_drop_no_longer_ends_streams() {
    let Some((app, state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    let (ws, sender_id) = connect_agent_ws(&app, addr, &token).await;

    sqlx::query(
        "INSERT INTO streams (id, sender_id, state, started_at) VALUES ($1, $2, 'live', $3)",
    )
    .bind("str_wsdrop")
    .bind(&sender_id)
    .bind(chrono::Utc::now())
    .execute(state.pool())
    .await
    .unwrap();

    // Drop the socket — the old behavior orphan-marked every active stream
    // 'ended' here. Reconciliation semantics: a WS drop is "unobserved".
    drop(ws);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let current: String = sqlx::query_scalar("SELECT state FROM streams WHERE id = $1")
        .bind("str_wsdrop")
        .fetch_one(state.pool())
        .await
        .unwrap();
    assert_eq!(
        current, "live",
        "a WS drop must not end streams — only reconciliation/sweep may"
    );
}

#[tokio::test]
async fn heartbeat_reconciles_stream_not_running_on_sender() {
    let Some((app, state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    let (mut ws, sender_id) = connect_agent_ws(&app, addr, &token).await;

    // A 'live' stream the agent does not report → ended on the next
    // heartbeat (no grace: only 'starting' rows get STARTING_GRACE).
    sqlx::query(
        "INSERT INTO streams (id, sender_id, state, started_at) VALUES ($1, $2, 'live', $3)",
    )
    .bind("str_gone")
    .bind(&sender_id)
    .bind(chrono::Utc::now())
    .execute(state.pool())
    .await
    .unwrap();

    send_heartbeat(&mut ws, &[]).await;

    let current = wait_for_state(&state, "str_gone", "ended").await;
    assert_eq!(current, "ended");
    let err: Option<String> = sqlx::query_scalar("SELECT error_message FROM streams WHERE id = $1")
        .bind("str_gone")
        .fetch_one(state.pool())
        .await
        .unwrap();
    assert_eq!(err.as_deref(), Some("not running on sender (reconciled)"));
}

#[tokio::test]
async fn heartbeat_readopts_inferred_end_but_not_confirmed_end() {
    let Some((app, state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    let (mut ws, sender_id) = connect_agent_ws(&app, addr, &token).await;

    // Inferred end (error_message set by the control plane) → readopted.
    sqlx::query(
        "INSERT INTO streams (id, sender_id, state, started_at, ended_at, error_message) \
         VALUES ($1, $2, 'ended', $3, $3, 'sender unobserved (connection lost)')",
    )
    .bind("str_inferred")
    .bind(&sender_id)
    .bind(chrono::Utc::now())
    .execute(state.pool())
    .await
    .unwrap();

    // Confirmed end (no error_message) → stays ended; intent is enforced.
    sqlx::query(
        "INSERT INTO streams (id, sender_id, state, started_at, ended_at) \
         VALUES ($1, $2, 'ended', $3, $3)",
    )
    .bind("str_confirmed")
    .bind(&sender_id)
    .bind(chrono::Utc::now())
    .execute(state.pool())
    .await
    .unwrap();

    send_heartbeat(&mut ws, &["str_inferred", "str_confirmed"]).await;

    let readopted = wait_for_state(&state, "str_inferred", "live").await;
    assert_eq!(readopted, "live", "inferred end must be readopted");
    let err: Option<String> = sqlx::query_scalar("SELECT error_message FROM streams WHERE id = $1")
        .bind("str_inferred")
        .fetch_one(state.pool())
        .await
        .unwrap();
    assert_eq!(
        err, None,
        "readoption must clear the inferred-end attribution"
    );

    let confirmed: String = sqlx::query_scalar("SELECT state FROM streams WHERE id = $1")
        .bind("str_confirmed")
        .fetch_one(state.pool())
        .await
        .unwrap();
    assert_eq!(confirmed, "ended", "confirmed end must not be resurrected");

    // The enforcement path re-sends stream.stop for the confirmed end.
    let stop = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("expected a re-sent stream.stop for the confirmed end");
    assert_eq!(stop["type"], "stream.stop");
    assert_eq!(stop["payload"]["stream_id"], "str_confirmed");
}

#[tokio::test]
async fn transition_rejects_illegal_moves() {
    let Some(state) = test_state().await else {
        return;
    };
    let token_suffix = "trans";

    // Seed a user + sender + ended stream directly.
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, role) VALUES ($1, $2, 'x', 'operator')",
    )
    .bind(format!("usr_{token_suffix}"))
    .bind(format!("{token_suffix}@test.com"))
    .execute(state.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO senders (id, owner_id) VALUES ($1, $2)")
        .bind(format!("snd_{token_suffix}"))
        .bind(format!("usr_{token_suffix}"))
        .execute(state.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO streams (id, sender_id, state, started_at, ended_at) \
         VALUES ($1, $2, 'ended', $3, $3)",
    )
    .bind("str_terminal")
    .bind(format!("snd_{token_suffix}"))
    .bind(chrono::Utc::now())
    .execute(state.pool())
    .await
    .unwrap();

    use strata_protocol::models::StreamState;

    // Terminal states are sticky against every non-readopt transition.
    for to in [StreamState::Live, StreamState::Stopping, StreamState::Ended] {
        let moved =
            strata_control::stream_state::transition(state.pool(), "str_terminal", to, None)
                .await
                .unwrap();
        assert!(!moved, "ended → {to} must be rejected");
    }

    // 'starting' is never a transition target (creation is INSERT-only).
    let moved = strata_control::stream_state::transition(
        state.pool(),
        "str_terminal",
        StreamState::Starting,
        None,
    )
    .await
    .unwrap();
    assert!(!moved);

    // A confirmed end (no error_message) is not readoptable.
    let readopted = strata_control::stream_state::readopt(state.pool(), "str_terminal")
        .await
        .unwrap();
    assert!(!readopted);
}

// ── Device identity: one-time tokens + challenge auth (E4) ───────────

/// Perform a full agent enrollment WITH a device public key, consuming the
/// one-time token. Returns (sender_id, composite_token).
async fn enroll_agent_with_key(
    app: &Router,
    addr: std::net::SocketAddr,
    user_token: &str,
    public_key: &str,
) -> (String, String) {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/senders",
            user_token,
            serde_json::json!({ "name": "Keyed Device" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let sender_id = body["sender_id"].as_str().unwrap().to_string();
    let token = body["enrollment_token"].as_str().unwrap().to_string();
    assert!(
        token.starts_with(&format!("{sender_id}.")),
        "expected composite <id>.<secret> token, got {token}"
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
        .await
        .unwrap();
    let auth = serde_json::json!({
        "id": "t", "type": "auth.login", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "enrollment_token": token,
            "device_public_key": public_key,
            "agent_version": "test", "hostname": "keyed", "arch": "x86_64",
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();
    let resp = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("enrollment response");
    assert_eq!(
        resp["payload"]["success"], true,
        "enrollment failed: {resp}"
    );

    (sender_id, token)
}

#[tokio::test]
async fn enrollment_token_is_single_use_and_key_auth_works() {
    let Some((app, _state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let (private_key, public_key) = strata_common::auth::generate_device_keypair();
    let (sender_id, enrollment_token) =
        enroll_agent_with_key(&app, addr, &token, &public_key).await;

    // 1. The token is consumed — a second enrollment with it must fail.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
        .await
        .unwrap();
    let auth = serde_json::json!({
        "id": "t", "type": "auth.login", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "enrollment_token": enrollment_token,
            "agent_version": "test", "hostname": "replay", "arch": "x86_64",
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();
    let resp = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("replay response");
    assert_eq!(
        resp["payload"]["success"], false,
        "a consumed enrollment token must be rejected: {resp}"
    );

    // 2. Challenge auth with the enrolled key succeeds.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
        .await
        .unwrap();
    let auth = serde_json::json!({
        "id": "t", "type": "auth.login", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "enrollment_token": null,
            "device_id": sender_id,
            "device_public_key": public_key,
            "agent_version": "test", "hostname": "keyed", "arch": "x86_64",
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();

    let challenge_msg = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("auth.challenge");
    assert_eq!(challenge_msg["type"], "auth.challenge");
    let challenge = challenge_msg["payload"]["challenge"].as_str().unwrap();

    let signature = strata_common::auth::sign_challenge(&private_key, challenge).unwrap();
    let response = serde_json::json!({
        "id": "t", "type": "auth.challenge.response", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": { "device_id": sender_id, "signature": signature },
    });
    ws.send(Message::Text(response.to_string().into()))
        .await
        .unwrap();

    let result = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("auth result");
    assert_eq!(result["type"], "auth.login.response");
    assert_eq!(
        result["payload"]["success"], true,
        "challenge auth with the enrolled key must succeed: {result}"
    );
    assert_eq!(result["payload"]["sender_id"], sender_id.as_str());
}

#[tokio::test]
async fn challenge_auth_rejects_wrong_key() {
    let Some((app, _state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let (_enrolled_private, enrolled_public) = strata_common::auth::generate_device_keypair();
    let (sender_id, _) = enroll_agent_with_key(&app, addr, &token, &enrolled_public).await;

    // Attacker knows the sender_id but holds a different keypair.
    let (attacker_private, _) = strata_common::auth::generate_device_keypair();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/agent/ws"))
        .await
        .unwrap();
    let auth = serde_json::json!({
        "id": "t", "type": "auth.login", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "device_id": sender_id,
            "agent_version": "test", "hostname": "mallory", "arch": "x86_64",
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();

    let challenge_msg = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("auth.challenge");
    let challenge = challenge_msg["payload"]["challenge"].as_str().unwrap();

    let signature = strata_common::auth::sign_challenge(&attacker_private, challenge).unwrap();
    let response = serde_json::json!({
        "id": "t", "type": "auth.challenge.response", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": { "device_id": sender_id, "signature": signature },
    });
    ws.send(Message::Text(response.to_string().into()))
        .await
        .unwrap();

    let result = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("auth result");
    assert_eq!(
        result["payload"]["success"], false,
        "a signature from the wrong key must be rejected: {result}"
    );
}

// ── Receiver-owned port allocation (E6) ──────────────────────────────

#[tokio::test]
async fn receiver_stream_start_ack_routes_allocated_ports() {
    let Some((app, state)) = test_app_with_state().await else {
        return;
    };
    let token = register_and_login(&app).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_app = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, serve_app).await.unwrap();
    });

    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    // Create + enroll a receiver over its WS.
    let resp = app
        .clone()
        .oneshot(auth_post(
            "/api/receivers",
            &token,
            serde_json::json!({ "bind_host": "203.0.113.7", "name": "rcv-test" }),
        ))
        .await
        .unwrap();
    let body = json_body(resp).await;
    let enrollment_token = body["enrollment_token"].as_str().unwrap().to_string();

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/receiver/ws"))
        .await
        .unwrap();
    let auth = serde_json::json!({
        "id": "t", "type": "auth.login", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "enrollment_token": enrollment_token,
            "receiver_version": "test", "hostname": "rcv-test", "region": null,
            "bind_host": "203.0.113.7", "link_ports": [5000, 5002, 5004], "max_streams": 4,
        },
    });
    ws.send(Message::Text(auth.to_string().into()))
        .await
        .unwrap();
    let resp = ws_recv_json(&mut ws, std::time::Duration::from_secs(2))
        .await
        .expect("receiver auth response");
    assert_eq!(
        resp["payload"]["success"], true,
        "receiver auth failed: {resp}"
    );

    // Simulate the control plane's request/ack: register a pending request,
    // have the "receiver" answer with its allocated ports, and check the
    // ack lands on the waiting oneshot (the path api/streams.rs::
    // request_receiver_start relies on).
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.pending_requests().insert("req_ports_1".into(), tx);

    let ack = serde_json::json!({
        "id": "t", "type": "receiver.stream.started", "ts": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "request_id": "req_ports_1",
            "stream_id": "str_x",
            "success": true,
            "bind_ports": [5002, 5004],
        },
    });
    ws.send(Message::Text(ack.to_string().into()))
        .await
        .unwrap();

    let value = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("ack timed out")
        .expect("oneshot dropped");
    let ack: strata_protocol::ReceiverStreamStartedPayload = serde_json::from_value(value).unwrap();
    assert!(ack.success);
    assert_eq!(ack.bind_ports, vec![5002, 5004]);
}
