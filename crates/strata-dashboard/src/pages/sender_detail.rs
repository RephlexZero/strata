//! Sender detail page — tabbed management UI.
//!
//! Architecture: All signals are created once at the page level. The page
//! body uses `style:display` toggling instead of a reactive closure that
//! destroys and recreates child components on every data update. Fine-grained
//! `Memo<>` signals extract individual fields from `sender`, so only the
//! specific DOM nodes that depend on changed values re-render. Child
//! components are mounted once and survive heartbeat updates (every 5s).

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

mod cards;
mod helpers;
mod tabs;

use crate::AuthState;
use crate::api;
use crate::types::{
    DashboardEvent, LinkStats, MediaInput, NetworkInterface, SenderDetail, StreamSummary,
    TestRunResponse,
};
use crate::ws::WsClient;

use helpers::{apply_full_status, format_duration};
use tabs::{DestinationModal, DiagnosticsTab, NetworkTab, SettingsTab, SourceTab, StreamTab};

// ═══════════════════════════════════════════════════════════════════
// Page root
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn SenderDetailPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let ws = expect_context::<WsClient>();
    let params = leptos_router::hooks::use_params_map();

    // ── Core signals (created ONCE, never destroyed) ─────────────
    let (sender, set_sender) = signal(Option::<SenderDetail>::None);
    let (_streams, set_streams) = signal(Vec::<StreamSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (action_loading, set_action_loading) = signal(false);

    // Live stats from WebSocket
    let (live_bitrate, set_live_bitrate) = signal(0u32);
    let (live_uptime, set_live_uptime) = signal(0u64);
    let (live_links, set_live_links) = signal(Vec::<LinkStats>::new());
    let (live_sender_metrics, set_live_sender_metrics) =
        signal(Option::<crate::types::TransportSenderMetrics>::None);
    let (live_receiver_metrics, set_live_receiver_metrics) =
        signal(Option::<crate::types::TransportReceiverMetrics>::None);
    let (stream_state, set_stream_state) = signal(String::from("idle"));
    let (active_stream_id, set_active_stream_id) = signal(Option::<String>::None);
    let (stream_detail, set_stream_detail) = signal(Option::<crate::types::StreamDetail>::None);

    // History for graph
    let (stats_history, set_stats_history) =
        signal(std::collections::VecDeque::<(f64, Vec<LinkStats>)>::new());

    // Staleness detection
    let (last_stats_ms, set_last_stats_ms) = signal(0.0f64);
    let (signal_lost, set_signal_lost) = signal(false);

    {
        let cb = Closure::<dyn Fn()>::wrap(Box::new(move || {
            let st = stream_state.get_untracked();
            if st == "live" {
                let now = js_sys::Date::now();
                let last = last_stats_ms.get_untracked();
                set_signal_lost.set(last > 0.0 && (now - last) > 5000.0);
            } else {
                set_signal_lost.set(false);
            }
        }));
        let _ = web_sys::window()
            .unwrap()
            .set_interval_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                2000,
            );
        cb.forget();
    }

    // Hardware status (from heartbeat or REST)
    let (hw_interfaces, set_hw_interfaces) = signal(Vec::<NetworkInterface>::new());
    let (hw_inputs, set_hw_inputs) = signal(Vec::<MediaInput>::new());
    let (hw_cpu, set_hw_cpu) = signal(Option::<f32>::None);
    let (hw_mem, set_hw_mem) = signal(Option::<u32>::None);
    let (_hw_uptime, set_hw_uptime) = signal(Option::<u64>::None);
    let (hw_receiver_url, set_hw_receiver_url) = signal(Option::<String>::None);

    // Unenroll
    let (unenroll_token, set_unenroll_token) = signal(Option::<String>::None);
    let (show_unenroll_confirm, set_show_unenroll_confirm) = signal(false);

    // Interface toggle
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

    // Destination picker modal
    let (show_start_modal, set_show_start_modal) = signal(false);
    let (destinations, set_destinations) = signal(Vec::<crate::types::DestinationSummary>::new());
    let (selected_dest, set_selected_dest) = signal(Option::<String>::None);
    let (dests_loading, set_dests_loading) = signal(false);

    // Receiver URL change confirm
    let (show_receiver_confirm, set_show_receiver_confirm) = signal(false);

    // ── Active tab ───────────────────────────────────────────────
    let (active_tab, set_active_tab) = signal(String::from("stream"));

    // ── Fine-grained derived signals (Memo) ──────────────────────
    let sender_loaded = Memo::new(move |_| sender.get().is_some());
    let sender_id_memo = Memo::new(move |_| sender.get().map(|s| s.id.clone()).unwrap_or_default());
    let sender_name = Memo::new(move |_| {
        sender
            .get()
            .map(|s| s.name.clone().unwrap_or_else(|| s.id.clone()))
            .unwrap_or_default()
    });
    let hostname = Memo::new(move |_| {
        sender
            .get()
            .map(|s| s.hostname.clone().unwrap_or_else(|| "Unknown host".into()))
            .unwrap_or_default()
    });
    let is_online = Memo::new(move |_| sender.get().map(|s| s.online).unwrap_or(false));
    let is_enrolled = Memo::new(move |_| sender.get().map(|s| s.enrolled).unwrap_or(false));
    let is_live = Memo::new(move |_| {
        let st = stream_state.get();
        st == "live" || st == "starting"
    });

    // ── Data loading ─────────────────────────────────────────────
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
                    let active = filtered
                        .iter()
                        .find(|s| s.state == "live" || s.state == "starting")
                        .or(filtered.first());
                    if let Some(latest) = active {
                        set_stream_state.set(latest.state.clone());
                        set_active_stream_id.set(Some(latest.id.clone()));
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

    let auth_stream_detail = auth.clone();
    Effect::new(move || {
        let stream_id = active_stream_id.get();
        let token = auth_stream_detail.token.get();
        if let (Some(stream_id), Some(token)) = (stream_id, token) {
            leptos::task::spawn_local(async move {
                if let Ok(detail) = api::get_stream(&token, &stream_id).await {
                    set_stream_detail.set(Some(detail));
                }
            });
        }
    });

    // ── WebSocket events ─────────────────────────────────────────
    Effect::new(move || {
        if let Some(event) = ws.last_event.get() {
            let sender_id = params.get().get("id").unwrap_or_default();
            match event {
                DashboardEvent::StreamStats {
                    sender_id: stats_sender_id,
                    uptime_s,
                    encoder_bitrate_kbps,
                    links,
                    sender_metrics,
                    receiver_metrics,
                    ..
                } => {
                    if stats_sender_id == sender_id {
                        set_live_bitrate.set(encoder_bitrate_kbps);
                        set_live_uptime.set(uptime_s);
                        set_live_links.set(links.clone());
                        set_live_sender_metrics.set(sender_metrics);
                        set_live_receiver_metrics.set(receiver_metrics);

                        let now = js_sys::Date::now();
                        set_last_stats_ms.set(now);
                        set_signal_lost.set(false);

                        set_stats_history.update(|h| {
                            h.push_back((now, links));
                            if h.len() > 60 {
                                // Keep last 60 seconds
                                h.pop_front();
                            }
                        });

                        let st = stream_state.get_untracked();
                        if st == "starting" {
                            set_stream_state.set("live".into());
                        }
                    }
                }
                DashboardEvent::StreamStateChanged {
                    stream_id,
                    sender_id: sid,
                    state,
                    ..
                } => {
                    if sid == sender_id {
                        set_stream_state.set(state.clone());
                        if state == "starting" || state == "live" {
                            set_active_stream_id.set(Some(stream_id));
                        }
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

    // ── Action handlers ──────────────────────────────────────────
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
            match api::start_stream(&token, &id, dest_id, None, None).await {
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
        let online = is_online.get_untracked();
        let st = stream_state.get_untracked();
        if online || st == "live" || st == "starting" {
            set_show_receiver_confirm.set(true);
        } else {
            do_save_config(());
        }
    };

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

    // ── View ─────────────────────────────────────────────────────
    view! {
        <div>
            // Global error banner
            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm mb-4">
                    <span>{e}</span>
                    <button class="btn btn-ghost btn-xs" on:click=move |_| set_error.set(None)>"✕"</button>
                </div>
            })}

            // Modals (always mounted, shown/hidden by signal)
            <DestinationModal
                show=show_start_modal
                set_show=set_show_start_modal
                destinations=destinations
                selected_dest=selected_dest
                set_selected_dest=set_selected_dest
                dests_loading=dests_loading
                on_confirm=confirm_start_stream
            />

            {move || show_receiver_confirm.get().then(|| view! {
                <div class="modal modal-open">
                    <div class="modal-box">
                        <h3 class="font-bold text-lg text-warning">"Change Receiver URL?"</h3>
                        <p class="mt-3 text-sm">
                            "This sender is currently "
                            <strong>{if stream_state.get() == "live" { "streaming" } else { "online" }}</strong>
                            ". Changing the receiver URL may interrupt the connection."
                        </p>
                        <div class="modal-action">
                            <button class="btn btn-ghost" on:click=move |_| set_show_receiver_confirm.set(false)>"Cancel"</button>
                            <button class="btn btn-warning" on:click=move |_| do_save_config(())>"Yes, Change"</button>
                        </div>
                    </div>
                    <div class="modal-backdrop" on:click=move |_| set_show_receiver_confirm.set(false)><button>"close"</button></div>
                </div>
            })}

            // Loading state
            <div style:display=move || if sender_loaded.get() { "none" } else { "block" }>
                <p class="text-base-content/60">"Loading…"</p>
            </div>

            // ── Main content (mounted once, never destroyed) ─────
            <div style:display=move || if sender_loaded.get() { "block" } else { "none" }>

                // ── Page Header ──────────────────────────────────
                <div class="flex justify-between items-center mb-2">
                    <div>
                        <h2 class="text-2xl font-semibold">{move || sender_name.get()}</h2>
                        <p class="text-sm text-base-content/60 mt-1">
                            {move || hostname.get()}
                            " · "
                            <span class=move || if is_online.get() { "badge badge-success gap-1" } else { "badge badge-ghost gap-1" }>
                                <span class=move || if is_online.get() { "w-2 h-2 rounded-full bg-success" } else { "w-2 h-2 rounded-full bg-base-content/30" }></span>
                                {move || if is_online.get() { "Online" } else { "Offline" }}
                            </span>
                        </p>
                    </div>
                    <div class="flex gap-2 items-center">
                        // System stats (compact inline)
                        <div class="hidden md:flex gap-3 text-xs text-base-content/50 font-mono mr-4">
                            {move || hw_cpu.get().map(|v| view! {
                                <span>"CPU " {format!("{:.0}%", v)}</span>
                            })}
                            {move || hw_mem.get().map(|v| view! {
                                <span>"RAM " {v} "MB"</span>
                            })}
                        </div>
                        {move || {
                            let auth = auth.clone();
                            if is_live.get() {
                                let auth = auth.clone();
                                view! {
                                    <button class="btn btn-error" on:click=stop_stream disabled=move || action_loading.get() || !auth.has_role("operator")>
                                        "Stop Stream"
                                    </button>
                                }.into_any()
                            } else {
                                let auth = auth.clone();
                                view! {
                                    <button class="btn btn-error font-bold" on:click=open_start_modal
                                        disabled=move || action_loading.get() || !is_online.get() || !auth.has_role("operator")>
                                        "Go Live"
                                    </button>
                                }.into_any()
                            }
                        }}
                    </div>
                </div>

                // ── Live banner (always mounted, hidden when not live) ──
                <div
                    class="bg-base-200 border border-base-300 rounded-lg p-3 mb-4"
                    style:display=move || if is_live.get() { "block" } else { "none" }
                >
                    <div class="flex items-center justify-between flex-wrap gap-2">
                        <div class="flex items-center gap-4">
                            <div class="flex items-center gap-2">
                                {move || signal_lost.get().then(|| view! {
                                    <span class="badge badge-warning badge-sm animate-pulse">"Signal Lost"</span>
                                })}
                                {move || {
                                    let sm = live_sender_metrics.get();
                                    let rm = live_receiver_metrics.get();
                                    let mut degraded = false;
                                    if let Some(s) = sm.as_ref()
                                        && s.packets_sent > 0
                                    {
                                        let pre_fec_loss = (s.retransmissions as f64 / s.packets_sent as f64) * 100.0;
                                        if pre_fec_loss > 5.0 { degraded = true; }
                                    }
                                    if let (Some(s), Some(r)) = (sm.as_ref(), rm.as_ref())
                                        && s.packets_sent > 0
                                    {
                                        let lost = s.packets_sent.saturating_sub(r.packets_delivered);
                                        let post_fec_loss = (lost as f64 / s.packets_sent as f64) * 100.0;
                                        if post_fec_loss > 1.0 { degraded = true; }
                                    }
                                    degraded.then(|| view! {
                                        <span class="badge badge-warning badge-sm">"Degraded"</span>
                                    })
                                }}
                                <span class=move || {
                                    let st = stream_state.get();
                                    match st.as_str() {
                                        "live" => "badge badge-error gap-1",
                                        "starting" | "stopping" => "badge badge-warning gap-1",
                                        _ => "badge badge-ghost gap-1",
                                    }
                                }>
                                    <span class=move || {
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
                            <div class="text-sm font-mono">
                                <span class="text-base-content/50">"Bitrate "</span>
                                <span class="font-bold">{move || live_bitrate.get()}</span>
                                <span class="text-base-content/50">" kbps"</span>
                            </div>
                            <div class="text-sm font-mono">
                                <span class="text-base-content/50">"Uptime "</span>
                                <span class="font-bold">{move || format_duration(live_uptime.get())}</span>
                            </div>
                            <div class="text-sm font-mono">
                                <span class="text-base-content/50">"Links "</span>
                                <span class="font-bold">{move || live_links.get().len()}</span>
                            </div>
                        </div>
                    </div>
                </div>

                // ── Tab bar ──────────────────────────────────────
                <div role="tablist" class="tabs tabs-bordered mb-4">
                    {["stream", "source", "network", "diagnostics", "settings"].into_iter().map(|tab| {
                        let label = match tab {
                            "stream" => "Stream",
                            "source" => "Source",
                            "network" => "Network",
                            "diagnostics" => "Diagnostics",
                            "settings" => "Settings",
                            _ => tab,
                        };
                        view! {
                            <a
                                role="tab"
                                class=move || if active_tab.get() == tab { "tab tab-active" } else { "tab" }
                                on:click=move |_| set_active_tab.set(tab.into())
                            >
                                {label}
                            </a>
                        }
                    }).collect::<Vec<_>>()}
                </div>

                // ── Tab panels (all mounted, visibility toggled) ─

                // STREAM TAB
                <div style:display=move || if active_tab.get() == "stream" { "block" } else { "none" }>
                    <StreamTab
                        stream_state=stream_state
                        live_links=live_links
                        live_bitrate=live_bitrate
                        stats_history=stats_history
                        sender_metrics=live_sender_metrics
                        receiver_metrics=live_receiver_metrics
                        sender_id=sender_id_memo
                        stream_detail=stream_detail
                    />
                </div>

                // SOURCE TAB
                <div style:display=move || if active_tab.get() == "source" { "block" } else { "none" }>
                    <SourceTab
                        sender_id=sender_id_memo
                        is_live=is_live
                        hw_inputs=hw_inputs
                    />
                </div>

                // NETWORK TAB
                <div style:display=move || if active_tab.get() == "network" { "block" } else { "none" }>
                    <NetworkTab
                        sender_id=sender_id_memo
                        interfaces=hw_interfaces
                        is_online=is_online
                        iface_loading=iface_loading
                        set_iface_loading=set_iface_loading
                        scan_msg=scan_msg
                        set_scan_msg=set_scan_msg
                        set_error=set_error
                    />
                </div>

                // DIAGNOSTICS TAB
                <div style:display=move || if active_tab.get() == "diagnostics" { "block" } else { "none" }>
                    <DiagnosticsTab sender_id=sender_id_memo is_online=is_online />
                </div>

                // SETTINGS TAB
                <div style:display=move || if active_tab.get() == "settings" { "block" } else { "none" }>
                    <SettingsTab
                        sender_id=sender_id_memo
                        is_online=is_online
                        is_enrolled=is_enrolled
                        receiver_input=receiver_input
                        set_receiver_input=set_receiver_input
                        hw_receiver_url=hw_receiver_url
                        config_msg=config_msg
                        save_config=save_config
                        test_loading=test_loading
                        test_result=test_result
                        run_test=run_test
                        unenroll_token=unenroll_token
                        show_unenroll_confirm=show_unenroll_confirm
                        set_show_unenroll_confirm=set_show_unenroll_confirm
                        do_unenroll=do_unenroll
                        action_loading=action_loading
                        sender=sender
                    />
                </div>
            </div>
        </div>
    }
}
