//! Sender detail page — tabbed management UI.
//!
//! Architecture: All signals are created once at the page level. The page
//! body uses `style:display` toggling instead of a reactive closure that
//! destroys and recreates child components on every data update. Fine-grained
//! `Memo<>` signals extract individual fields from `sender`, so only the
//! specific DOM nodes that depend on changed values re-render. Child
//! components are mounted once and survive heartbeat updates (every 5s).

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::api;
use crate::types::{
    DashboardEvent, EncoderConfigUpdate, LinkStats, MediaInput, NetworkInterface, SenderDetail,
    SenderFullStatus, SourceSwitchRequest, StreamConfigUpdateRequest, StreamSummary,
    TestRunResponse,
};
use crate::ws::WsClient;
use crate::AuthState;

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
    let (stream_state, set_stream_state) = signal(String::from("idle"));

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
                    ..
                } => {
                    if stats_sender_id == sender_id {
                        set_live_bitrate.set(encoder_bitrate_kbps);
                        set_live_uptime.set(uptime_s);
                        set_live_links.set(links);
                        set_last_stats_ms.set(js_sys::Date::now());
                        set_signal_lost.set(false);
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
                            if is_live.get() {
                                view! {
                                    <button class="btn btn-error btn-sm" on:click=stop_stream disabled=move || action_loading.get()>
                                        "Stop Stream"
                                    </button>
                                }.into_any()
                            } else {
                                view! {
                                    <button class="btn btn-primary btn-sm" on:click=open_start_modal
                                        disabled=move || action_loading.get() || !is_online.get()>
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
                    {["stream", "source", "network", "settings"].into_iter().map(|tab| {
                        let label = match tab {
                            "stream" => "Stream",
                            "source" => "Source",
                            "network" => "Network",
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
                        sender_id=sender_id_memo
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

// ═══════════════════════════════════════════════════════════════════
// Destination picker modal
// ═══════════════════════════════════════════════════════════════════

#[component]
fn DestinationModal(
    show: ReadSignal<bool>,
    set_show: WriteSignal<bool>,
    destinations: ReadSignal<Vec<crate::types::DestinationSummary>>,
    selected_dest: ReadSignal<Option<String>>,
    set_selected_dest: WriteSignal<Option<String>>,
    dests_loading: ReadSignal<bool>,
    on_confirm: impl Fn(web_sys::MouseEvent) + 'static + Copy + Send,
) -> impl IntoView {
    view! {
        {move || show.get().then(|| view! {
            <div class="modal modal-open">
                <div class="modal-box">
                    <h3 class="font-bold text-lg">"Start Stream"</h3>
                    <p class="text-sm text-base-content/60 mt-2">
                        "Select a destination, or start without one for bonded RIST only."
                    </p>
                    <div class="mt-4">
                        {move || {
                            if dests_loading.get() {
                                view! { <p class="text-sm text-base-content/40">"Loading destinations…"</p> }.into_any()
                            } else {
                                let dests = destinations.get();
                                view! {
                                    <div class="flex flex-col gap-2">
                                        <label class="flex items-center gap-3 p-3 bg-base-300 rounded cursor-pointer hover:bg-base-content/10 border border-base-300"
                                            class:border-primary=move || selected_dest.get().is_none()
                                        >
                                            <input type="radio" name="destination" class="radio radio-sm radio-primary"
                                                checked=move || selected_dest.get().is_none()
                                                on:change=move |_| set_selected_dest.set(None)
                                            />
                                            <div>
                                                <div class="font-medium text-sm">"Bonded RIST Only"</div>
                                                <div class="text-xs text-base-content/60">"No RTMP relay"</div>
                                            </div>
                                        </label>
                                        {dests.iter().map(|d| {
                                            let d_id = d.id.clone();
                                            let d_id2 = d.id.clone();
                                            let d_id3 = d.id.clone();
                                            view! {
                                                <label class="flex items-center gap-3 p-3 bg-base-300 rounded cursor-pointer hover:bg-base-content/10 border border-base-300"
                                                    class:border-primary=move || selected_dest.get().as_deref() == Some(&d_id2)
                                                >
                                                    <input type="radio" name="destination" class="radio radio-sm radio-primary"
                                                        checked=move || selected_dest.get().as_deref() == Some(&d_id3)
                                                        on:change=move |_| set_selected_dest.set(Some(d_id.clone()))
                                                    />
                                                    <div>
                                                        <div class="font-medium text-sm">{d.name.clone()}</div>
                                                        <div class="text-xs text-base-content/60 font-mono">{d.platform.clone()} " · " {d.url.clone()}</div>
                                                    </div>
                                                </label>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }.into_any()
                            }
                        }}
                    </div>
                    <div class="modal-action">
                        <button class="btn btn-ghost" on:click=move |_| set_show.set(false)>"Cancel"</button>
                        <button class="btn btn-primary" on:click=on_confirm disabled=move || dests_loading.get()>"Go Live"</button>
                    </div>
                </div>
                <div class="modal-backdrop" on:click=move |_| set_show.set(false)><button>"close"</button></div>
            </div>
        })}
    }
}

// ═══════════════════════════════════════════════════════════════════
// STREAM TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
fn StreamTab(
    stream_state: ReadSignal<String>,
    live_links: ReadSignal<Vec<LinkStats>>,
    live_bitrate: ReadSignal<u32>,
    sender_id: Memo<String>,
) -> impl IntoView {
    view! {
        <div>
            // Link performance cards
            <div class="card bg-base-200 border border-base-300 mb-4">
                <div class="card-body">
                    <h3 class="card-title text-base">"Link Performance"</h3>
                    {move || {
                        let st = stream_state.get();
                        if st != "live" && st != "starting" {
                            return view! {
                                <p class="text-sm text-base-content/40">"Start a stream to see link stats"</p>
                            }.into_any();
                        }

                        let links = live_links.get();
                        if links.is_empty() {
                            return view! {
                                <p class="text-sm text-base-content/40">"Waiting for link data…"</p>
                            }.into_any();
                        }

                        view! {
                            <div class="grid gap-3 mt-2">
                                <For
                                    each=move || live_links.get()
                                    key=|l| l.id
                                    children=move |link| {
                                        let is_down = link.state == "Down" || link.state == "OS Down";
                                        let state_cls = match link.state.as_str() {
                                            "Live" => "badge badge-success badge-sm",
                                            "Probing" => "badge badge-warning badge-sm",
                                            "Down" | "OS Down" => "badge badge-error badge-sm",
                                            _ => "badge badge-ghost badge-sm",
                                        };
                                        let iface_name = if link.interface.is_empty() || link.interface == "unknown" {
                                            format!("Link {}", link.id)
                                        } else {
                                            link.interface.clone()
                                        };

                                        view! {
                                            <div class=if is_down {
                                                "bg-base-300 rounded-lg p-3 opacity-50"
                                            } else {
                                                "bg-base-300 rounded-lg p-3"
                                            }>
                                                <div class="flex justify-between items-center mb-2">
                                                    <div class="flex items-center gap-2">
                                                        <span class="font-semibold font-mono text-sm">{iface_name}</span>
                                                        {link.link_kind.as_ref().map(|k| view! {
                                                            <span class="badge badge-ghost badge-xs">{k.clone()}</span>
                                                        })}
                                                    </div>
                                                    <span class=state_cls>{link.state.clone()}</span>
                                                </div>
                                                <div class="grid grid-cols-2 md:grid-cols-5 gap-2 text-xs">
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"RTT"</div>
                                                        <div class="font-mono font-semibold">{format!("{:.1} ms", link.rtt_ms)}</div>
                                                    </div>
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"Loss"</div>
                                                        <div class="font-mono font-semibold">{format!("{:.2}%", link.loss_rate * 100.0)}</div>
                                                    </div>
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"Throughput"</div>
                                                        <div class="font-mono font-semibold">{format_bps(link.observed_bps)}</div>
                                                    </div>
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"Capacity"</div>
                                                        <div class="font-mono font-semibold">{format_bps(link.capacity_bps)}</div>
                                                    </div>
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"Sent"</div>
                                                        <div class="font-mono font-semibold">{format_bytes(link.sent_bytes)}</div>
                                                    </div>
                                                </div>
                                            </div>
                                        }
                                    }
                                />
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>

            // Encoder controls
            <LiveSettingsCard sender_id=sender_id stream_state=stream_state live_bitrate=live_bitrate />
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// SOURCE TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
fn SourceTab(
    sender_id: Memo<String>,
    is_live: Memo<bool>,
    hw_inputs: ReadSignal<Vec<MediaInput>>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    // Source type: "test", "device", "uri"
    let (source_type, set_source_type) = signal(String::from("test"));
    let (test_pattern, set_test_pattern) = signal(String::from("smpte"));
    let (selected_device, set_selected_device) = signal(String::new());
    let (source_uri, set_source_uri) = signal(String::new());
    let (switching, set_switching) = signal(false);
    let (switch_msg, set_switch_msg) = signal(Option::<(String, &'static str)>::None);

    let do_switch = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let stype = source_type.get_untracked();
        set_switching.set(true);
        set_switch_msg.set(None);

        let req = match stype.as_str() {
            "device" => {
                let dev = selected_device.get_untracked();
                if dev.is_empty() {
                    set_switch_msg.set(Some(("Select a device first".into(), "err")));
                    set_switching.set(false);
                    return;
                }
                SourceSwitchRequest {
                    mode: "v4l2".into(),
                    device: Some(dev),
                    pattern: None,
                    uri: None,
                }
            }
            "uri" => {
                let uri = source_uri.get_untracked();
                if uri.is_empty() {
                    set_switch_msg.set(Some(("Enter a URI first".into(), "err")));
                    set_switching.set(false);
                    return;
                }
                SourceSwitchRequest {
                    mode: "uri".into(),
                    uri: Some(uri),
                    pattern: None,
                    device: None,
                }
            }
            _ => SourceSwitchRequest {
                mode: "test".into(),
                pattern: Some(test_pattern.get_untracked()),
                uri: None,
                device: None,
            },
        };

        leptos::task::spawn_local(async move {
            match api::switch_source(&token, &id, &req).await {
                Ok(()) => set_switch_msg.set(Some(("Source switched successfully".into(), "ok"))),
                Err(e) => set_switch_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_switching.set(false);
        });
    };

    view! {
        <div>
            // Not-live hint
            <div
                class="alert alert-info text-sm mb-4"
                style:display=move || if is_live.get() { "none" } else { "flex" }
            >
                "Source switching is available while a stream is running. Start a stream first."
            </div>

            {move || switch_msg.get().map(|(msg, kind)| {
                let cls = match kind {
                    "ok" => "alert alert-success text-sm mb-4",
                    _ => "alert alert-error text-sm mb-4",
                };
                view! { <div class={cls}>{msg}</div> }
            })}

            // Source type cards — radio selection
            <div class="grid gap-3 mb-4">

                // ── Test Pattern ──
                <div
                    class=move || if source_type.get() == "test" {
                        "card bg-base-200 border-2 border-primary cursor-pointer"
                    } else {
                        "card bg-base-200 border border-base-300 hover:border-base-content/20 cursor-pointer"
                    }
                    on:click=move |_| set_source_type.set("test".into())
                >
                    <div class="card-body p-4">
                        <div class="flex items-center gap-3">
                            <input type="radio" name="source_type" class="radio radio-primary radio-sm"
                                checked=move || source_type.get() == "test"
                                on:change=move |_| set_source_type.set("test".into())
                            />
                            <div class="flex-1">
                                <div class="font-semibold text-sm">"Test Pattern"</div>
                                <div class="text-xs text-base-content/60">"Built-in colour bars, bouncing ball, or noise"</div>
                            </div>
                        </div>
                        <div
                            class="mt-3 pl-8"
                            style:display=move || if source_type.get() == "test" { "block" } else { "none" }
                        >
                            <div class="grid grid-cols-2 md:grid-cols-4 gap-2">
                                {[("smpte", "SMPTE Bars", "Classic colour bars"),
                                  ("ball", "Bouncing Ball", "Moving target for latency"),
                                  ("snow", "Snow", "Random noise"),
                                  ("black", "Black", "Solid black frame"),
                                ].into_iter().map(|(val, name, desc)| {
                                    view! {
                                        <label
                                            class=move || if test_pattern.get() == val {
                                                "cursor-pointer p-3 rounded-lg text-center border bg-primary/10 border-primary"
                                            } else {
                                                "cursor-pointer p-3 rounded-lg text-center border bg-base-300 border-base-300 hover:border-base-content/20"
                                            }
                                        >
                                            <input type="radio" name="test_pattern" class="hidden"
                                                checked=move || test_pattern.get() == val
                                                on:change=move |_| set_test_pattern.set(val.into())
                                            />
                                            <div class="text-sm font-medium">{name}</div>
                                            <div class="text-xs text-base-content/50 mt-0.5">{desc}</div>
                                        </label>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                    </div>
                </div>

                // ── Capture Device ──
                <div
                    class=move || if source_type.get() == "device" {
                        "card bg-base-200 border-2 border-primary cursor-pointer"
                    } else {
                        "card bg-base-200 border border-base-300 hover:border-base-content/20 cursor-pointer"
                    }
                    on:click=move |_| set_source_type.set("device".into())
                >
                    <div class="card-body p-4">
                        <div class="flex items-center gap-3">
                            <input type="radio" name="source_type" class="radio radio-primary radio-sm"
                                checked=move || source_type.get() == "device"
                                on:change=move |_| set_source_type.set("device".into())
                            />
                            <div class="flex-1">
                                <div class="font-semibold text-sm">"Capture Device"</div>
                                <div class="text-xs text-base-content/60">"Camera or HDMI capture card (/dev/video*)"</div>
                            </div>
                        </div>
                        <div
                            class="mt-3 pl-8"
                            style:display=move || if source_type.get() == "device" { "block" } else { "none" }
                        >
                            {move || {
                                let inputs = hw_inputs.get();
                                let video_inputs: Vec<_> = inputs.into_iter()
                                    .filter(|i| i.device.starts_with("/dev/video"))
                                    .collect();

                                if video_inputs.is_empty() {
                                    view! {
                                        <p class="text-sm text-base-content/40">
                                            "No video capture devices detected on this sender."
                                        </p>
                                    }.into_any()
                                } else {
                                    view! {
                                        <div class="flex flex-col gap-2">
                                            {video_inputs.into_iter().map(|input| {
                                                let dev = input.device.clone();
                                                let dev2 = input.device.clone();
                                                let dev3 = input.device.clone();
                                                let caps = input.capabilities.join(", ");
                                                let status_badge = match input.status.as_str() {
                                                    "available" => "badge badge-success badge-xs",
                                                    "in_use" => "badge badge-warning badge-xs",
                                                    _ => "badge badge-ghost badge-xs",
                                                };
                                                view! {
                                                    <label
                                                        class=move || if selected_device.get() == dev2 {
                                                            "flex items-center gap-3 p-3 rounded-lg cursor-pointer border bg-primary/10 border-primary"
                                                        } else {
                                                            "flex items-center gap-3 p-3 rounded-lg cursor-pointer border bg-base-300 border-base-300 hover:border-base-content/20"
                                                        }
                                                    >
                                                        <input type="radio" name="device" class="radio radio-sm radio-primary"
                                                            checked=move || selected_device.get() == dev3
                                                            on:change=move |_| set_selected_device.set(dev.clone())
                                                        />
                                                        <div class="flex-1">
                                                            <div class="flex items-center gap-2">
                                                                <span class="font-mono text-sm font-medium">{input.device.clone()}</span>
                                                                <span class=status_badge>{input.status.clone()}</span>
                                                            </div>
                                                            <div class="text-xs text-base-content/60">
                                                                {input.label}
                                                                {(!caps.is_empty()).then(|| view! { <span>" · " {caps}</span> })}
                                                            </div>
                                                        </div>
                                                    </label>
                                                }
                                            }).collect::<Vec<_>>()}
                                        </div>
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>
                </div>

                // ── Media File / URI ──
                <div
                    class=move || if source_type.get() == "uri" {
                        "card bg-base-200 border-2 border-primary cursor-pointer"
                    } else {
                        "card bg-base-200 border border-base-300 hover:border-base-content/20 cursor-pointer"
                    }
                    on:click=move |_| set_source_type.set("uri".into())
                >
                    <div class="card-body p-4">
                        <div class="flex items-center gap-3">
                            <input type="radio" name="source_type" class="radio radio-primary radio-sm"
                                checked=move || source_type.get() == "uri"
                                on:change=move |_| set_source_type.set("uri".into())
                            />
                            <div class="flex-1">
                                <div class="font-semibold text-sm">"Media File / URL"</div>
                                <div class="text-xs text-base-content/60">"Play a file, HTTP URL, or RTSP stream"</div>
                            </div>
                        </div>
                        <div
                            class="mt-3 pl-8"
                            style:display=move || if source_type.get() == "uri" { "block" } else { "none" }
                        >
                            <fieldset class="fieldset">
                                <label class="fieldset-label">"Media URI"</label>
                                <input
                                    type="text"
                                    class="input input-bordered w-full"
                                    placeholder="file:///media/video.mp4 or https://example.com/stream.mp4"
                                    prop:value=move || source_uri.get()
                                    on:input=move |ev| set_source_uri.set(event_target_value(&ev))
                                />
                            </fieldset>
                            <p class="text-xs text-base-content/40 mt-1">
                                "Supports file://, http://, https://, rtsp://, or any GStreamer-compatible URI. "
                                "File paths are on the sender device."
                            </p>
                        </div>
                    </div>
                </div>
            </div>

            // Apply button
            <div class="flex justify-end">
                <button
                    class="btn btn-primary"
                    on:click=do_switch
                    disabled=move || switching.get() || !is_live.get()
                >
                    {move || if switching.get() { "Switching…" } else { "Switch Source" }}
                </button>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// NETWORK TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
fn NetworkTab(
    sender_id: Memo<String>,
    interfaces: ReadSignal<Vec<NetworkInterface>>,
    is_online: Memo<bool>,
    iface_loading: ReadSignal<Option<String>>,
    set_iface_loading: WriteSignal<Option<String>>,
    scan_msg: ReadSignal<Option<(String, &'static str)>>,
    set_scan_msg: WriteSignal<Option<(String, &'static str)>>,
    set_error: WriteSignal<Option<String>>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let auth_scan = auth.clone();
    let do_scan = move |_| {
        let token = auth_scan.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_scan_msg.set(Some(("Scanning…".into(), "info")));
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
                                "Found {} new: {}",
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
        <div>
            <div class="flex justify-between items-center mb-3">
                <div class="flex items-center gap-2">
                    <h3 class="text-lg font-semibold">"Network Interfaces"</h3>
                    <span class="badge badge-ghost badge-sm">
                        {move || interfaces.get().len()} " total"
                    </span>
                </div>
                <button class="btn btn-ghost btn-sm" on:click=do_scan
                    disabled=move || !is_online.get()
                >
                    "Scan for New"
                </button>
            </div>

            {move || scan_msg.get().map(|(msg, kind)| {
                let cls = match kind {
                    "ok" => "alert alert-success text-sm mb-3",
                    "err" => "alert alert-error text-sm mb-3",
                    _ => "alert alert-info text-sm mb-3",
                };
                view! { <div class={cls}>{msg}</div> }
            })}

            {move || {
                let ifaces = interfaces.get();
                if ifaces.is_empty() {
                    return view! {
                        <p class="text-sm text-base-content/40">"No interface data — sender may be offline"</p>
                    }.into_any();
                }

                view! {
                    <div class="grid gap-2">
                        {ifaces.into_iter().map(|iface| {
                            let name = iface.name.clone();
                            let name_toggle = iface.name.clone();
                            let auth = auth.clone();
                            let enabled = iface.enabled;
                            let connected = iface.state == "connected";

                            let (badge_cls, label) = if !enabled {
                                ("badge badge-error badge-sm", "Disabled")
                            } else if connected {
                                ("badge badge-success badge-sm", "Up")
                            } else {
                                ("badge badge-ghost badge-sm", "Down")
                            };

                            let type_icon = match iface.iface_type.as_str() {
                                "cellular" => "📶",
                                "wifi" => "📡",
                                _ => "🔌",
                            };

                            let mut meta = vec![format!("{type_icon} {}", iface.iface_type)];
                            if let Some(t) = &iface.technology { meta.push(t.clone()); }
                            if let Some(c) = &iface.carrier { meta.push(c.clone()); }
                            if let Some(db) = iface.signal_dbm { meta.push(format!("{db} dBm")); }
                            if let Some(ip) = &iface.ip { meta.push(ip.clone()); }

                            let toggle = move |_| {
                                let sid = sender_id.get_untracked();
                                let iface_name = name_toggle.clone();
                                let token = auth.token.get_untracked().unwrap_or_default();
                                set_iface_loading.set(Some(iface_name.clone()));
                                leptos::task::spawn_local(async move {
                                    let result = if enabled {
                                        api::disable_interface(&token, &sid, &iface_name).await
                                    } else {
                                        api::enable_interface(&token, &sid, &iface_name).await
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
                                <div class=if enabled {
                                    "flex items-center justify-between p-3 bg-base-200 rounded-lg border border-base-300"
                                } else {
                                    "flex items-center justify-between p-3 bg-base-200 rounded-lg border border-base-300 opacity-50"
                                }>
                                    <div class="flex items-center gap-3">
                                        <input
                                            type="checkbox"
                                            class=move || if is_loading() { "toggle toggle-success toggle-sm animate-pulse" } else { "toggle toggle-success toggle-sm" }
                                            checked=enabled
                                            on:change=toggle
                                            disabled=move || is_loading2() || !is_online.get()
                                        />
                                        <div>
                                            <span class="font-semibold font-mono text-sm">{name}</span>
                                            <div class="flex gap-2 text-xs text-base-content/60">
                                                {meta.into_iter().map(|p| view! {
                                                    <span>{p}</span>
                                                }).collect::<Vec<_>>()}
                                            </div>
                                        </div>
                                    </div>
                                    <span class=badge_cls>{label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// SETTINGS TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
fn SettingsTab(
    sender_id: Memo<String>,
    is_online: Memo<bool>,
    is_enrolled: Memo<bool>,
    receiver_input: ReadSignal<String>,
    set_receiver_input: WriteSignal<String>,
    hw_receiver_url: ReadSignal<Option<String>>,
    config_msg: ReadSignal<Option<(String, &'static str)>>,
    save_config: impl Fn(web_sys::MouseEvent) + 'static + Copy + Send,
    test_loading: ReadSignal<bool>,
    test_result: ReadSignal<Option<TestRunResponse>>,
    run_test: impl Fn(web_sys::MouseEvent) + 'static + Copy + Send,
    unenroll_token: ReadSignal<Option<String>>,
    show_unenroll_confirm: ReadSignal<bool>,
    set_show_unenroll_confirm: WriteSignal<bool>,
    do_unenroll: impl Fn(web_sys::MouseEvent) + 'static + Copy + Send,
    action_loading: ReadSignal<bool>,
    sender: ReadSignal<Option<SenderDetail>>,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-4">
            // ── Receiver Config ──
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <h3 class="card-title text-base">"Receiver Configuration"</h3>

                    {move || config_msg.get().map(|(msg, kind)| {
                        let cls = match kind {
                            "ok" => "alert alert-success text-sm",
                            "err" => "alert alert-error text-sm",
                            _ => "alert alert-info text-sm",
                        };
                        view! { <div class={cls}>{msg}</div> }
                    })}

                    <p class="text-sm text-base-content/60 mb-3">
                        "RIST receiver address for bonded transport."
                    </p>

                    <div class="flex gap-3 items-end">
                        <fieldset class="fieldset flex-1">
                            <label class="fieldset-label">"Receiver URL"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder="receiver.example.com:5000"
                                prop:value=move || receiver_input.get()
                                disabled=move || !is_online.get()
                                on:input=move |ev| set_receiver_input.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <button class="btn btn-primary" on:click=save_config disabled=move || !is_online.get()>
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

            // ── Connectivity Test ──
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <div class="flex justify-between items-center">
                        <h3 class="card-title text-base">"Connectivity Test"</h3>
                        <button
                            class="btn btn-ghost btn-sm"
                            on:click=run_test
                            disabled=move || test_loading.get() || !is_online.get()
                        >
                            {move || if test_loading.get() { "Testing…" } else { "Run Test" }}
                        </button>
                    </div>
                    {move || test_result.get().map(|r| view! {
                        <div class="grid grid-cols-2 md:grid-cols-4 gap-2 mt-3">
                            <div class="bg-base-300 rounded p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Cloud"</div>
                                <div class=if r.cloud_reachable { "font-semibold font-mono text-success text-sm" } else { "font-semibold font-mono text-error text-sm" }>
                                    {if r.cloud_reachable { "Reachable" } else { "Unreachable" }}
                                </div>
                            </div>
                            <div class="bg-base-300 rounded p-3">
                                <div class="text-xs text-base-content/40 uppercase">"WebSocket"</div>
                                <div class=if r.cloud_connected { "font-semibold font-mono text-success text-sm" } else { "font-semibold font-mono text-error text-sm" }>
                                    {if r.cloud_connected { "Connected" } else { "Disconnected" }}
                                </div>
                            </div>
                            <div class="bg-base-300 rounded p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Receiver"</div>
                                <div class=if r.receiver_reachable {
                                    "font-semibold font-mono text-success text-sm"
                                } else if r.receiver_url.is_some() {
                                    "font-semibold font-mono text-error text-sm"
                                } else {
                                    "font-semibold font-mono text-base-content/40 text-sm"
                                }>
                                    {if r.receiver_url.is_some() {
                                        if r.receiver_reachable { "Reachable" } else { "Unreachable" }
                                    } else {
                                        "Not configured"
                                    }}
                                </div>
                            </div>
                            <div class="bg-base-300 rounded p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Enrolled"</div>
                                <div class=if r.enrolled { "font-semibold font-mono text-success text-sm" } else { "font-semibold font-mono text-base-content/40 text-sm" }>
                                    {if r.enrolled { "Yes" } else { "No" }}
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

            // ── Device Details ──
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <h3 class="card-title text-base">"Device Details"</h3>
                    <div class="overflow-x-auto">
                        <table class="table table-sm">
                            <tbody>
                                <tr>
                                    <td class="text-base-content/60 w-36">"ID"</td>
                                    <td><code class="text-xs font-mono">{move || sender_id.get()}</code></td>
                                </tr>
                                <tr>
                                    <td class="text-base-content/60">"Enrolled"</td>
                                    <td>{move || if is_enrolled.get() { "Yes" } else { "No" }}</td>
                                </tr>
                                <tr>
                                    <td class="text-base-content/60">"Created"</td>
                                    <td>{move || sender.get().map(|s| s.created_at.clone()).unwrap_or_default()}</td>
                                </tr>
                                <tr>
                                    <td class="text-base-content/60">"Last seen"</td>
                                    <td>{move || sender.get().and_then(|s| s.last_seen_at.clone()).unwrap_or_else(|| "Never".into())}</td>
                                </tr>
                            </tbody>
                        </table>
                    </div>
                </div>
            </div>

            // ── Danger Zone ──
            <div class="card bg-base-200 border border-error">
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
                                "Disconnects and resets enrollment. A new token will be issued."
                            </p>
                        </div>
                        {move || {
                            let enrolled = is_enrolled.get();
                            if !enrolled && unenroll_token.get().is_none() {
                                view! {
                                    <button class="btn btn-disabled" disabled=true>"Not Enrolled"</button>
                                }.into_any()
                            } else if show_unenroll_confirm.get() {
                                view! {
                                    <div class="flex gap-2">
                                        <button class="btn btn-error" on:click=do_unenroll disabled=move || action_loading.get()>
                                            "Confirm"
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
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// LIVE SETTINGS (encoder controls)
// ═══════════════════════════════════════════════════════════════════

#[component]
fn LiveSettingsCard(
    sender_id: Memo<String>,
    stream_state: ReadSignal<String>,
    live_bitrate: ReadSignal<u32>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (manual_mode, set_manual_mode) = signal(false);
    let (custom_bitrate, set_custom_bitrate) = signal(2500u32);
    let (tune, set_tune) = signal(String::from("zerolatency"));
    let (applying, set_applying) = signal(false);
    let (apply_msg, set_apply_msg) = signal(Option::<(String, &'static str)>::None);

    let toggle_manual = move |_| {
        let entering = !manual_mode.get_untracked();
        if entering {
            let current = live_bitrate.get_untracked();
            if current > 0 {
                set_custom_bitrate.set(current);
            }
        }
        set_manual_mode.set(entering);
    };

    let do_apply = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_applying.set(true);
        set_apply_msg.set(None);

        let br = if manual_mode.get_untracked() {
            Some(custom_bitrate.get_untracked())
        } else {
            None
        };

        let req = StreamConfigUpdateRequest {
            encoder: Some(EncoderConfigUpdate {
                bitrate_kbps: br,
                tune: Some(tune.get_untracked()),
                ..Default::default()
            }),
            scheduler: None,
        };

        leptos::task::spawn_local(async move {
            match api::update_stream_config(&token, &id, &req).await {
                Ok(()) => set_apply_msg.set(Some(("Applied".into(), "ok"))),
                Err(e) => set_apply_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_applying.set(false);
        });
    };

    view! {
        <div
            class="card bg-base-200 border border-base-300"
            style:display=move || {
                let st = stream_state.get();
                if st == "live" || st == "starting" { "block" } else { "none" }
            }
        >
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"Encoder Settings"</h3>
                    <span class="badge badge-ghost badge-sm">"Hot Reconfig"</span>
                </div>

                {move || apply_msg.get().map(|(msg, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm mt-2",
                        _ => "alert alert-error text-sm mt-2",
                    };
                    view! { <div class={cls}>{msg}</div> }
                })}

                <div class="mt-3 flex flex-col gap-4">
                    <div class="bg-base-300 rounded-lg p-4">
                        <div class="flex justify-between items-center">
                            <div>
                                <div class="text-xs text-base-content/40 uppercase tracking-wide">"Current Encoder Bitrate"</div>
                                <div class="text-2xl font-mono font-bold mt-1">
                                    {move || live_bitrate.get()}
                                    <span class="text-sm text-base-content/60 font-normal">" kbps"</span>
                                </div>
                            </div>
                            <label class="flex items-center gap-2 cursor-pointer">
                                <span class="text-sm text-base-content/60">"Manual Override"</span>
                                <input
                                    type="checkbox"
                                    class="toggle toggle-sm toggle-primary"
                                    prop:checked=move || manual_mode.get()
                                    on:change=toggle_manual
                                />
                            </label>
                        </div>
                    </div>

                    <div style:display=move || if manual_mode.get() { "block" } else { "none" }>
                        <fieldset class="fieldset">
                            <label class="fieldset-label">"Target Bitrate (kbps)"</label>
                            <div class="flex items-center gap-3">
                                <input
                                    type="range" class="range range-sm range-primary flex-1"
                                    min="500" max="15000" step="100"
                                    prop:value=move || custom_bitrate.get().to_string()
                                    on:input=move |ev| {
                                        if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                            set_custom_bitrate.set(v);
                                        }
                                    }
                                />
                                <input
                                    type="number" class="input input-bordered input-sm w-24 font-mono text-right"
                                    min="500" max="15000" step="100"
                                    prop:value=move || custom_bitrate.get().to_string()
                                    on:input=move |ev| {
                                        if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                            set_custom_bitrate.set(v.clamp(500, 15000));
                                        }
                                    }
                                />
                            </div>
                            <div class="flex justify-between text-xs text-base-content/40 mt-0.5">
                                <span>"500"</span>
                                <span>"15,000"</span>
                            </div>
                        </fieldset>
                    </div>

                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Tune Preset"</label>
                        <select
                            class="select select-bordered select-sm w-full max-w-xs"
                            on:change=move |ev| set_tune.set(event_target_value(&ev))
                        >
                            <option value="zerolatency" selected=move || tune.get() == "zerolatency">"Zero Latency"</option>
                            <option value="fastdecode" selected=move || tune.get() == "fastdecode">"Fast Decode"</option>
                            <option value="film" selected=move || tune.get() == "film">"Film"</option>
                            <option value="animation" selected=move || tune.get() == "animation">"Animation"</option>
                            <option value="stillimage" selected=move || tune.get() == "stillimage">"Still Image"</option>
                        </select>
                    </fieldset>

                    <p class="text-xs text-base-content/40">
                        "FEC and scheduling are managed automatically."
                    </p>
                </div>

                <div class="card-actions justify-end mt-3">
                    <button
                        class="btn btn-primary btn-sm"
                        on:click=do_apply
                        disabled=move || applying.get()
                    >
                        {move || if applying.get() { "Applying…" } else { "Apply" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════

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
