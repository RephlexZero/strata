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
        <div class="portal-header">
            <h1>"Strata Sender"</h1>
            <div class="header-badges">
                {move || {
                    let s = status.get();
                    let (cloud_class, cloud_dot, cloud_text) = match s.as_ref().map(|s| s.cloud_connected) {
                        Some(true) => ("badge badge-online", "dot dot-green", "Connected"),
                        _ => ("badge badge-offline", "dot dot-gray", "Offline"),
                    };
                    let (enroll_class, enroll_dot, enroll_text) = match s.as_ref().map(|s| s.enrolled) {
                        Some(true) => ("badge badge-online", "dot dot-green", "Enrolled"),
                        _ => ("badge badge-offline", "dot dot-gray", "Not Enrolled"),
                    };
                    view! {
                        <span class={cloud_class}><span class={cloud_dot}></span>{cloud_text}</span>
                        <span class={enroll_class}><span class={enroll_dot}></span>{enroll_text}</span>
                    }
                }}
            </div>
        </div>

        <div class="container">
            // ── Error ───────────────────────────────────────────
            {move || error.get().map(|e| view! {
                <div class="msg msg-err">{e}</div>
            })}

            // ── System Stats ────────────────────────────────────
            {move || status.get().map(|s| view! {
                <div class="card">
                    <div class="card-header">
                        <h2>"📊 System"</h2>
                    </div>
                    <div class="stats-grid">
                        <div class="stat-card">
                            <div class="stat-label">"CPU"</div>
                            <div class="stat-value">
                                {format!("{:.1}", s.cpu_percent)}
                                <span class="stat-unit">"%"</span>
                            </div>
                        </div>
                        <div class="stat-card">
                            <div class="stat-label">"Memory"</div>
                            <div class="stat-value">
                                {s.mem_used_mb}
                                <span class="stat-unit">" MB"</span>
                            </div>
                        </div>
                        <div class="stat-card">
                            <div class="stat-label">"Uptime"</div>
                            <div class="stat-value">{format_uptime(s.uptime_s)}</div>
                        </div>
                        <div class="stat-card">
                            <div class="stat-label">"Pipeline"</div>
                            <div class="stat-value" style={if s.streaming { "color: var(--red);" } else { "color: var(--text-muted);" }}>
                                {if s.streaming { "● Live" } else { "Idle" }}
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
            <div class="card">
                <div class="card-header">
                    <h2>"🎯 Receiver"</h2>
                </div>

                {move || config_msg.get().map(|(msg, kind)| {
                    let cls = match kind { "ok" => "msg msg-ok", "err" => "msg msg-err", _ => "msg msg-info" };
                    view! { <div class={cls}>{msg}</div> }
                })}

                <p style="font-size: 13px; color: var(--text-secondary); margin-bottom: 14px;">
                    "Set the RIST receiver address this sender will transmit to."
                </p>

                <div class="form-row">
                    <div class="form-group">
                        <label>"Receiver URL"</label>
                        <input
                            class="form-input"
                            type="text"
                            placeholder="rist://receiver.example.com:5000"
                            prop:value=move || receiver_input.get()
                            on:input=move |ev| {
                                set_receiver_input.set(event_target_value(&ev));
                            }
                        />
                    </div>
                    <button class="btn btn-primary" on:click=save_config>"Save"</button>
                </div>

                {move || {
                    let url = status.get().and_then(|s| s.receiver_url);
                    view! {
                        <p style="margin-top: 10px; font-size: 12px; color: var(--text-muted); font-family: var(--font-mono);">
                            {url.map(|u| format!("Current: {u}")).unwrap_or_else(|| "No receiver configured".into())}
                        </p>
                    }
                }}
            </div>

            // ── Media Inputs ────────────────────────────────────
            <MediaInputsCard
                inputs=Signal::derive(move || {
                    status.get().map(|s| s.inputs).unwrap_or_default()
                })
            />

            // ── Cloud Enrollment ────────────────────────────────
            <div class="card">
                <div class="card-header">
                    <h2>"🔗 Cloud Enrollment"</h2>
                </div>

                {move || enroll_msg.get().map(|(msg, kind)| {
                    let cls = match kind { "ok" => "msg msg-ok", "err" => "msg msg-err", _ => "msg msg-info" };
                    view! { <div class={cls}>{msg}</div> }
                })}

                {move || {
                    let s = status.get();
                    let enrolled = s.as_ref().map(|s| s.enrolled).unwrap_or(false);

                    if enrolled {
                        let sid = s.and_then(|s| s.sender_id).unwrap_or_else(|| "—".into());
                        view! {
                            <div class="msg msg-ok">"This device is enrolled and connected to the cloud."</div>
                            <p style="font-family: var(--font-mono); font-size: 13px; color: var(--text-secondary); margin-bottom: 12px;">
                                "Sender ID: " {sid}
                            </p>
                            {move || if show_unenroll.get() {
                                view! {
                                    <div style="display: flex; gap: 8px; align-items: center;">
                                        <button
                                            class="btn btn-danger"
                                            on:click=do_unenroll
                                            disabled=move || enroll_loading.get()
                                        >
                                            "Confirm Unenroll"
                                        </button>
                                        <button
                                            class="btn btn-secondary"
                                            on:click=move |_| set_show_unenroll.set(false)
                                        >
                                            "Cancel"
                                        </button>
                                        <span style="font-size: 13px; color: var(--text-secondary);">
                                            "This will disconnect from the cloud. You'll need a new token to re-enroll."
                                        </span>
                                    </div>
                                }.into_any()
                            } else {
                                view! {
                                    <button
                                        class="btn btn-danger btn-sm"
                                        on:click=move |_| set_show_unenroll.set(true)
                                    >
                                        "Unenroll Device"
                                    </button>
                                }.into_any()
                            }}
                        }.into_any()
                    } else {
                        view! {
                            <p style="font-size: 13px; color: var(--text-secondary); margin-bottom: 14px;">
                                "Enter the enrollment token from your Strata dashboard to link this sender to your account. "
                                "Tokens are 8 characters in "
                                <span style="font-family: var(--font-mono);">"XXXX-XXXX"</span>
                                " format."
                            </p>
                            <div class="form-group">
                                <label>"Enrollment Token"</label>
                                <input
                                    class="form-input token-input"
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
                            </div>
                            <div class="form-group">
                                <label>"Control Plane URL " <span style="color: var(--text-muted);">"(optional)"</span></label>
                                <input
                                    class="form-input"
                                    type="text"
                                    placeholder="wss://platform.example.com/agent/ws"
                                    prop:value=move || ctrl_url_input.get()
                                    on:input=move |ev| {
                                        set_ctrl_url_input.set(event_target_value(&ev));
                                    }
                                />
                            </div>
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

            // ── Connectivity Test ───────────────────────────────
            <div class="card">
                <div class="card-header">
                    <h2>"🔍 Connectivity Test"</h2>
                </div>
                <button
                    class="btn btn-ghost"
                    on:click=run_test
                    disabled=move || test_loading.get()
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
        <div class="card">
            <div class="card-header">
                <h2>"📡 Network Interfaces"</h2>
                <button class="btn btn-ghost btn-sm" on:click=do_scan>"Scan for New"</button>
            </div>

            {move || scan_msg.get().map(|(msg, kind)| {
                let cls = match kind { "ok" => "msg msg-ok", "err" => "msg msg-err", _ => "msg msg-info" };
                view! { <div class={cls}>{msg}</div> }
            })}

            {move || {
                let ifaces = interfaces.get();
                if ifaces.is_empty() {
                    view! {
                        <p style="color: var(--text-secondary);">"Scanning…"</p>
                    }.into_any()
                } else {
                    view! {
                        <div>
                            {ifaces.into_iter().map(|iface| {
                                let name = iface.name.clone();
                                let name_toggle = iface.name.clone();
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

                                let is_loading_class = {
                                    let n = iface.name.clone();
                                    move || iface_loading.get().as_deref() == Some(&n)
                                };
                                let is_loading_disabled = {
                                    let n = iface.name.clone();
                                    move || iface_loading.get().as_deref() == Some(&n)
                                };

                                view! {
                                    <div class={if enabled { "iface-item" } else { "iface-item disabled" }}>
                                        <div class="iface-left">
                                            <label class={move || if is_loading_class() { "toggle loading" } else { "toggle" }}>
                                                <input
                                                    type="checkbox"
                                                    checked=enabled
                                                    on:change=toggle
                                                    disabled=is_loading_disabled
                                                />
                                                <span class="slider"></span>
                                            </label>
                                            <span class="iface-name">{name}</span>
                                        </div>
                                        <div class="iface-right">
                                            <div class="iface-meta">
                                                {meta_parts.into_iter().map(|p| view! {
                                                    <span>{p}</span>
                                                }).collect::<Vec<_>>()}
                                            </div>
                                            <span class={badge_cls}>
                                                <span class={dot}></span>
                                                {label}
                                            </span>
                                        </div>
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

// ── Media Inputs Card ───────────────────────────────────────────────

#[component]
fn MediaInputsCard(inputs: Signal<Vec<MediaInput>>) -> impl IntoView {
    view! {
        <div class="card">
            <div class="card-header">
                <h2>"🎥 Media Inputs"</h2>
            </div>
            {move || {
                let inputs = inputs.get();
                if inputs.is_empty() {
                    view! {
                        <p style="color: var(--text-secondary);">"No inputs detected"</p>
                    }.into_any()
                } else {
                    view! {
                        <div>
                            {inputs.into_iter().map(|input| {
                                let caps = input.capabilities.join(", ");
                                view! {
                                    <div class="input-item">
                                        <div class="input-label">{input.label}</div>
                                        <div class="input-dev">
                                            {input.device} " · " {input.input_type}
                                        </div>
                                        {(!caps.is_empty()).then(|| view! {
                                            <div class="input-caps">{caps}</div>
                                        })}
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
