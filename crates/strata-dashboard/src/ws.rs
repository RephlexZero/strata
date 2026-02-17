//! Dashboard WebSocket client with automatic reconnection.
//!
//! Connects to `/ws` with the JWT token, receives `DashboardEvent`s,
//! and exposes them as Leptos signals for reactive UI updates.
//! Reconnects automatically with a fixed 3-second delay on disconnect.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

use crate::types::DashboardEvent;

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
        let url = build_ws_url(token);
        setup_websocket(url, self.set_event, self.set_connected);
    }
}

/// Build the WebSocket URL from the current page location.
fn build_ws_url(token: &str) -> String {
    let location = web_sys::window().unwrap().location();
    let protocol = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = location.host().unwrap_or_else(|_| "localhost:3000".into());
    format!("{protocol}://{host}/ws?token={token}")
}

/// Set up a WebSocket connection with all event handlers.
/// On disconnect, schedules an automatic reconnection after 3 seconds.
fn setup_websocket(
    url: String,
    set_event: WriteSignal<Option<DashboardEvent>>,
    set_connected: WriteSignal<bool>,
) {
    let ws = match WebSocket::new(&url) {
        Ok(ws) => ws,
        Err(e) => {
            log::error!("WebSocket connect failed: {e:?}");
            // Retry after delay
            schedule_reconnect(url, set_event, set_connected);
            return;
        }
    };

    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    // onopen
    let on_open = Closure::<dyn FnMut()>::new({
        let sc = set_connected;
        move || {
            log::info!("WebSocket connected");
            sc.set(true);
        }
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    // onclose — trigger reconnect
    let on_close = Closure::<dyn FnMut()>::new({
        let url = url.clone();
        let sc = set_connected;
        move || {
            log::warn!("WebSocket disconnected, reconnecting in 3s…");
            sc.set(false);
            schedule_reconnect(url.clone(), set_event, sc);
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
fn schedule_reconnect(
    url: String,
    set_event: WriteSignal<Option<DashboardEvent>>,
    set_connected: WriteSignal<bool>,
) {
    let reconnect = Closure::wrap(Box::new(move || {
        log::info!("attempting WebSocket reconnect…");
        setup_websocket(url.clone(), set_event, set_connected);
    }) as Box<dyn FnMut()>);

    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            reconnect.as_ref().unchecked_ref(),
            3_000,
        );
    reconnect.forget();
}
