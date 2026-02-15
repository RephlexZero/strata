//! Strata Dashboard — Leptos CSR WASM application.
//!
//! Single-page app that talks to the strata-control REST API and
//! receives live updates over the dashboard WebSocket.

pub mod api;
pub mod pages;
pub mod types;
pub mod ws;

use gloo_storage::{LocalStorage, Storage};
use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use pages::destinations::DestinationsPage;
use pages::login::LoginPage;
use pages::sender_detail::SenderDetailPage;
use pages::senders::SendersPage;
use pages::streams::StreamsPage;
use ws::WsClient;

const TOKEN_KEY: &str = "strata_token";

// ── Auth State ──────────────────────────────────────────────────────

/// Global authentication state, provided via Leptos context.
#[derive(Clone)]
pub struct AuthState {
    pub token: ReadSignal<Option<String>>,
    set_token: WriteSignal<Option<String>>,
}

impl AuthState {
    fn new() -> Self {
        let stored: Option<String> = LocalStorage::get(TOKEN_KEY).ok();
        let (token, set_token) = signal(stored);
        Self { token, set_token }
    }

    pub fn login(&self, token: String) {
        let _ = LocalStorage::set(TOKEN_KEY, &token);
        self.set_token.set(Some(token));
    }

    pub fn logout(&self) {
        LocalStorage::delete(TOKEN_KEY);
        self.set_token.set(None);
    }

    pub fn is_authenticated(&self) -> bool {
        self.token.get_untracked().is_some()
    }
}

// ── App Root ────────────────────────────────────────────────────────

/// Leptos application root.
#[component]
pub fn App() -> impl IntoView {
    let auth = AuthState::new();
    let ws_client = WsClient::new();

    // Connect WebSocket when we have a token
    let ws_connect = ws_client.clone();
    let auth_ws = auth.clone();
    Effect::new(move || {
        if let Some(token) = auth_ws.token.get() {
            ws_connect.connect(&token);
        }
    });

    provide_context(auth.clone());
    provide_context(ws_client);

    view! {
        <Router>
            {move || {
                if auth.token.get().is_none() {
                    view! { <LoginPage /> }.into_any()
                } else {
                    view! { <DashboardShell /> }.into_any()
                }
            }}
        </Router>
    }
}

// ── Dashboard Shell (sidebar + content) ─────────────────────────────

#[component]
fn DashboardShell() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let ws = expect_context::<WsClient>();

    view! {
        <div class="flex min-h-screen">
            // Sidebar
            <nav class="w-60 bg-base-200 border-r border-base-300 flex flex-col fixed top-0 left-0 bottom-0 z-10">
                <div class="p-5 border-b border-base-300 flex items-center gap-2.5">
                    <h1 class="text-lg font-bold tracking-tight">"Strata"</h1>
                    <span class="text-xs text-base-content/40 font-mono">"v0.1"</span>
                </div>
                <ul class="menu flex-1 p-2 gap-0.5">
                    <li><a href="/senders">"📡 Senders"</a></li>
                    <li><a href="/streams">"📺 Streams"</a></li>
                    <li><a href="/destinations">"🎯 Destinations"</a></li>
                </ul>
                <div class="p-3 border-t border-base-300">
                    <div class="flex justify-between items-center">
                        <span>
                            {move || if ws.connected.get() {
                                view! { <span class="badge badge-success badge-sm gap-1"><span class="w-2 h-2 rounded-full bg-success"></span>"Live"</span> }.into_any()
                            } else {
                                view! { <span class="badge badge-ghost badge-sm gap-1"><span class="w-2 h-2 rounded-full bg-base-content/30"></span>"Offline"</span> }.into_any()
                            }}
                        </span>
                        <button
                            class="btn btn-ghost btn-sm"
                            on:click=move |_| auth.logout()
                        >
                            "Logout"
                        </button>
                    </div>
                </div>
            </nav>
            // Main content
            <main class="flex-1 ml-60 p-6 max-w-5xl">
                <Routes fallback=|| view! { <SendersPage /> }>
                    <Route path=path!("/") view=SendersPage />
                    <Route path=path!("/senders") view=SendersPage />
                    <Route path=path!("/senders/:id") view=SenderDetailPage />
                    <Route path=path!("/streams") view=StreamsPage />
                    <Route path=path!("/destinations") view=DestinationsPage />
                </Routes>
            </main>
        </div>
    }
}

// ── WASM entry point ────────────────────────────────────────────────

/// Called automatically when the WASM module is initialized.
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);
    log::info!("Strata Dashboard starting");
    leptos::mount::mount_to_body(App);
}
