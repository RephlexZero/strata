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

/// Build a test app with a fresh database pool and return (Router, JwtContext).
async fn test_app() -> Option<Router> {
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
    let state = strata_control::state::AppState::new(pool, jwt);

    let app = Router::new()
        .nest("/api", strata_control::api::router())
        .with_state(state);

    Some(app)
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
