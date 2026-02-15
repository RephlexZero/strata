//! Sender detail page — full management: hardware stats, network interfaces,
//! receiver config, media inputs, connectivity test, stream controls, unenroll.

use leptos::prelude::*;

use crate::api;
use crate::types::{
    DashboardEvent, LinkStats, MediaInput, NetworkInterface, SenderDetail, SenderFullStatus,
    StreamSummary, TestRunResponse,
};
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

    // Hardware status (from heartbeat or REST)
    let (hw_interfaces, set_hw_interfaces) = signal(Vec::<NetworkInterface>::new());
    let (hw_inputs, set_hw_inputs) = signal(Vec::<MediaInput>::new());
    let (hw_cpu, set_hw_cpu) = signal(Option::<f32>::None);
    let (hw_mem, set_hw_mem) = signal(Option::<u32>::None);
    let (hw_uptime, set_hw_uptime) = signal(Option::<u64>::None);
    let (hw_receiver_url, set_hw_receiver_url) = signal(Option::<String>::None);

    // Unenroll state
    let (unenroll_token, set_unenroll_token) = signal(Option::<String>::None);
    let (show_unenroll_confirm, set_show_unenroll_confirm) = signal(false);

    // Track interface toggle loading state
    let (iface_loading, set_iface_loading) = signal(Option::<String>::None);

    // Receiver config
    let (receiver_input, set_receiver_input) = signal(String::new());
    let (config_msg, set_config_msg) = signal(Option::<(String, &'static str)>::None);
    let (receiver_loaded, set_receiver_loaded) = signal(false);

    // Interface scan
    let (scan_msg, set_scan_msg) = signal(Option::<(String, &'static str)>::None);

    // Connectivity test
    let (test_result, set_test_result) = signal(Option::<TestRunResponse>::None);
    let (test_loading, set_test_loading) = signal(false);

    // Load sender detail + status
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
                // Load streams
                if let Ok(all) = api::list_streams(&token).await {
                    let filtered: Vec<_> = all.into_iter().filter(|s| s.sender_id == id).collect();
                    if let Some(latest) = filtered.first() {
                        set_stream_state.set(latest.state.clone());
                    }
                    set_streams.set(filtered);
                }
                // Load full status (interfaces, system stats)
                if let Ok(status) = api::get_sender_status(&token, &id).await {
                    apply_full_status(
                        &status,
                        &set_hw_interfaces,
                        &set_hw_inputs,
                        &set_hw_cpu,
                        &set_hw_mem,
                        &set_hw_uptime,
                        &set_hw_receiver_url,
                    );
                    // Pre-fill receiver input
                    if !receiver_loaded.get_untracked() {
                        if let Some(ref url) = status.receiver_url {
                            set_receiver_input.set(url.clone());
                        }
                        set_receiver_loaded.set(true);
                    }
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
                    status,
                } => {
                    if sid == sender_id {
                        set_sender.update(|s| {
                            if let Some(s) = s {
                                s.online = online;
                            }
                        });
                        // Update hardware data from heartbeat
                        if let Some(status) = status {
                            apply_full_status(
                                &status,
                                &set_hw_interfaces,
                                &set_hw_inputs,
                                &set_hw_cpu,
                                &set_hw_mem,
                                &set_hw_uptime,
                                &set_hw_receiver_url,
                            );
                        }
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

    let auth_unenroll = auth.clone();
    let do_unenroll = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_unenroll.token.get_untracked().unwrap_or_default();
        set_action_loading.set(true);
        set_show_unenroll_confirm.set(false);
        leptos::task::spawn_local(async move {
            match api::unenroll_sender(&token, &id).await {
                Ok(resp) => {
                    set_unenroll_token.set(Some(resp.enrollment_token));
                    set_sender.update(|s| {
                        if let Some(s) = s {
                            s.enrolled = false;
                            s.online = false;
                        }
                    });
                    set_hw_interfaces.set(vec![]);
                    set_hw_inputs.set(vec![]);
                    set_hw_cpu.set(None);
                    set_hw_mem.set(None);
                    set_hw_uptime.set(None);
                    set_hw_receiver_url.set(None);
                    set_action_loading.set(false);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_action_loading.set(false);
                }
            }
        });
    };

    // Config save handler
    let auth_config = auth.clone();
    let save_config = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_config.token.get_untracked().unwrap_or_default();
        let url = receiver_input.get_untracked();
        let url_val = if url.is_empty() { None } else { Some(url) };
        leptos::task::spawn_local(async move {
            match api::set_sender_config(&token, &id, url_val).await {
                Ok(resp) => {
                    let msg = if resp.receiver_url.is_some() {
                        "Configuration saved"
                    } else {
                        "Receiver URL cleared"
                    };
                    set_config_msg.set(Some((msg.into(), "ok")));
                    set_hw_receiver_url.set(resp.receiver_url);
                }
                Err(e) => set_config_msg.set(Some((format!("Save failed: {e}"), "err"))),
            }
        });
    };

    // Test handler
    let auth_test = auth.clone();
    let run_test = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_test.token.get_untracked().unwrap_or_default();
        set_test_loading.set(true);
        set_test_result.set(None);
        leptos::task::spawn_local(async move {
            match api::run_sender_test(&token, &id).await {
                Ok(r) => set_test_result.set(Some(r)),
                Err(e) => set_error.set(Some(format!("Test failed: {e}"))),
            }
            set_test_loading.set(false);
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
                            // ── Page Header ─────────────────────────────
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

                            // ── System Stats (CPU, Memory, Uptime) ──────
                            {move || {
                                let cpu = hw_cpu.get();
                                let mem = hw_mem.get();
                                let up = hw_uptime.get();
                                if cpu.is_some() || mem.is_some() || up.is_some() {
                                    view! {
                                        <div class="stats-grid" style="margin-bottom: 16px;">
                                            {cpu.map(|v| view! {
                                                <div class="stat-card">
                                                    <div class="stat-label">"CPU"</div>
                                                    <div class="stat-value">
                                                        {format!("{:.0}", v)}
                                                        <span class="stat-unit">"%"</span>
                                                    </div>
                                                </div>
                                            })}
                                            {mem.map(|v| view! {
                                                <div class="stat-card">
                                                    <div class="stat-label">"Memory"</div>
                                                    <div class="stat-value">
                                                        {v}
                                                        <span class="stat-unit">" MB"</span>
                                                    </div>
                                                </div>
                                            })}
                                            {up.map(|v| view! {
                                                <div class="stat-card">
                                                    <div class="stat-label">"Device Uptime"</div>
                                                    <div class="stat-value">
                                                        {format_duration(v)}
                                                    </div>
                                                </div>
                                            })}
                                        </div>
                                    }.into_any()
                                } else {
                                    view! { <span></span> }.into_any()
                                }
                            }}

                            // ── Network Interfaces ──────────────────────
                            <NetworkCard
                                sender_id=s.id.clone()
                                interfaces=hw_interfaces
                                is_online=is_online
                                iface_loading=iface_loading
                                set_iface_loading=set_iface_loading
                                scan_msg=scan_msg
                                set_scan_msg=set_scan_msg
                                set_error=set_error
                            />

                            // ── Receiver Config ─────────────────────────
                            <div class="card" style="margin-bottom: 16px;">
                                <div class="card-header">
                                    <h3>"🎯 Receiver Configuration"</h3>
                                </div>

                                {move || config_msg.get().map(|(msg, kind)| {
                                    let cls = match kind { "ok" => "msg msg-ok", "err" => "msg msg-err", _ => "msg msg-info" };
                                    view! { <div class={cls}>{msg}</div> }
                                })}

                                <p style="font-size: 13px; color: var(--text-secondary); margin-bottom: 14px;">
                                    "Set the RIST receiver address this sender will transmit to."
                                </p>

                                <div style="display: flex; gap: 12px; align-items: flex-end;">
                                    <div class="form-group" style="flex: 1; margin-bottom: 0;">
                                        <label>"Receiver URL"</label>
                                        <input
                                            class="form-input"
                                            type="text"
                                            placeholder="rist://receiver.example.com:5000"
                                            prop:value=move || receiver_input.get()
                                            disabled=!is_online
                                            on:input=move |ev| {
                                                set_receiver_input.set(event_target_value(&ev));
                                            }
                                        />
                                    </div>
                                    <button
                                        class="btn btn-primary"
                                        on:click=save_config
                                        disabled=!is_online
                                        style="height: 38px;"
                                    >
                                        "Save"
                                    </button>
                                </div>

                                {move || {
                                    let url = hw_receiver_url.get();
                                    view! {
                                        <p style="margin-top: 10px; font-size: 12px; color: var(--text-muted); font-family: var(--font-mono);">
                                            {url.map(|u| format!("Current: {u}")).unwrap_or_else(|| "No receiver configured".into())}
                                        </p>
                                    }
                                }}
                            </div>

                            // ── Media Inputs ────────────────────────────
                            <MediaInputsCard inputs=hw_inputs />

                            // ── Connectivity Test ───────────────────────
                            <div class="card" style="margin-bottom: 16px;">
                                <div class="card-header">
                                    <h3>"🔍 Connectivity Test"</h3>
                                </div>
                                <button
                                    class="btn btn-ghost"
                                    on:click=run_test
                                    disabled=move || test_loading.get() || !is_online
                                >
                                    {move || if test_loading.get() { "Testing…" } else { "Run Test" }}
                                </button>
                                {move || test_result.get().map(|r| view! {
                                    <div class="test-grid" style="margin-top: 12px;">
                                        <div class="test-item">
                                            <div class="test-label">"Cloud"</div>
                                            <div class={if r.cloud_reachable { "test-value test-pass" } else { "test-value test-fail" }}>
                                                {if r.cloud_reachable { "✓ Reachable" } else { "✗ Unreachable" }}
                                            </div>
                                        </div>
                                        <div class="test-item">
                                            <div class="test-label">"WebSocket"</div>
                                            <div class={if r.cloud_connected { "test-value test-pass" } else { "test-value test-fail" }}>
                                                {if r.cloud_connected { "✓ Connected" } else { "✗ Disconnected" }}
                                            </div>
                                        </div>
                                        <div class="test-item">
                                            <div class="test-label">"Receiver"</div>
                                            <div class={
                                                if r.receiver_url.is_some() {
                                                    if r.receiver_reachable { "test-value test-pass" } else { "test-value test-fail" }
                                                } else {
                                                    "test-value test-na"
                                                }
                                            }>
                                                {if r.receiver_url.is_some() {
                                                    if r.receiver_reachable { "✓ Reachable" } else { "✗ Unreachable" }
                                                } else {
                                                    "— Not set"
                                                }}
                                            </div>
                                        </div>
                                        <div class="test-item">
                                            <div class="test-label">"Enrolled"</div>
                                            <div class={if r.enrolled { "test-value test-pass" } else { "test-value test-na" }}>
                                                {if r.enrolled { "✓ Yes" } else { "⚠ No" }}
                                            </div>
                                        </div>
                                    </div>
                                    {r.control_url.as_ref().map(|url| view! {
                                        <p style="font-size: 12px; color: var(--text-muted); margin-top: 8px; font-family: var(--font-mono);">
                                            "Control: " {url.clone()}
                                        </p>
                                    })}
                                    {r.receiver_url.as_ref().map(|url| view! {
                                        <p style="font-size: 12px; color: var(--text-muted); margin-top: 4px; font-family: var(--font-mono);">
                                            "Receiver: " {url.clone()}
                                        </p>
                                    })}
                                })}
                            </div>

                            // ── Stream Status ───────────────────────────
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

                            // ── Details Card ────────────────────────────
                            <div class="card" style="margin-bottom: 16px;">
                                <div class="card-header">
                                    <h3>"Details"</h3>
                                </div>
                                <div class="table-wrap">
                                    <table class="data-table">
                                        <tbody>
                                            <tr>
                                                <td style="color: var(--text-secondary); width: 140px;">"ID"</td>
                                                <td><code style="font-size: 12px;">{s.id.clone()}</code></td>
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

                            // ── Unenroll Card ───────────────────────────
                            <div class="card" style="border-color: var(--danger, #e74c3c);">
                                <div class="card-header">
                                    <h3 style="color: var(--danger, #e74c3c);">"Danger Zone"</h3>
                                </div>

                                {move || unenroll_token.get().map(|token| view! {
                                    <div style="background: var(--bg-card, #1a1d23); border-radius: 8px; padding: 16px; margin-bottom: 12px;">
                                        <p style="color: var(--success, #2ecc71); margin: 0 0 8px 0;">"Sender unenrolled. New enrollment token:"</p>
                                        <code style="font-size: 18px; letter-spacing: 2px; color: var(--text-primary);">
                                            {token}
                                        </code>
                                    </div>
                                })}

                                <div style="display: flex; align-items: center; justify-content: space-between;">
                                    <div>
                                        <p style="margin: 0; color: var(--text-primary);">"Unenroll Sender"</p>
                                        <p style="margin: 4px 0 0 0; font-size: 13px; color: var(--text-secondary);">
                                            "Disconnects the sender and resets enrollment. A new token will be issued."
                                        </p>
                                    </div>
                                    {move || {
                                        if show_unenroll_confirm.get() {
                                            view! {
                                                <div style="display: flex; gap: 8px;">
                                                    <button
                                                        class="btn btn-danger"
                                                        on:click=do_unenroll
                                                        disabled=move || action_loading.get()
                                                    >
                                                        "Confirm Unenroll"
                                                    </button>
                                                    <button
                                                        class="btn btn-secondary"
                                                        on:click=move |_| set_show_unenroll_confirm.set(false)
                                                    >
                                                        "Cancel"
                                                    </button>
                                                </div>
                                            }.into_any()
                                        } else {
                                            view! {
                                                <button
                                                    class="btn btn-danger"
                                                    on:click=move |_| set_show_unenroll_confirm.set(true)
                                                    disabled=move || action_loading.get()
                                                >
                                                    "Unenroll"
                                                </button>
                                            }.into_any()
                                        }
                                    }}
                                </div>
                            </div>
                        }.into_any()
                    }
                }
            }}
        </div>
    }
}

