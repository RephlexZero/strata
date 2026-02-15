//! Dashboard WebSocket client.
//!
//! Connects to `/ws` with the JWT token, receives `DashboardEvent`s,
//! and exposes them as a Leptos signal for reactive UI updates.

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

    /// Connect to the dashboard WebSocket.
    pub fn connect(&self, token: &str) {
        let location = web_sys::window().unwrap().location();
        let protocol = if location.protocol().unwrap_or_default() == "https:" {
            "wss"
        } else {
            "ws"
        };
        let host = location.host().unwrap_or_else(|_| "localhost:3000".into());
        let url = format!("{protocol}://{host}/ws?token={token}");

        let ws = match WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(e) => {
                log::error!("WebSocket connect failed: {e:?}");
                return;
            }
        };

        ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // onopen
        let set_connected = self.set_connected;
        let on_open = Closure::<dyn FnMut()>::new(move || {
            log::info!("WebSocket connected");
            set_connected.set(true);
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();

        // onclose
        let set_connected2 = self.set_connected;
        let on_close = Closure::<dyn FnMut()>::new(move || {
            log::warn!("WebSocket disconnected");
            set_connected2.set(false);
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
        let set_event = self.set_event;
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(text) = e.data().dyn_into::<js_sys::JsString>() {
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

        // Keep the WebSocket alive for the lifetime of the app.
        std::mem::forget(ws);
    }
}
