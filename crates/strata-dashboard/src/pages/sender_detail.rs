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

    // Destination picker for starting a stream
    let (show_start_modal, set_show_start_modal) = signal(false);
    let (destinations, set_destinations) = signal(Vec::<crate::types::DestinationSummary>::new());
    let (selected_dest, set_selected_dest) = signal(Option::<String>::None);
    let (dests_loading, set_dests_loading) = signal(false);

    // Receiver URL change confirmation
    let (show_receiver_confirm, set_show_receiver_confirm) = signal(false);

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
                if let Ok(all) = api::list_streams(&token).await {
                    let filtered: Vec<_> = all.into_iter().filter(|s| s.sender_id == id).collect();
                    // Prefer a live/starting stream over the most-recent (which may be "ended")
                    let active = filtered
                        .iter()
                        .find(|s| s.state == "live" || s.state == "starting")
                        .or(filtered.first());
                    if let Some(latest) = active {
                        set_stream_state.set(latest.state.clone());
                    }
                    set_streams.set(filtered);
                }
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
                    sender_id: stats_sender_id,
                    uptime_s,
                    encoder_bitrate_kbps,
                    links,
                    ..
                } => {
                    // Only apply stats that belong to this sender
                    if stats_sender_id == sender_id {
                        set_live_bitrate.set(encoder_bitrate_kbps);
                        set_live_uptime.set(uptime_s);
                        set_live_links.set(links);
                        // Stats flowing means the stream is live — auto-promote
                        // from "starting" in case we missed the state change event.
                        let st = stream_state.get_untracked();
                        if st == "starting" {
                            set_stream_state.set("live".into());
                        }
                    }
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

    let auth_open = auth.clone();
    let open_start_modal = move |_| {
        set_show_start_modal.set(true);
        set_selected_dest.set(None);
        set_dests_loading.set(true);
        let token = auth_open.token.get_untracked().unwrap_or_default();
        leptos::task::spawn_local(async move {
            match api::list_destinations(&token).await {
                Ok(dests) => set_destinations.set(dests),
                Err(_) => set_destinations.set(vec![]),
            }
            set_dests_loading.set(false);
        });
    };

    let auth_start2 = auth.clone();
    let confirm_start_stream = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_start2.token.get_untracked().unwrap_or_default();
        let dest_id = selected_dest.get_untracked();
        set_action_loading.set(true);
        set_show_start_modal.set(false);
        leptos::task::spawn_local(async move {
            match api::start_stream(&token, &id, dest_id).await {
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

    // Config save — with confirmation guard when online/streaming
    let auth_config = auth.clone();
    let do_save_config = move |_| {
        let id = params.get().get("id").unwrap_or_default();
        let token = auth_config.token.get_untracked().unwrap_or_default();
        let url = receiver_input.get_untracked();
        let url_val = if url.is_empty() { None } else { Some(url) };
        set_show_receiver_confirm.set(false);
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

    let save_config = move |_| {
        // If online or streaming, show warning first
        let is_online = sender.get().map(|s| s.online).unwrap_or(false);
        let st = stream_state.get_untracked();
        if is_online || st == "live" || st == "starting" {
            set_show_receiver_confirm.set(true);
        } else {
            do_save_config(());
        }
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
                <div class="alert alert-error text-sm mb-4">{e}</div>
            })}

            // ── Start Stream — Destination Picker (outside sender reactive scope) ──
            {move || show_start_modal.get().then(|| {
                view! {
                    <div class="modal modal-open">
                        <div class="modal-box">
                            <h3 class="font-bold text-lg">"Start Stream"</h3>
                            <p class="text-sm text-base-content/60 mt-2">
                                "Select a destination for the stream, or start without one for bonded RIST only."
                            </p>

                            <div class="mt-4">
                                {move || {
                                    if dests_loading.get() {
                                        view! { <p class="text-sm text-base-content/40">"Loading destinations…"</p> }.into_any()
                                    } else {
                                        let dests = destinations.get();
                                        view! {
                                            <div class="flex flex-col gap-2">
                                                // "No destination" option — RIST-only
                                                <label class="flex items-center gap-3 p-3 bg-base-300 rounded cursor-pointer hover:bg-base-content/10 border border-base-300"
                                                    class:border-primary=move || selected_dest.get().is_none()
                                                >
                                                    <input
                                                        type="radio"
                                                        name="destination"
                                                        class="radio radio-sm radio-primary"
                                                        checked=move || selected_dest.get().is_none()
                                                        on:change=move |_| set_selected_dest.set(None)
                                                    />
                                                    <div>
                                                        <div class="font-medium text-sm">"Bonded RIST Only"</div>
                                                        <div class="text-xs text-base-content/60">"Stream to the configured receiver without an RTMP relay"</div>
                                                    </div>
                                                </label>

                                                // Destination options
                                                {dests.iter().map(|d| {
                                                    let d_id = d.id.clone();
                                                    let d_id3 = d.id.clone();
                                                    let d_id4 = d.id.clone();
                                                    let d_name = d.name.clone();
                                                    let d_platform = d.platform.clone();
                                                    let d_url = d.url.clone();
                                                    view! {
                                                        <label class="flex items-center gap-3 p-3 bg-base-300 rounded cursor-pointer hover:bg-base-content/10 border border-base-300"
                                                            class:border-primary=move || selected_dest.get().as_deref() == Some(&d_id3)
                                                        >
                                                            <input
                                                                type="radio"
                                                                name="destination"
                                                                class="radio radio-sm radio-primary"
                                                                checked=move || selected_dest.get().as_deref() == Some(&d_id4)
                                                                on:change=move |_| set_selected_dest.set(Some(d_id.clone()))
                                                            />
                                                            <div>
                                                                <div class="font-medium text-sm">{d_name}</div>
                                                                <div class="text-xs text-base-content/60 font-mono">{d_platform} " · " {d_url}</div>
                                                            </div>
                                                        </label>
                                                    }
                                                }).collect::<Vec<_>>()}

                                                {dests.is_empty().then(|| view! {
                                                    <p class="text-xs text-base-content/40 mt-1">
                                                        "No destinations configured. Add one from the Destinations page."
                                                    </p>
                                                })}
                                            </div>
                                        }.into_any()
                                    }
                                }}
                            </div>

                            <div class="modal-action">
                                <button class="btn btn-ghost" on:click=move |_| set_show_start_modal.set(false)>
                                    "Cancel"
                                </button>
                                <button
                                    class="btn btn-primary"
                                    on:click=confirm_start_stream
                                    disabled=move || dests_loading.get()
                                >
                                    "▶ Go Live"
                                </button>
                            </div>
                        </div>
                        <div class="modal-backdrop" on:click=move |_| set_show_start_modal.set(false)>
                            <button>"close"</button>
                        </div>
                    </div>
                }
            })}

            // ── Receiver URL Change Confirmation (outside sender reactive scope) ──
            {move || show_receiver_confirm.get().then(|| view! {
                <div class="modal modal-open">
                    <div class="modal-box">
                        <h3 class="font-bold text-lg text-warning">"⚠ Change Receiver URL?"</h3>
                        <p class="mt-3 text-sm">
                            "This sender is currently "
                            <strong>{if stream_state.get() == "live" { "streaming" } else { "online" }}</strong>
                            ". Changing the receiver URL may cause a "
                            <strong>"connection loss"</strong>
                            " and require re-pairing."
                        </p>
                        <p class="mt-2 text-sm text-base-content/60">
                            "Are you sure you want to proceed?"
                        </p>
                        <div class="modal-action">
                            <button class="btn btn-ghost" on:click=move |_| set_show_receiver_confirm.set(false)>
                                "Cancel"
                            </button>
                            <button class="btn btn-warning" on:click=move |_| do_save_config(())>
                                "Yes, Change Receiver"
                            </button>
                        </div>
                    </div>
                    <div class="modal-backdrop" on:click=move |_| set_show_receiver_confirm.set(false)>
                        <button>"close"</button>
                    </div>
                </div>
            })}

            {move || {
                let s = sender.get();
                match s {
                    None => view! { <p class="text-base-content/60">"Loading…"</p> }.into_any(),
                    Some(s) => {
                        let is_online = s.online;
                        let is_live = stream_state.get() == "live" || stream_state.get() == "starting";
                        view! {
                            // ── Page Header ─────────────────────────────
                            <div class="flex justify-between items-center mb-6">
                                <div>
                                    <h2 class="text-2xl font-semibold">{s.name.clone().unwrap_or_else(|| s.id.clone())}</h2>
                                    <p class="text-sm text-base-content/60 mt-1">
                                        {s.hostname.clone().unwrap_or_else(|| "Unknown host".into())}
                                        " · "
                                        <span class={if s.online { "badge badge-success gap-1" } else { "badge badge-ghost gap-1" }}>
                                            <span class={if s.online { "w-2 h-2 rounded-full bg-success" } else { "w-2 h-2 rounded-full bg-base-content/30" }}></span>
                                            {if s.online { "Online" } else { "Offline" }}
                                        </span>
                                    </p>
                                </div>
                                <div class="flex gap-2">
                                    {if is_live {
                                        view! {
                                            <button class="btn btn-error" on:click=stop_stream disabled=move || action_loading.get()>
                                                "■ Stop Stream"
                                            </button>
                                        }.into_any()
                                    } else {
                                        view! {
                                            <button class="btn btn-primary" on:click=open_start_modal disabled=move || action_loading.get() || !is_online>
                                                "▶ Start Stream"
                                            </button>
                                        }.into_any()
                                    }}
                                </div>
                            </div>

                            // ── System Stats ────────────────────────────
                            {move || {
                                let cpu = hw_cpu.get();
                                let mem = hw_mem.get();
                                let up = hw_uptime.get();
                                if cpu.is_some() || mem.is_some() || up.is_some() {
                                    view! {
                                        <div class="stats stats-horizontal bg-base-200 border border-base-300 w-full mb-4">
                                            {cpu.map(|v| view! {
                                                <div class="stat">
                                                    <div class="stat-title">"CPU"</div>
                                                    <div class="stat-value text-lg font-mono">{format!("{:.0}", v)}<span class="text-sm text-base-content/60">" %"</span></div>
                                                </div>
                                            })}
                                            {mem.map(|v| view! {
                                                <div class="stat">
                                                    <div class="stat-title">"Memory"</div>
                                                    <div class="stat-value text-lg font-mono">{v}<span class="text-sm text-base-content/60">" MB"</span></div>
                                                </div>
                                            })}
                                            {up.map(|v| view! {
                                                <div class="stat">
                                                    <div class="stat-title">"Device Uptime"</div>
                                                    <div class="stat-value text-lg font-mono">{format_duration(v)}</div>
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
                            <div class="card bg-base-200 border border-base-300 mb-4">
                                <div class="card-body">
                                    <h3 class="card-title text-base">"🎯 Receiver Configuration"</h3>

                                    {move || config_msg.get().map(|(msg, kind)| {
                                        let cls = match kind {
                                            "ok" => "alert alert-success text-sm",
                                            "err" => "alert alert-error text-sm",
                                            _ => "alert alert-info text-sm",
                                        };
                                        view! { <div class={cls}>{msg}</div> }
                                    })}

                                    <p class="text-sm text-base-content/60 mb-3">
                                        "Set the RIST receiver address this sender will transmit to."
                                    </p>

                                    <div class="flex gap-3 items-end">
                                        <fieldset class="fieldset flex-1">
                                            <label class="fieldset-label">"Receiver URL"</label>
                                            <input
                                                class="input input-bordered w-full"
                                                type="text"
                                                placeholder="rist://receiver.example.com:5000"
                                                prop:value=move || receiver_input.get()
                                                disabled=!is_online
                                                on:input=move |ev| {
                                                    set_receiver_input.set(event_target_value(&ev));
                                                }
                                            />
                                        </fieldset>
                                        <button class="btn btn-primary" on:click=save_config disabled=!is_online>
                                            "Save"
                                        </button>
                                    </div>

                                    {move || {
                                        let url = hw_receiver_url.get();
                                        view! {
                                            <p class="mt-2 text-xs text-base-content/40 font-mono">
                                                {url.map(|u| format!("Current: {u}")).unwrap_or_else(|| "No receiver configured".into())}
                                            </p>
                                        }
                                    }}
                                </div>
                            </div>

                            // ── Media Inputs ────────────────────────────
                            <MediaInputsCard inputs=hw_inputs />

                            // ── Connectivity Test ───────────────────────
                            <div class="card bg-base-200 border border-base-300 mb-4">
                                <div class="card-body">
                                    <h3 class="card-title text-base">"🔍 Connectivity Test"</h3>
                                    <button
                                        class="btn btn-ghost btn-sm w-fit"
                                        on:click=run_test
                                        disabled=move || test_loading.get() || !is_online
                                    >
                                        {move || if test_loading.get() { "Testing…" } else { "Run Test" }}
                                    </button>
                                    {move || test_result.get().map(|r| view! {
                                        <div class="grid grid-cols-2 md:grid-cols-4 gap-2 mt-3">
                                            <div class="bg-base-300 rounded p-3">
                                                <div class="text-xs text-base-content/40 uppercase tracking-wide">"Cloud"</div>
                                                <div class={if r.cloud_reachable { "font-semibold font-mono text-success" } else { "font-semibold font-mono text-error" }}>
                                                    {if r.cloud_reachable { "✓ Reachable" } else { "✗ Unreachable" }}
                                                </div>
                                            </div>
                                            <div class="bg-base-300 rounded p-3">
                                                <div class="text-xs text-base-content/40 uppercase tracking-wide">"WebSocket"</div>
                                                <div class={if r.cloud_connected { "font-semibold font-mono text-success" } else { "font-semibold font-mono text-error" }}>
                                                    {if r.cloud_connected { "✓ Connected" } else { "✗ Disconnected" }}
                                                </div>
                                            </div>
                                            <div class="bg-base-300 rounded p-3">
                                                <div class="text-xs text-base-content/40 uppercase tracking-wide">"Receiver"</div>
                                                <div class={
                                                    if r.receiver_url.is_some() {
                                                        if r.receiver_reachable { "font-semibold font-mono text-success" } else { "font-semibold font-mono text-error" }
                                                    } else {
                                                        "font-semibold font-mono text-base-content/40"
                                                    }
                                                }>
                                                    {if r.receiver_url.is_some() {
                                                        if r.receiver_reachable { "✓ Reachable" } else { "✗ Unreachable" }
                                                    } else {
                                                        "— Not set"
                                                    }}
                                                </div>
                                            </div>
                                            <div class="bg-base-300 rounded p-3">
                                                <div class="text-xs text-base-content/40 uppercase tracking-wide">"Enrolled"</div>
                                                <div class={if r.enrolled { "font-semibold font-mono text-success" } else { "font-semibold font-mono text-base-content/40" }}>
                                                    {if r.enrolled { "✓ Yes" } else { "⚠ No" }}
                                                </div>
                                            </div>
                                        </div>
                                        {r.control_url.as_ref().map(|url| view! {
                                            <p class="text-xs text-base-content/40 mt-2 font-mono">"Control: " {url.clone()}</p>
                                        })}
                                        {r.receiver_url.as_ref().map(|url| view! {
                                            <p class="text-xs text-base-content/40 mt-1 font-mono">"Receiver: " {url.clone()}</p>
                                        })}
                                    })}
                                </div>
                            </div>

                            // ── Stream Status ───────────────────────────
                            <div class="card bg-base-200 border border-base-300 mb-4">
                                <div class="card-body">
                                    <div class="flex justify-between items-center">
                                        <h3 class="card-title text-base">"Stream"</h3>
                                        <span class={
                                            let st = stream_state.get();
                                            match st.as_str() {
                                                "live" => "badge badge-error gap-1",
                                                "starting" | "stopping" => "badge badge-warning gap-1",
                                                _ => "badge badge-ghost gap-1",
                                            }
                                        }>
                                            <span class={
                                                let st = stream_state.get();
                                                match st.as_str() {
                                                    "live" => "w-2 h-2 rounded-full bg-error animate-pulse-dot",
                                                    "starting" | "stopping" => "w-2 h-2 rounded-full bg-warning",
                                                    _ => "w-2 h-2 rounded-full bg-base-content/30",
                                                }
                                            }></span>
                                            {move || stream_state.get().to_uppercase()}
                                        </span>
                                    </div>

                                    {move || {
                                        let st = stream_state.get();
                                        if st == "live" || st == "starting" {
                                            view! {
                                                <div class="stats stats-horizontal bg-base-300 w-full mt-3">
                                                    <div class="stat">
                                                        <div class="stat-title">"Throughput"</div>
                                                        <div class="stat-value text-lg font-mono">{move || live_bitrate.get()}<span class="text-sm text-base-content/60">" kbps"</span></div>
                                                    </div>
                                                    <div class="stat">
                                                        <div class="stat-title">"Uptime"</div>
                                                        <div class="stat-value text-lg font-mono">{move || format_duration(live_uptime.get())}</div>
                                                    </div>
                                                    <div class="stat">
                                                        <div class="stat-title">"Links"</div>
                                                        <div class="stat-value text-lg font-mono">{move || live_links.get().len()}</div>
                                                    </div>
                                                </div>

                                                // Link stats table
                                                {move || {
                                                    let links = live_links.get();
                                                    if links.is_empty() {
                                                        view! { <p class="text-sm text-base-content/40 mt-3">"Waiting for link stats…"</p> }.into_any()
                                                    } else {
                                                        view! {
                                                            <div class="overflow-x-auto mt-3">
                                                                <table class="table table-sm">
                                                                    <thead>
                                                                        <tr>
                                                                            <th>"Interface"</th>
                                                                            <th>"State"</th>
                                                                            <th>"RTT"</th>
                                                                            <th>"Loss"</th>
                                                                            <th>"Throughput"</th>
                                                                            <th>"Capacity"</th>
                                                                            <th>"Sent"</th>
                                                                        </tr>
                                                                    </thead>
                                                                    <tbody>
                                                                        <For
                                                                            each=move || live_links.get()
                                                                            key=|l| l.id
                                                                            children=move |link| {
                                                                                let state_cls = match link.state.as_str() {
                                                                                    "Live" => "badge badge-success badge-sm",
                                                                                    "Probing" => "badge badge-warning badge-sm",
                                                                                    "Down" | "OS Down" => "badge badge-error badge-sm",
                                                                                    _ => "badge badge-ghost badge-sm",
                                                                                };
                                                                                let row_cls = if link.state == "Down" || link.state == "OS Down" {
                                                                                    "font-mono text-sm opacity-50"
                                                                                } else {
                                                                                    "font-mono text-sm"
                                                                                };
                                                                                view! {
                                                                                    <tr class=row_cls>
                                                                                        <td>
                                                                                            {link.interface.clone()}
                                                                                            {link.link_kind.as_ref().map(|k| view! {
                                                                                                <span class="text-xs text-base-content/40 ml-1">"(" {k.clone()} ")"</span>
                                                                                            })}
                                                                                        </td>
                                                                                        <td><span class=state_cls>{link.state.clone()}</span></td>
                                                                                        <td>{format!("{:.1}ms", link.rtt_ms)}</td>
                                                                                        <td>{format!("{:.2}%", link.loss_rate * 100.0)}</td>
                                                                                        <td>{format_bps(link.observed_bps)}</td>
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
                                                <p class="text-sm text-base-content/40">"No active stream"</p>
                                            }.into_any()
                                        }
                                    }}
                                </div>
                            </div>

                            // ── Details Card ────────────────────────────
                            <div class="card bg-base-200 border border-base-300 mb-4">
                                <div class="card-body">
                                    <h3 class="card-title text-base">"Details"</h3>
                                    <div class="overflow-x-auto">
                                        <table class="table table-sm">
                                            <tbody>
                                                <tr>
                                                    <td class="text-base-content/60 w-36">"ID"</td>
                                                    <td><code class="text-xs font-mono">{s.id.clone()}</code></td>
                                                </tr>
                                                <tr>
                                                    <td class="text-base-content/60">"Enrolled"</td>
                                                    <td>{if s.enrolled { "Yes" } else { "No" }}</td>
                                                </tr>
                                                <tr>
                                                    <td class="text-base-content/60">"Created"</td>
                                                    <td>{s.created_at.clone()}</td>
                                                </tr>
                                                <tr>
                                                    <td class="text-base-content/60">"Last seen"</td>
                                                    <td>{s.last_seen_at.clone().unwrap_or_else(|| "Never".into())}</td>
                                                </tr>
                                            </tbody>
                                        </table>
                                    </div>
                                </div>
                            </div>

                            // ── Unenroll Card (Danger Zone) ─────────────
                            <div class="card bg-base-200 border border-error mb-4">
                                <div class="card-body">
                                    <h3 class="card-title text-base text-error">"Danger Zone"</h3>

                                    {move || unenroll_token.get().map(|token| view! {
                                        <div class="bg-base-300 rounded-lg p-4 mb-3">
                                            <p class="text-success mb-2">"Sender unenrolled. New enrollment token:"</p>
                                            <code class="text-lg tracking-widest">{token}</code>
                                        </div>
                                    })}

                                    <div class="flex items-center justify-between">
                                        <div>
                                            <p class="font-medium">"Unenroll Sender"</p>
                                            <p class="text-sm text-base-content/60 mt-1">
                                                "Disconnects the sender and resets enrollment. A new token will be issued."
                                            </p>
                                        </div>
                                        {move || {
                                            let is_enrolled = sender.get().map(|s| s.enrolled).unwrap_or(false);
                                            if !is_enrolled && unenroll_token.get().is_none() {
                                                view! {
                                                    <button class="btn btn-disabled" disabled=true>
                                                        "Not Enrolled"
                                                    </button>
                                                }.into_any()
                                            } else if show_unenroll_confirm.get() {
                                                view! {
                                                    <div class="flex gap-2">
                                                        <button class="btn btn-error" on:click=do_unenroll disabled=move || action_loading.get()>
                                                            "Confirm Unenroll"
                                                        </button>
                                                        <button class="btn btn-ghost" on:click=move |_| set_show_unenroll_confirm.set(false)>
                                                            "Cancel"
                                                        </button>
                                                    </div>
                                                }.into_any()
                                            } else {
                                                view! {
                                                    <button class="btn btn-error" on:click=move |_| set_show_unenroll_confirm.set(true) disabled=move || action_loading.get()>
                                                        "Unenroll"
                                                    </button>
                                                }.into_any()
                                            }
                                        }}
                                    </div>
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
        <div class="card bg-base-200 border border-base-300 mb-4">
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"📡 Network Interfaces"</h3>
                    <div class="flex gap-2 items-center">
                        <span class="badge badge-ghost badge-sm">
                            {move || interfaces.get().len()} " interfaces"
                        </span>
                        <button class="btn btn-ghost btn-sm" on:click=do_scan disabled=!is_online>
                            "Scan for New"
                        </button>
                    </div>
                </div>

                {move || scan_msg.get().map(|(msg, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm mt-2",
                        "err" => "alert alert-error text-sm mt-2",
                        _ => "alert alert-info text-sm mt-2",
                    };
                    view! { <div class={cls}>{msg}</div> }
                })}

                {move || {
                    let ifaces = interfaces.get();
                    if ifaces.is_empty() {
                        view! {
                            <p class="text-sm text-base-content/40 mt-2">
                                "No interface data — sender may be offline"
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <div class="flex flex-col gap-2 mt-2">
                                {ifaces.into_iter().map(|iface| {
                                    let name = iface.name.clone();
                                    let name_toggle = iface.name.clone();
                                    let sender_id = sender_id.clone();
                                    let auth = auth.clone();
                                    let enabled = iface.enabled;
                                    let connected = iface.state == "connected";

                                    let (badge_cls, label) = if !enabled {
                                        ("badge badge-error badge-sm gap-1", "Disabled")
                                    } else if connected {
                                        ("badge badge-success badge-sm gap-1", "Up")
                                    } else {
                                        ("badge badge-ghost badge-sm gap-1", "Down")
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

                                    let is_loading = {
                                        let n = iface.name.clone();
                                        move || iface_loading.get().as_deref() == Some(&n)
                                    };
                                    let is_loading2 = {
                                        let n = iface.name.clone();
                                        move || iface_loading.get().as_deref() == Some(&n)
                                    };

                                    view! {
                                        <div class={if enabled {
                                            "flex items-center justify-between p-3 bg-base-300 rounded border border-base-300"
                                        } else {
                                            "flex items-center justify-between p-3 bg-base-300 rounded border border-base-300 opacity-50"
                                        }}>
                                            <div class="flex items-center gap-3">
                                                <input
                                                    type="checkbox"
                                                    class={move || if is_loading() { "toggle toggle-success toggle-sm animate-pulse" } else { "toggle toggle-success toggle-sm" }}
                                                    checked=enabled
                                                    on:change=toggle
                                                    disabled=move || is_loading2() || !is_online
                                                />
                                                <div>
                                                    <span class="font-semibold font-mono text-sm">{name}</span>
                                                    <div class="flex gap-2 text-xs text-base-content/60">
                                                        {meta_parts.into_iter().map(|p| view! {
                                                            <span>{p}</span>
                                                        }).collect::<Vec<_>>()}
                                                    </div>
                                                </div>
                                            </div>
                                            <span class={badge_cls}>{label}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

/// Media inputs card — shows detected cameras, capture cards, etc.
#[component]
fn MediaInputsCard(inputs: ReadSignal<Vec<MediaInput>>) -> impl IntoView {
    view! {
        <div class="card bg-base-200 border border-base-300 mb-4">
            <div class="card-body">
                <h3 class="card-title text-base">"🎥 Media Inputs"</h3>
                {move || {
                    let inputs = inputs.get();
                    if inputs.is_empty() {
                        view! {
                            <p class="text-sm text-base-content/40">"No inputs detected"</p>
                        }.into_any()
                    } else {
                        view! {
                            <div class="flex flex-col gap-2">
                                {inputs.into_iter().map(|input| {
                                    let caps = input.capabilities.join(", ");
                                    let status_badge = match input.status.as_str() {
                                        "available" => "badge badge-success badge-sm gap-1",
                                        "in_use" => "badge badge-error badge-sm gap-1",
                                        _ => "badge badge-ghost badge-sm gap-1",
                                    };
                                    view! {
                                        <div class="flex items-center justify-between p-3 bg-base-300 rounded border border-base-300">
                                            <div>
                                                <div class="font-medium text-sm">{input.label}</div>
                                                <div class="text-xs text-base-content/60 font-mono mt-0.5">
                                                    {input.device} " · " {input.input_type}
                                                    {(!caps.is_empty()).then(|| view! { <span>" · " {caps}</span> })}
                                                </div>
                                            </div>
                                            <span class={status_badge}>{input.status.clone()}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
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