/// Network interfaces card with toggle switches and scan button.
#[component]
fn NetworkCard(
    sender_id: String,
    interfaces: ReadSignal<Vec<NetworkInterface>>,
    is_online: bool,
    iface_loading: ReadSignal<Option<String>>,
    set_iface_loading: WriteSignal<Option<String>>,
    scan_msg: ReadSignal<Option<(String, &'static str)>>,
    set_scan_msg: WriteSignal<Option<(String, &'static str)>>,
    set_error: WriteSignal<Option<String>>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let sender_id_scan = sender_id.clone();
    let auth_scan = auth.clone();
    let do_scan = move |_| {
        let token = auth_scan.token.get_untracked().unwrap_or_default();
        let id = sender_id_scan.clone();
        set_scan_msg.set(Some(("Scanning for new interfaces…".into(), "info")));
        leptos::task::spawn_local(async move {
            match api::scan_sender_interfaces(&token, &id).await {
                Ok(r) => {
                    if r.discovered.is_empty() {
                        set_scan_msg.set(Some((
                            format!("No new interfaces found. {} total.", r.total),
                            "info",
                        )));
                    } else {
                        set_scan_msg.set(Some((
                            format!(
                                "Found {} new interface(s): {}",
                                r.discovered.len(),
                                r.discovered.join(", ")
                            ),
                            "ok",
                        )));
                    }
                }
                Err(e) => set_scan_msg.set(Some((format!("Scan failed: {e}"), "err"))),
            }
        });
    };

    view! {
        <div class="card" style="margin-bottom: 16px;">
            <div class="card-header">
                <h3>"📡 Network Interfaces"</h3>
                <div style="display: flex; gap: 8px; align-items: center;">
                    <span class="badge" style="background: var(--bg-tertiary, #252830);">
                        {move || interfaces.get().len()} " interfaces"
                    </span>
                    <button
                        class="btn btn-ghost btn-sm"
                        on:click=do_scan
                        disabled=!is_online
                    >
                        "Scan for New"
                    </button>
                </div>
            </div>

            {move || scan_msg.get().map(|(msg, kind)| {
                let cls = match kind { "ok" => "msg msg-ok", "err" => "msg msg-err", _ => "msg msg-info" };
                view! { <div class={cls}>{msg}</div> }
            })}

            {move || {
                let ifaces = interfaces.get();
                if ifaces.is_empty() {
                    view! {
                        <p style="color: var(--text-muted); font-size: 13px;">
                            "No interface data — sender may be offline"
                        </p>
                    }.into_any()
                } else {
                    view! {
                        <div class="interface-list">
                            {ifaces.into_iter().map(|iface| {
                                let name = iface.name.clone();
                                let name_toggle = iface.name.clone();
                                let sender_id = sender_id.clone();
                                let auth = auth.clone();
                                let enabled = iface.enabled;
                                let connected = iface.state == "connected";

                                let (dot, badge_cls, label) = if !enabled {
                                    ("dot dot-red", "badge badge-offline", "Disabled")
                                } else if connected {
                                    ("dot dot-green", "badge badge-online", "Up")
                                } else {
                                    ("dot dot-gray", "badge badge-offline", "Down")
                                };

                                let type_icon = match iface.iface_type.as_str() {
                                    "cellular" => "📶",
                                    "wifi" => "📡",
                                    _ => "🔌",
                                };

                                let mut meta_parts = vec![];
                                meta_parts.push(format!("{type_icon} {}", iface.iface_type));
                                if let Some(t) = &iface.technology { meta_parts.push(t.clone()); }
                                if let Some(c) = &iface.carrier { meta_parts.push(c.clone()); }
                                if let Some(db) = iface.signal_dbm { meta_parts.push(format!("{db} dBm")); }
                                if let Some(ip) = &iface.ip { meta_parts.push(ip.clone()); }

                                let toggle = move |_| {
                                    let sender_id = sender_id.clone();
                                    let iface_name = name_toggle.clone();
                                    let token = auth.token.get_untracked().unwrap_or_default();
                                    set_iface_loading.set(Some(iface_name.clone()));
                                    leptos::task::spawn_local(async move {
                                        let result = if enabled {
                                            api::disable_interface(&token, &sender_id, &iface_name).await
                                        } else {
                                            api::enable_interface(&token, &sender_id, &iface_name).await
                                        };
                                        if let Err(e) = result {
                                            set_error.set(Some(e));
                                        }
                                        set_iface_loading.set(None);
                                    });
                                };

                                let is_loading_cls = {
                                    let n = iface.name.clone();
                                    move || iface_loading.get().as_deref() == Some(&n)
                                };
                                let is_loading_disabled = {
                                    let n = iface.name.clone();
                                    move || iface_loading.get().as_deref() == Some(&n)
                                };

                                view! {
                                    <div class={if enabled { "interface-item" } else { "interface-item disabled" }}>
                                        <div style="display: flex; align-items: center; gap: 12px;">
                                            <label class={move || if is_loading_cls() { "toggle loading" } else { "toggle" }}>
                                                <input
                                                    type="checkbox"
                                                    checked=enabled
                                                    on:change=toggle
                                                    disabled=move || is_loading_disabled() || !is_online
                                                />
                                                <span class="slider"></span>
                                            </label>
                                            <div>
                                                <span class="iface-name">{name}</span>
                                                <div class="iface-meta">
                                                    {meta_parts.into_iter().map(|p| view! {
                                                        <span>{p}</span>
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            </div>
                                        </div>
                                        <span class={badge_cls}>
                                            <span class={dot}></span>
                                            {label}
                                        </span>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// Media inputs card — shows detected cameras, capture cards, etc.
#[component]
fn MediaInputsCard(inputs: ReadSignal<Vec<MediaInput>>) -> impl IntoView {
    view! {
        <div class="card" style="margin-bottom: 16px;">
            <div class="card-header">
                <h3>"🎥 Media Inputs"</h3>
            </div>
            {move || {
                let inputs = inputs.get();
                if inputs.is_empty() {
                    view! {
                        <p style="color: var(--text-muted); font-size: 13px;">"No inputs detected"</p>
                    }.into_any()
                } else {
                    view! {
                        <div class="interface-list">
                            {inputs.into_iter().map(|input| {
                                let caps = input.capabilities.join(", ");
                                let status_class = match input.status.as_str() {
                                    "available" => "badge badge-online",
                                    "in_use" => "badge badge-live",
                                    _ => "badge badge-offline",
                                };
                                let status_dot = match input.status.as_str() {
                                    "available" => "dot dot-green",
                                    "in_use" => "dot dot-red",
                                    _ => "dot dot-gray",
                                };
                                view! {
                                    <div class="interface-item">
                                        <div>
                                            <div style="font-weight: 500; margin-bottom: 2px;">{input.label}</div>
                                            <div class="iface-meta">
                                                <span>{input.device}</span>
                                                <span>{input.input_type}</span>
                                                {(!caps.is_empty()).then(|| view! { <span>{caps}</span> })}
                                            </div>
                                        </div>
                                        <span class={status_class}>
                                            <span class={status_dot}></span>
                                            {input.status.clone()}
                                        </span>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// Apply a SenderFullStatus to the hardware signals.
fn apply_full_status(
    status: &SenderFullStatus,
    set_ifaces: &WriteSignal<Vec<NetworkInterface>>,
    set_inputs: &WriteSignal<Vec<MediaInput>>,
    set_cpu: &WriteSignal<Option<f32>>,
    set_mem: &WriteSignal<Option<u32>>,
    set_uptime: &WriteSignal<Option<u64>>,
    set_receiver_url: &WriteSignal<Option<String>>,
) {
    if let Some(ifaces) = &status.network_interfaces {
        set_ifaces.set(ifaces.clone());
    }
    if let Some(inputs) = &status.media_inputs {
        set_inputs.set(inputs.clone());
    }
    if status.cpu_percent.is_some() {
        set_cpu.set(status.cpu_percent);
    }
    if status.mem_used_mb.is_some() {
        set_mem.set(status.mem_used_mb);
    }
    if status.uptime_s.is_some() {
        set_uptime.set(status.uptime_s);
    }
    if status.receiver_url.is_some() {
        set_receiver_url.set(status.receiver_url.clone());
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
