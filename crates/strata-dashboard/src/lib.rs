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
        <div class="app-layout">
            <nav class="sidebar">
                <div class="sidebar-brand">
                    <h1>"Strata"</h1>
                    <span class="version">"v0.1"</span>
                </div>
                <div class="sidebar-nav">
                    <a href="/senders">
                        <span class="icon">"📡"</span>
                        "Senders"
                    </a>
                    <a href="/streams">
                        <span class="icon">"📺"</span>
                        "Streams"
                    </a>
                    <a href="/destinations">
                        <span class="icon">"🎯"</span>
                        "Destinations"
                    </a>
                </div>
                <div class="sidebar-footer">
                    <div style="display: flex; justify-content: space-between; align-items: center;">
                        <span>
                            {move || if ws.connected.get() {
                                view! { <span class="badge badge-online"><span class="dot dot-green"></span>"Live"</span> }.into_any()
                            } else {
                                view! { <span class="badge badge-offline"><span class="dot dot-gray"></span>"Offline"</span> }.into_any()
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
            <main class="main-content">
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

/// Called by trunk to mount the app.
pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);
    log::info!("Strata Dashboard starting");
    leptos::mount::mount_to_body(App);
}
