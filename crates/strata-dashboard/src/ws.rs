//! Dashboard WebSocket client with automatic reconnection.
//!
//! Connects to `/ws`, sends the JWT token as the first message (an
//! `auth.login` envelope, matching the server's agent/receiver WS
//! handshake — not a `?token=` query param, since those end up in
//! proxy/access logs), receives `DashboardEvent`s, and exposes them as
//! Leptos signals for reactive UI updates. Reconnects automatically with a
//! ~3-4s jittered delay on disconnect (E9: avoids every tab reconnecting
//! in lockstep after a control-plane restart).

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

use strata_protocol::DashboardEvent;

/// Holds the live event stream from the dashboard WebSocket.
#[derive(Clone)]
pub struct WsClient {
    /// The most recent event received.
    pub last_event: ReadSignal<Option<DashboardEvent>>,
    set_event: WriteSignal<Option<DashboardEvent>>,
    /// Connection status.
    pub connected: ReadSignal<bool>,
    set_connected: WriteSignal<bool>,
}

impl Default for WsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WsClient {
    /// Create a new WS client (does not connect yet).
    pub fn new() -> Self {
        let (last_event, set_event) = signal(None::<DashboardEvent>);
        let (connected, set_connected) = signal(false);
        Self {
            last_event,
            set_event,
            connected,
            set_connected,
        }
    }

    /// Connect to the dashboard WebSocket. Reconnects automatically on disconnect.
    pub fn connect(&self, token: &str) {
        let url = build_ws_url();
        setup_websocket(url, token.to_string(), self.set_event, self.set_connected);
    }
}

/// Build the WebSocket URL from the current page location. The token is no
/// longer part of the URL — it's sent as the first message instead (see
/// `send_auth`).
fn build_ws_url() -> String {
    let location = web_sys::window().unwrap().location();
    let protocol = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = location.host().unwrap_or_else(|_| "localhost:3000".into());
    format!("{protocol}://{host}/ws")
}

/// Build the `auth.login` envelope JSON sent as the first WebSocket message.
/// Matches `strata_protocol::Envelope` / `DashboardAuthPayload`'s
/// wire shape; `id`/`ts` aren't validated server-side beyond `payload` and
/// `type`, so lightweight WASM-local values are fine here.
fn build_auth_message(token: &str) -> String {
    let ts = js_sys::Date::new_0()
        .to_iso_string()
        .as_string()
        .unwrap_or_default();
    serde_json::json!({
        "id": format!("dashboard-{}", js_sys::Date::now()),
        "type": "auth.login",
        "ts": ts,
        "proto_version": strata_protocol::PROTOCOL_VERSION,
        "payload": { "token": token },
    })
    .to_string()
}

/// Set up a WebSocket connection with all event handlers.
/// On disconnect, schedules an automatic reconnection after 3 seconds.
fn setup_websocket(
    url: String,
    token: String,
    set_event: WriteSignal<Option<DashboardEvent>>,
    set_connected: WriteSignal<bool>,
) {
    let ws = match WebSocket::new(&url) {
        Ok(ws) => ws,
        Err(e) => {
            log::error!("WebSocket connect failed: {e:?}");
            // Retry after delay
            schedule_reconnect(url, token, set_event, set_connected);
            return;
        }
    };

    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // onopen — send the auth handshake as the first message.
    let on_open = Closure::<dyn FnMut()>::new({
        let ws = ws.clone();
        let token = token.clone();
        let sc = set_connected;
        move || {
            log::info!("WebSocket connected, authenticating…");
            if let Err(e) = ws.send_with_str(&build_auth_message(&token)) {
                log::error!("failed to send WS auth message: {e:?}");
            }
            sc.set(true);
        }
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    // onclose — trigger reconnect
    let on_close = Closure::<dyn FnMut()>::new({
        let url = url.clone();
        let token = token.clone();
        let sc = set_connected;
        move || {
            log::warn!("WebSocket disconnected, reconnecting in 3s…");
            sc.set(false);
            schedule_reconnect(url.clone(), token.clone(), set_event, sc);
        }
    });
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    // onerror
    let on_error = Closure::<dyn FnMut()>::new(move || {
        log::error!("WebSocket error");
    });
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();

    // onmessage
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
        if let Ok(text) = e.data().dyn_into::<web_sys::js_sys::JsString>() {
            let s: String = text.into();
            // The auth handshake response is Envelope-wrapped
            // (`{"type":"auth.login.response",...}`); the live event stream
            // is not. Peek at "type" to tell them apart before parsing as
            // a `DashboardEvent`.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
                && v.get("type").and_then(|t| t.as_str()) == Some("auth.login.response")
            {
                let ok = v
                    .get("payload")
                    .and_then(|p| p.get("success"))
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false);
                if !ok {
                    let err = v
                        .get("payload")
                        .and_then(|p| p.get("error"))
                        .and_then(|e| e.as_str())
                        .unwrap_or("unknown error");
                    log::error!("dashboard WS auth rejected: {err}");
                }
                return;
            }
            match serde_json::from_str::<DashboardEvent>(&s) {
                Ok(event) => {
                    set_event.set(Some(event));
                }
                Err(err) => {
                    log::warn!("Failed to parse WS event: {err}");
                }
            }
        }
    });
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    // The WebSocket stays alive until onclose fires, at which point the
    // reconnect handler creates a new one. We forget this handle so it
    // isn't dropped — the browser GC cleans up closed WebSockets.
    std::mem::forget(ws);
}

/// Schedule a WebSocket reconnection after a fixed delay (3 seconds).
/// Base reconnect delay (ms). A control-plane restart makes every open
/// dashboard tab reconnect at once; `RECONNECT_JITTER_MS` spreads that out
/// so they don't all hit `/ws` in the same instant (E9).
const RECONNECT_BASE_MS: i32 = 3_000;
const RECONNECT_JITTER_MS: f64 = 1_000.0;

fn schedule_reconnect(
    url: String,
    token: String,
    set_event: WriteSignal<Option<DashboardEvent>>,
    set_connected: WriteSignal<bool>,
) {
    let reconnect = Closure::wrap(Box::new(move || {
        log::info!("attempting WebSocket reconnect…");
        setup_websocket(url.clone(), token.clone(), set_event, set_connected);
    }) as Box<dyn FnMut()>);

    let delay_ms = RECONNECT_BASE_MS + (js_sys::Math::random() * RECONNECT_JITTER_MS) as i32;
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            reconnect.as_ref().unchecked_ref(),
            delay_ms,
        );
    reconnect.forget();
}
