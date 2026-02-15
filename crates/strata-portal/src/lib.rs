//! Strata Sender Portal — Leptos CSR WASM application.
//!
//! Local management UI served by the agent on :3001. No auth required
//! (physically local access). Provides:
//! - System stats (CPU, memory, uptime)
//! - Network interface management with toggles
//! - Cloud enrollment / unenrollment
//! - Receiver configuration
//! - Connectivity testing

pub mod api;
pub mod types;

use leptos::prelude::*;
use types::{DeviceStatus, MediaInput, NetworkInterface, TestResult};

/// Leptos application root.
#[component]
pub fn App() -> impl IntoView {
    // Core state
    let (status, set_status) = signal(Option::<DeviceStatus>::None);
    let (error, set_error) = signal(Option::<String>::None);

    // Enrollment
    let (token_input, set_token_input) = signal(String::new());
    let (ctrl_url_input, set_ctrl_url_input) = signal(String::new());
    let (enroll_loading, set_enroll_loading) = signal(false);
    let (enroll_msg, set_enroll_msg) = signal(Option::<(String, &'static str)>::None);

    // Config
    let (receiver_input, set_receiver_input) = signal(String::new());
    let (config_msg, set_config_msg) = signal(Option::<(String, &'static str)>::None);
    let (receiver_loaded, set_receiver_loaded) = signal(false);

    // Interface toggle loading
    let (iface_loading, set_iface_loading) = signal(Option::<String>::None);
    let (scan_msg, set_scan_msg) = signal(Option::<(String, &'static str)>::None);

    // Test
    let (test_result, set_test_result) = signal(Option::<TestResult>::None);
    let (test_loading, set_test_loading) = signal(false);

    // Unenroll confirm
    let (show_unenroll, set_show_unenroll) = signal(false);

    // ── Auto-refresh status every 3s ────────────────────────────
    let refresh = move || {
        leptos::task::spawn_local(async move {
            match api::get_status().await {
                Ok(s) => {
                    // Pre-populate receiver input on first load
                    if !receiver_loaded.get_untracked() {
                        if let Some(url) = &s.receiver_url {
                            set_receiver_input.set(url.clone());
                        }
                        set_receiver_loaded.set(true);
                    }
                    set_status.set(Some(s));
                    set_error.set(None);
                }
                Err(e) => set_error.set(Some(format!("Status refresh failed: {e}"))),
            }
        });
    };

    // Initial load
    refresh();

    // Periodic refresh
    Effect::new(move || {
        use gloo_timers::callback::Interval;
        let interval = Interval::new(3_000, refresh);
        // Keep interval alive — leak the handle (standard pattern for Leptos Effects)
        std::mem::forget(interval);
    });

    // ── Enrollment handler ──────────────────────────────────────
    let do_enroll = move |_| {
        let token = token_input.get_untracked();
        if token.is_empty() {
            set_enroll_msg.set(Some(("Enrollment token is required".into(), "err")));
            return;
        }
        let ctrl = ctrl_url_input.get_untracked();
        let ctrl_url = if ctrl.is_empty() { None } else { Some(ctrl) };

        set_enroll_loading.set(true);
        set_enroll_msg.set(Some(("Connecting to cloud…".into(), "info")));

        leptos::task::spawn_local(async move {
            match api::enroll(&token, ctrl_url).await {
                Ok(resp) => {
                    set_enroll_msg.set(Some((
                        resp.message
                            .unwrap_or_else(|| "Enrollment initiated".into()),
                        "ok",
                    )));
                    // Poll until enrolled
                    let mut tries = 0u32;
                    loop {
                        gloo_timers::future::TimeoutFuture::new(2_000).await;
                        tries += 1;
                        if let Ok(s) = api::get_status().await {
                            if s.enrolled {
                                set_enroll_msg
                                    .set(Some(("Device enrolled successfully!".into(), "ok")));
                                set_status.set(Some(s));
                                break;
                            }
                        }
                        if tries > 15 {
                            set_enroll_msg
                                .set(Some(("Enrollment timed out. Check logs.".into(), "err")));
                            break;
                        }
                    }
                    set_enroll_loading.set(false);
                }
                Err(e) => {
                    set_enroll_msg.set(Some((e, "err")));
                    set_enroll_loading.set(false);
                }
            }
        });
    };

    // ── Unenroll handler ────────────────────────────────────────
    let do_unenroll = move |_| {
        set_show_unenroll.set(false);
        set_enroll_loading.set(true);
        leptos::task::spawn_local(async move {
            match api::unenroll().await {
                Ok(resp) => {
                    set_enroll_msg.set(Some((
                        resp.message.unwrap_or_else(|| "Device unenrolled".into()),
                        "ok",
                    )));
                    set_token_input.set(String::new());
                    // Refresh immediately
                    if let Ok(s) = api::get_status().await {
                        set_status.set(Some(s));
                    }
                }
                Err(e) => set_enroll_msg.set(Some((e, "err"))),
            }
            set_enroll_loading.set(false);
        });
    };

    // ── Config save handler ─────────────────────────────────────
    let save_config = move |_| {
        let url = receiver_input.get_untracked();
        leptos::task::spawn_local(async move {
            match api::set_config(Some(url), None).await {
                Ok(resp) => {
                    let msg = if resp.receiver_url.is_some() {
                        "Configuration saved"
                    } else {
                        "Receiver URL cleared"
                    };
                    set_config_msg.set(Some((msg.into(), "ok")));
                }
                Err(e) => set_config_msg.set(Some((format!("Save failed: {e}"), "err"))),
            }
        });
    };

    // ── Test handler ────────────────────────────────────────────
    let run_test = move |_| {
        set_test_loading.set(true);
        set_test_result.set(None);
        leptos::task::spawn_local(async move {
            match api::run_test().await {
                Ok(r) => set_test_result.set(Some(r)),
                Err(e) => set_error.set(Some(format!("Test failed: {e}"))),
            }
            set_test_loading.set(false);
        });
    };

    view! {
        // ── Header ──────────────────────────────────────────────
        <div class="bg-base-200 border-b border-base-300 px-6 py-4 flex justify-between items-center">
            <h1 class="text-xl font-semibold">"Strata Sender"</h1>
            <div class="flex gap-2">
                {move || {
                    let s = status.get();
                    let (cloud_cls, cloud_text) = match s.as_ref().map(|s| s.cloud_connected) {
                        Some(true) => ("badge badge-success gap-1", "Connected"),
                        _ => ("badge badge-ghost gap-1", "Offline"),
                    };
                    let (enroll_cls, enroll_text) = match s.as_ref().map(|s| s.enrolled) {
                        Some(true) => ("badge badge-success gap-1", "Enrolled"),
                        _ => ("badge badge-ghost gap-1", "Not Enrolled"),
                    };
                    view! {
                        <span class={cloud_cls}>
                            <span class={if cloud_text == "Connected" { "w-2 h-2 rounded-full bg-success" } else { "w-2 h-2 rounded-full bg-base-content/30" }}></span>
                            {cloud_text}
                        </span>
                        <span class={enroll_cls}>
                            <span class={if enroll_text == "Enrolled" { "w-2 h-2 rounded-full bg-success" } else { "w-2 h-2 rounded-full bg-base-content/30" }}></span>
                            {enroll_text}
                        </span>
                    }
                }}
            </div>
        </div>

        <div class="max-w-3xl mx-auto px-4 py-6 space-y-4">
            // ── Error ───────────────────────────────────────────
            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm">{e}</div>
            })}

            // ── System Stats ────────────────────────────────────
            {move || status.get().map(|s| view! {
                <div class="card bg-base-200 border border-base-300">
                    <div class="card-body">
                        <h2 class="card-title text-base">"📊 System"</h2>
                        <div class="stats stats-horizontal bg-base-300 w-full">
                            <div class="stat">
                                <div class="stat-title">"CPU"</div>
                                <div class="stat-value text-lg font-mono">
                                    {format!("{:.1}", s.cpu_percent)}
                                    <span class="text-sm text-base-content/60">" %"</span>
                                </div>
                            </div>
                            <div class="stat">
                                <div class="stat-title">"Memory"</div>
                                <div class="stat-value text-lg font-mono">
                                    {s.mem_used_mb}
                                    <span class="text-sm text-base-content/60">" MB"</span>
                                </div>
                            </div>
                            <div class="stat">
                                <div class="stat-title">"Uptime"</div>
                                <div class="stat-value text-lg font-mono">{format_uptime(s.uptime_s)}</div>
                            </div>
                            <div class="stat">
                                <div class="stat-title">"Pipeline"</div>
                                <div class={if s.streaming { "stat-value text-lg font-mono text-error" } else { "stat-value text-lg font-mono text-base-content/40" }}>
                                    {if s.streaming { "● Live" } else { "Idle" }}
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            })}

            // ── Network Interfaces ──────────────────────────────
            <InterfacesCard
                interfaces=Signal::derive(move || {
                    status.get().map(|s| s.interfaces).unwrap_or_default()
                })
                iface_loading=iface_loading
                set_iface_loading=set_iface_loading
                scan_msg=scan_msg
                set_scan_msg=set_scan_msg
                set_error=set_error
            />

            // ── Receiver Config ─────────────────────────────────
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <h2 class="card-title text-base">"🎯 Receiver"</h2>

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
                                on:input=move |ev| {
                                    set_receiver_input.set(event_target_value(&ev));
                                }
                            />
                        </fieldset>
                        <button class="btn btn-primary" on:click=save_config>"Save"</button>
                    </div>

                    {move || {
                        let url = status.get().and_then(|s| s.receiver_url);
                        view! {
                            <p class="mt-2 text-xs text-base-content/40 font-mono">
                                {url.map(|u| format!("Current: {u}")).unwrap_or_else(|| "No receiver configured".into())}
                            </p>
                        }
                    }}
                </div>
            </div>

            // ── Media Inputs ────────────────────────────────────
            <MediaInputsCard
                inputs=Signal::derive(move || {
                    status.get().map(|s| s.inputs).unwrap_or_default()
                })
            />

            // ── Cloud Enrollment ────────────────────────────────
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <h2 class="card-title text-base">"🔗 Cloud Enrollment"</h2>

                    {move || enroll_msg.get().map(|(msg, kind)| {
                        let cls = match kind {
                            "ok" => "alert alert-success text-sm",
                            "err" => "alert alert-error text-sm",
                            _ => "alert alert-info text-sm",
                        };
                        view! { <div class={cls}>{msg}</div> }
                    })}

                    {move || {
                        let s = status.get();
                        let enrolled = s.as_ref().map(|s| s.enrolled).unwrap_or(false);

                        if enrolled {
                            let sid = s.and_then(|s| s.sender_id).unwrap_or_else(|| "—".into());
                            view! {
                                <div class="alert alert-success text-sm">"This device is enrolled and connected to the cloud."</div>
                                <p class="font-mono text-sm text-base-content/60 mb-3">
                                    "Sender ID: " {sid}
                                </p>
                                {move || if show_unenroll.get() {
                                    view! {
                                        <div class="flex gap-2 items-center">
                                            <button
                                                class="btn btn-error"
                                                on:click=do_unenroll
                                                disabled=move || enroll_loading.get()
                                            >
                                                "Confirm Unenroll"
                                            </button>
                                            <button
                                                class="btn btn-ghost"
                                                on:click=move |_| set_show_unenroll.set(false)
                                            >
                                                "Cancel"
                                            </button>
                                            <span class="text-sm text-base-content/60">
                                                "This will disconnect from the cloud. You'll need a new token to re-enroll."
                                            </span>
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <button
                                            class="btn btn-error btn-sm"
                                            on:click=move |_| set_show_unenroll.set(true)
                                        >
                                            "Unenroll Device"
                                        </button>
                                    }.into_any()
                                }}
                            }.into_any()
                        } else {
                            view! {
                                <p class="text-sm text-base-content/60 mb-3">
                                    "Enter the enrollment token from your Strata dashboard to link this sender to your account. "
                                    "Tokens are 8 characters in "
                                    <span class="font-mono">"XXXX-XXXX"</span>
                                    " format."
                                </p>
                                <fieldset class="fieldset mb-3">
                                    <label class="fieldset-label">"Enrollment Token"</label>
                                    <input
                                        class="input input-bordered w-full max-w-xs font-mono text-lg tracking-widest"
                                        type="text"
                                        placeholder="XXXX-XXXX"
                                        maxlength="9"
                                        prop:value=move || token_input.get()
                                        on:input=move |ev| {
                                            let raw = event_target_value(&ev);
                                            let formatted = format_token(&raw);
                                            set_token_input.set(formatted);
                                        }
                                    />
                                </fieldset>
                                <fieldset class="fieldset mb-3">
                                    <label class="fieldset-label">"Control Plane URL " <span class="text-base-content/40">"(optional)"</span></label>
                                    <input
                                        class="input input-bordered w-full"
                                        type="text"
                                        placeholder="wss://platform.example.com/agent/ws"
                                        prop:value=move || ctrl_url_input.get()
                                        on:input=move |ev| {
                                            set_ctrl_url_input.set(event_target_value(&ev));
                                        }
                                    />
                                </fieldset>
                                <button
                                    class="btn btn-primary"
                                    on:click=do_enroll
                                    disabled=move || enroll_loading.get()
                                >
                                    {move || if enroll_loading.get() { "Enrolling…" } else { "Enroll Device" }}
                                </button>
                            }.into_any()
                        }
                    }}
                </div>
            </div>

            // ── Connectivity Test ───────────────────────────────
            <div class="card bg-base-200 border border-base-300">
                <div class="card-body">
                    <h2 class="card-title text-base">"🔍 Connectivity Test"</h2>
                    <button
                        class="btn btn-ghost btn-sm w-fit"
                        on:click=run_test
                        disabled=move || test_loading.get()
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
        </div>
    }
}

// ── Network Interfaces Card ─────────────────────────────────────────

#[component]
fn InterfacesCard(
    interfaces: Signal<Vec<NetworkInterface>>,
    iface_loading: ReadSignal<Option<String>>,
    set_iface_loading: WriteSignal<Option<String>>,
    scan_msg: ReadSignal<Option<(String, &'static str)>>,
    set_scan_msg: WriteSignal<Option<(String, &'static str)>>,
    set_error: WriteSignal<Option<String>>,
) -> impl IntoView {
    let do_scan = move |_| {
        set_scan_msg.set(Some(("Scanning for new interfaces…".into(), "info")));
        leptos::task::spawn_local(async move {
            match api::scan_interfaces().await {
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
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h2 class="card-title text-base">"📡 Network Interfaces"</h2>
                    <button class="btn btn-ghost btn-sm" on:click=do_scan>"Scan for New"</button>
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
                            <p class="text-base-content/60">"Scanning…"</p>
                        }.into_any()
                    } else {
                        view! {
                            <div class="flex flex-col gap-2 mt-2">
                                {ifaces.into_iter().map(|iface| {
                                    let name = iface.name.clone();
                                    let name_toggle = iface.name.clone();
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
                                        let n = name_toggle.clone();
                                        set_iface_loading.set(Some(n.clone()));
                                        leptos::task::spawn_local(async move {
                                            let result = if enabled {
                                                api::disable_interface(&n).await
                                            } else {
                                                api::enable_interface(&n).await
                                            };
                                            if let Err(e) = result {
                                                set_error.set(Some(format!("Interface error: {e}")));
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
                                                    disabled=is_loading2
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

// ── Media Inputs Card ───────────────────────────────────────────────

#[component]
fn MediaInputsCard(inputs: Signal<Vec<MediaInput>>) -> impl IntoView {
    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h2 class="card-title text-base">"🎥 Media Inputs"</h2>
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

// ── Helpers ─────────────────────────────────────────────────────────

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Format token input as XXXX-XXXX (uppercase, alphanumeric only).
fn format_token(raw: &str) -> String {
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_uppercase();
    if clean.len() > 4 {
        format!("{}-{}", &clean[..4], &clean[4..])
    } else {
        clean
    }
}

fn event_target_value(ev: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .unwrap()
        .unchecked_into::<web_sys::HtmlInputElement>()
        .value()
}

// ── WASM entry point ────────────────────────────────────────────────

#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    let _ = console_log::init_with_level(log::Level::Debug);
    log::info!("Strata Portal starting");
    leptos::mount::mount_to_body(App);
}
