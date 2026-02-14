//! Sender detail page — hardware info, stream controls, live stats.

use leptos::prelude::*;

use crate::api;
use crate::types::{DashboardEvent, LinkStats, SenderDetail, StreamSummary};
use crate::ws::WsClient;
use crate::AuthState;

/// Detail view for a single sender.
#[component]
pub fn SenderDetailPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let ws = expect_context::<WsClient>();
    let params = leptos_router::hooks::use_params_map();

    let (sender, set_sender) = signal(Option::<SenderDetail>::None);
    let (_streams, set_streams) = signal(Vec::<StreamSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (action_loading, set_action_loading) = signal(false);

    // Live stats from WebSocket
    let (live_bitrate, set_live_bitrate) = signal(0u32);
    let (live_uptime, set_live_uptime) = signal(0u64);
    let (live_links, set_live_links) = signal(Vec::<LinkStats>::new());
    let (stream_state, set_stream_state) = signal(String::from("idle"));

    // Load sender detail
    let auth_load = auth.clone();
    Effect::new(move || {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_load.token.get();
        if let Some(token) = token {
            if id.is_empty() {
                return;
            }
            let id = id.clone();
            let token = token.clone();
            leptos::task::spawn_local(async move {
                match api::get_sender(&token, &id).await {
                    Ok(s) => set_sender.set(Some(s)),
                    Err(e) => set_error.set(Some(e)),
                }
                // Also load streams
                if let Ok(all) = api::list_streams(&token).await {
                    let filtered: Vec<_> = all.into_iter().filter(|s| s.sender_id == id).collect();
                    // Update stream state from most recent
                    if let Some(latest) = filtered.first() {
                        set_stream_state.set(latest.state.clone());
                    }
                    set_streams.set(filtered);
                }
            });
        }
    });

    // React to WebSocket events for this sender
    Effect::new(move || {
        if let Some(event) = ws.last_event.get() {
            let sender_id = params.get().get("id").unwrap_or_default();
            match event {
                DashboardEvent::StreamStats {
                    stream_id: _,
                    uptime_s,
                    encoder_bitrate_kbps,
                    links,
                } => {
                    set_live_bitrate.set(encoder_bitrate_kbps);
                    set_live_uptime.set(uptime_s);
                    set_live_links.set(links);
                }
                DashboardEvent::StreamStateChanged {
                    sender_id: sid,
                    state,
                    ..
                } => {
                    if sid == sender_id {
                        set_stream_state.set(state);
                    }
                }
                DashboardEvent::SenderStatus {
                    sender_id: sid,
                    online,
                    ..
                } => {
                    if sid == sender_id {
                        set_sender.update(|s| {
                            if let Some(s) = s {
                                s.online = online;
                            }
                        });
                    }
                }
            }
        }
    });

    let auth_start = auth.clone();
    let start_stream = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_start.token.get_untracked().unwrap_or_default();
        set_action_loading.set(true);
        leptos::task::spawn_local(async move {
            match api::start_stream(&token, &id, None).await {
                Ok(resp) => {
                    set_stream_state.set(resp.state);
                    set_action_loading.set(false);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_action_loading.set(false);
                }
            }
        });
    };

    let auth_stop = auth.clone();
    let stop_stream = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_stop.token.get_untracked().unwrap_or_default();
        set_action_loading.set(true);
        leptos::task::spawn_local(async move {
            match api::stop_stream(&token, &id).await {
                Ok(()) => {
                    set_stream_state.set("stopping".into());
                    set_action_loading.set(false);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_action_loading.set(false);
                }
            }
        });
    };

    view! {
        <div>
            {move || error.get().map(|e| view! {
                <div class="error-msg">{e}</div>
            })}

            {move || {
                let s = sender.get();
                match s {
                    None => view! { <p style="color: var(--text-secondary);">"Loading…"</p> }.into_any(),
                    Some(s) => {
                        let is_online = s.online;
                        let is_live = stream_state.get() == "live" || stream_state.get() == "starting";
                        view! {
                            <div class="page-header">
                                <div>
                                    <h2>{s.name.clone().unwrap_or_else(|| s.id.clone())}</h2>
                                    <p class="subtitle">
                                        {s.hostname.clone().unwrap_or_else(|| "Unknown host".into())}
                                        " · "
                                        <span class={if s.online { "badge badge-online" } else { "badge badge-offline" }}>
                                            <span class={if s.online { "dot dot-green" } else { "dot dot-gray" }}></span>
                                            {if s.online { "Online" } else { "Offline" }}
                                        </span>
                                    </p>
                                </div>
                                <div style="display: flex; gap: 8px;">
                                    {if is_live {
                                        view! {
                                            <button
                                                class="btn btn-danger"
                                                on:click=stop_stream
                                                disabled=move || action_loading.get()
                                            >
                                                "■ Stop Stream"
                                            </button>
                                        }.into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="btn btn-primary"
                                                on:click=start_stream
                                                disabled=move || action_loading.get() || !is_online
                                            >
                                                "▶ Start Stream"
                                            </button>
                                        }.into_any()
                                    }}
                                </div>
                            </div>

                            // Stream Status
                            <div class="card" style="margin-bottom: 16px;">
                                <div class="card-header">
                                    <h3>"Stream"</h3>
                                    <span class={
                                        let st = stream_state.get();
                                        match st.as_str() {
                                            "live" => "badge badge-live",
                                            "starting" | "stopping" => "badge badge-starting",
                                            _ => "badge badge-idle",
                                        }
                                    }>
                                        <span class={
                                            let st = stream_state.get();
                                            match st.as_str() {
                                                "live" => "dot dot-red",
                                                "starting" | "stopping" => "dot dot-yellow",
                                                _ => "dot dot-gray",
                                            }
                                        }></span>
                                        {move || stream_state.get().to_uppercase()}
                                    </span>
                                </div>

                                {move || {
                                    let st = stream_state.get();
                                    if st == "live" || st == "starting" {
                                        view! {
                                            <div class="stats-grid">
                                                <div class="stat-card">
                                                    <div class="stat-label">"Bitrate"</div>
                                                    <div class="stat-value">
                                                        {move || live_bitrate.get()}
                                                        <span class="stat-unit">"kbps"</span>
                                                    </div>
                                                </div>
                                                <div class="stat-card">
                                                    <div class="stat-label">"Uptime"</div>
                                                    <div class="stat-value">
                                                        {move || format_duration(live_uptime.get())}
                                                    </div>
                                                </div>
                                                <div class="stat-card">
                                                    <div class="stat-label">"Links"</div>
                                                    <div class="stat-value">
                                                        {move || live_links.get().len()}
                                                    </div>
                                                </div>
                                            </div>

                                            // Link stats table
                                            {move || {
                                                let links = live_links.get();
                                                if links.is_empty() {
                                                    view! { <p style="color: var(--text-muted); font-size: 13px;">"Waiting for link stats…"</p> }.into_any()
                                                } else {
                                                    view! {
                                                        <div class="table-wrap">
                                                            <table class="data-table">
                                                                <thead>
                                                                    <tr>
                                                                        <th>"Interface"</th>
                                                                        <th>"State"</th>
                                                                        <th>"RTT"</th>
                                                                        <th>"Loss"</th>
                                                                        <th>"Capacity"</th>
                                                                        <th>"Sent"</th>
                                                                    </tr>
                                                                </thead>
                                                                <tbody>
                                                                    <For
                                                                        each=move || live_links.get()
                                                                        key=|l| l.id
                                                                        children=move |link| {
                                                                            view! {
                                                                                <tr>
                                                                                    <td>{link.interface.clone()}</td>
                                                                                    <td>{link.state.clone()}</td>
                                                                                    <td>{format!("{:.1}ms", link.rtt_ms)}</td>
                                                                                    <td>{format!("{:.2}%", link.loss_rate * 100.0)}</td>
                                                                                    <td>{format_bps(link.capacity_bps)}</td>
                                                                                    <td>{format_bytes(link.sent_bytes)}</td>
                                                                                </tr>
                                                                            }
                                                                        }
                                                                    />
                                                                </tbody>
                                                            </table>
                                                        </div>
                                                    }.into_any()
                                                }
                                            }}
                                        }.into_any()
                                    } else {
                                        view! {
                                            <p style="color: var(--text-muted); font-size: 13px;">"No active stream"</p>
                                        }.into_any()
                                    }
                                }}
                            </div>

                            // Sender Info
                            <div class="card">
                                <div class="card-header">
                                    <h3>"Details"</h3>
                                </div>
                                <div class="table-wrap">
                                    <table class="data-table">
                                        <tbody>
                                            <tr>
                                                <td style="color: var(--text-secondary); width: 140px;">"ID"</td>
                                                <td>{s.id.clone()}</td>
                                            </tr>
                                            <tr>
                                                <td style="color: var(--text-secondary);">"Enrolled"</td>
                                                <td>{if s.enrolled { "Yes" } else { "No" }}</td>
                                            </tr>
                                            <tr>
                                                <td style="color: var(--text-secondary);">"Created"</td>
                                                <td>{s.created_at.clone()}</td>
                                            </tr>
                                            <tr>
                                                <td style="color: var(--text-secondary);">"Last seen"</td>
                                                <td>{s.last_seen_at.clone().unwrap_or_else(|| "Never".into())}</td>
                                            </tr>
                                        </tbody>
                                    </table>
                                </div>
                            </div>
                        }.into_any()
                    }
                }
            }}
        </div>
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn format_bps(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.0} kbps", bps as f64 / 1_000.0)
    } else {
        format!("{bps} bps")
    }
}

fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}
