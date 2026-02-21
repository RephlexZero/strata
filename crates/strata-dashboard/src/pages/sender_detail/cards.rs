use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::AuthState;
use crate::api;
use crate::types::{EncoderConfigUpdate, LinkStats, StreamConfigUpdateRequest};

use super::helpers::format_bytes;

#[component]
pub fn BandwidthGraph(
    history: ReadSignal<std::collections::VecDeque<(f64, Vec<LinkStats>)>>,
) -> impl IntoView {
    // Colors for up to 6 links
    let colors = [
        "#3b82f6", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6", "#ec4899",
    ];

    view! {
        <div class="w-full h-32 bg-base-300 rounded-lg overflow-hidden relative">
            {move || {
                let hist = history.get();
                if hist.is_empty() {
                    return view! { <div class="absolute inset-0 flex items-center justify-center text-base-content/40 text-sm">"Waiting for data…"</div> }.into_any();
                }

                // Find max total bandwidth to scale the Y axis
                let mut max_bps = 1_000_000.0; // Minimum scale 1 Mbps
                for (_, links) in &hist {
                    let total: u64 = links.iter().map(|l| l.observed_bps).sum();
                    if total as f64 > max_bps {
                        max_bps = total as f64;
                    }
                }
                // Add 10% headroom
                max_bps *= 1.1;

                // We want to draw a stacked area chart.
                // X axis: 0 to 60 (seconds)
                // Y axis: 0 to max_bps
                let width = 800.0;
                let height = 128.0;

                // Get all unique link IDs across the history to assign consistent colors
                let mut link_ids = std::collections::HashSet::new();
                for (_, links) in &hist {
                    for l in links {
                        link_ids.insert(l.id);
                    }
                }
                let mut sorted_ids: Vec<_> = link_ids.into_iter().collect();
                sorted_ids.sort_unstable();

                // Build polygons for each link
                let mut polygons = Vec::new();
                let mut previous_y_points = vec![height; hist.len()];

                for (i, &link_id) in sorted_ids.iter().enumerate() {
                    let color = colors[i % colors.len()];
                    let mut points = String::new();
                    let mut current_y_points = Vec::with_capacity(hist.len());

                    // Top edge (left to right)
                    for (j, (_, links)) in hist.iter().enumerate() {
                        let x = (j as f64 / 59.0) * width;
                        let bps = links.iter().find(|l| l.id == link_id).map(|l| l.observed_bps).unwrap_or(0) as f64;

                        // The Y coordinate is the previous Y minus the height of this segment
                        let segment_height = (bps / max_bps) * height;
                        let y = previous_y_points[j] - segment_height;

                        points.push_str(&format!("{x},{y} "));
                        current_y_points.push(y);
                    }

                    // Bottom edge (right to left, following previous Y)
                    for j in (0..hist.len()).rev() {
                        let x = (j as f64 / 59.0) * width;
                        let y = previous_y_points[j];
                        points.push_str(&format!("{x},{y} "));
                    }

                    polygons.push(view! {
                        <polygon points=points fill=color opacity="0.8" />
                    });

                    previous_y_points = current_y_points;
                }

                // Format max label
                let max_label = if max_bps >= 1_000_000.0 {
                    format!("{:.1} Mbps", max_bps / 1_000_000.0)
                } else {
                    format!("{:.0} kbps", max_bps / 1_000.0)
                };

                view! {
                    <svg width="100%" height="100%" viewBox=format!("0 0 {width} {height}") preserveAspectRatio="none">
                        {polygons}
                    </svg>
                    <div class="absolute top-1 left-2 text-[10px] font-mono text-base-content/60 bg-base-300/80 px-1 rounded">
                        {max_label}
                    </div>
                    <div class="absolute bottom-1 left-2 text-[10px] font-mono text-base-content/60 bg-base-300/80 px-1 rounded">
                        "0 bps"
                    </div>
                }.into_any()
            }}
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// SOURCE TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn OtaUpdatesCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (update_info, set_update_info) = signal(Option::<crate::types::UpdateInfo>::None);
    let (checking, set_checking) = signal(false);
    let (installing, set_installing) = signal(false);
    let (ota_msg, set_ota_msg) = signal(Option::<(String, &'static str)>::None);

    let auth_check = auth.clone();
    let do_check = move |_: web_sys::MouseEvent| {
        let token = auth_check.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_checking.set(true);
        set_ota_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::check_updates(&token, &id).await {
                Ok(info) => set_update_info.set(Some(info)),
                Err(e) => set_ota_msg.set(Some((format!("Check failed: {e}"), "err"))),
            }
            set_checking.set(false);
        });
    };

    let do_install = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_installing.set(true);
        set_ota_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::trigger_update(&token, &id).await {
                Ok(()) => set_ota_msg.set(Some((
                    "Update initiated. Device will restart.".into(),
                    "ok",
                ))),
                Err(e) => set_ota_msg.set(Some((format!("Install failed: {e}"), "err"))),
            }
            set_installing.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"Software Updates (OTA)"</h3>
                    <button class="btn btn-ghost btn-sm" on:click=do_check
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || checking.get() || !auth.has_role("admin")
                        }
                    >
                        {move || if checking.get() { "Checking…" } else { "Check for Updates" }}
                    </button>
                </div>

                {move || ota_msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                {move || {
                    let auth = auth.clone();
                    update_info.get().map(move |info| {
                    view! {
                        <div class="grid grid-cols-2 md:grid-cols-3 gap-3 mt-2">
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Current Version"</div>
                                <div class="font-mono font-semibold text-sm">{info.current_version.clone()}</div>
                            </div>
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Latest Version"</div>
                                <div class="font-mono font-semibold text-sm">
                                    {info.latest_version.clone().unwrap_or_else(|| "—".into())}
                                </div>
                            </div>
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Status"</div>
                                <div class=if info.update_available { "font-semibold text-sm text-warning" } else { "font-semibold text-sm text-success" }>
                                    {if info.update_available { "Update Available" } else { "Up to Date" }}
                                </div>
                            </div>
                        </div>
                        {info.release_notes.clone().map(|notes| view! {
                            <div class="bg-base-300 rounded-lg p-3 mt-2 text-sm">{notes}</div>
                        })}
                        {info.update_available.then(|| {
                            let auth = auth.clone();
                            view! {
                            <div class="card-actions justify-end mt-3">
                                <button class="btn btn-warning btn-sm" on:click=do_install
                                    disabled=move || installing.get() || !auth.has_role("admin")
                                >
                                    {move || if installing.get() { "Installing…" } else { "Install Update" }}
                                </button>
                            </div>
                        }})}
                    }
                })}}
            </div>
        </div>
    }
}

// ── Live Log Viewer ─────────────────────────────────────────────

#[component]
pub fn LiveLogViewerCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (log_service, set_log_service) = signal(String::from("strata-bonding"));
    let (log_lines, set_log_lines) = signal(Vec::<crate::types::LogLine>::new());
    let (loading, set_loading) = signal(false);
    let (log_count, set_log_count) = signal(100u32);
    let (filter_text, set_filter_text) = signal(String::new());

    let do_fetch = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let svc = log_service.get_untracked();
        let count = log_count.get_untracked();
        set_loading.set(true);
        leptos::task::spawn_local(async move {
            match api::get_logs(&token, &id, Some(&svc), Some(count)).await {
                Ok(resp) => set_log_lines.set(resp.lines),
                Err(_) => set_log_lines.set(Vec::new()),
            }
            set_loading.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h3 class="card-title text-base">"Live Logs"</h3>
                <div class="flex flex-wrap gap-2 items-end mt-2">
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Service"</label>
                        <select class="select select-bordered select-sm"
                            on:change=move |ev| set_log_service.set(event_target_value(&ev))
                        >
                            <option value="strata-bonding" selected=move || log_service.get() == "strata-bonding">"strata-bonding"</option>
                            <option value="strata-gst" selected=move || log_service.get() == "strata-gst">"strata-gst"</option>
                            <option value="strata-agent" selected=move || log_service.get() == "strata-agent">"strata-agent"</option>
                        </select>
                    </fieldset>
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Lines"</label>
                        <select class="select select-bordered select-sm"
                            on:change=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<u32>() { set_log_count.set(v); } }
                        >
                            <option value="50">"50"</option>
                            <option value="100" selected=true>"100"</option>
                            <option value="500">"500"</option>
                        </select>
                    </fieldset>
                    <input
                        type="text" class="input input-bordered input-sm w-48"
                        placeholder="Filter…"
                        prop:value=move || filter_text.get()
                        on:input=move |ev| set_filter_text.set(event_target_value(&ev))
                    />
                    <button class="btn btn-ghost btn-sm" on:click=do_fetch
                        disabled=move || !is_online.get() || loading.get() || !auth.has_role("admin")
                    >
                        {move || if loading.get() { "Loading…" } else { "Fetch Logs" }}
                    </button>
                </div>

                <div class="bg-base-300 rounded-lg p-2 mt-3 max-h-96 overflow-y-auto font-mono text-xs">
                    {move || {
                        let lines = log_lines.get();
                        let filter = filter_text.get().to_lowercase();
                        if lines.is_empty() {
                            return view! { <p class="text-base-content/40 p-2">"No logs loaded. Click \"Fetch Logs\" to retrieve."</p> }.into_any();
                        }
                        let filtered: Vec<_> = lines.iter().filter(|l| {
                            filter.is_empty() || l.message.to_lowercase().contains(&filter)
                        }).collect();
                        view! {
                            <div class="flex flex-col">
                                {filtered.into_iter().map(|l| {
                                    let color = match l.level.as_deref() {
                                        Some("ERROR") | Some("error") => "text-error",
                                        Some("WARN") | Some("warn") => "text-warning",
                                        Some("DEBUG") | Some("debug") => "text-base-content/40",
                                        _ => "text-base-content/80",
                                    };
                                    view! {
                                        <div class={format!("py-0.5 border-b border-base-content/5 {color}")}>
                                            {l.timestamp.clone().map(|ts| view! {
                                                <span class="text-base-content/30 mr-2">{ts}</span>
                                            })}
                                            {l.level.clone().map(|lv| view! {
                                                <span class="mr-2 font-semibold">{lv}</span>
                                            })}
                                            <span>{l.message.clone()}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
}

// ── Network Tools ───────────────────────────────────────────────

#[component]
pub fn NetworkToolsCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (tool, set_tool) = signal(String::from("ping"));
    let (target, set_target) = signal(String::from("8.8.8.8"));
    let (running, set_running) = signal(false);
    let (result, set_result) = signal(Option::<crate::types::NetworkToolResult>::None);

    let do_run = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let t = tool.get_untracked();
        let tgt = target.get_untracked();
        let tgt_opt = if t == "speedtest" { None } else { Some(tgt) };
        set_running.set(true);
        set_result.set(None);
        leptos::task::spawn_local(async move {
            match api::run_network_tool(&token, &id, &t, tgt_opt.as_deref()).await {
                Ok(r) => set_result.set(Some(r)),
                Err(e) => set_result.set(Some(crate::types::NetworkToolResult {
                    tool: t,
                    output: format!("Error: {e}"),
                    success: false,
                })),
            }
            set_running.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h3 class="card-title text-base">"Network Tools"</h3>
                <div class="flex flex-wrap gap-2 items-end mt-2">
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Tool"</label>
                        <select class="select select-bordered select-sm"
                            on:change=move |ev| set_tool.set(event_target_value(&ev))
                        >
                            <option value="ping" selected=move || tool.get() == "ping">"Ping"</option>
                            <option value="traceroute" selected=move || tool.get() == "traceroute">"Traceroute"</option>
                            <option value="speedtest" selected=move || tool.get() == "speedtest">"Speed Test"</option>
                        </select>
                    </fieldset>
                    <fieldset class="fieldset"
                        style:display=move || if tool.get() == "speedtest" { "none" } else { "block" }
                    >
                        <label class="fieldset-label">"Target"</label>
                        <input
                            type="text" class="input input-bordered input-sm w-48"
                            prop:value=move || target.get()
                            on:input=move |ev| set_target.set(event_target_value(&ev))
                        />
                    </fieldset>
                    <button class="btn btn-ghost btn-sm" on:click=do_run
                        disabled=move || !is_online.get() || running.get() || !auth.has_role("admin")
                    >
                        {move || if running.get() { "Running…" } else { "Run" }}
                    </button>
                </div>

                {move || result.get().map(|r| {
                    let border_cls = if r.success { "border-success" } else { "border-error" };
                    view! {
                        <div class={format!("bg-base-300 rounded-lg p-3 mt-3 font-mono text-xs whitespace-pre-wrap border {border_cls} max-h-64 overflow-y-auto")}>
                            {r.output}
                        </div>
                    }
                })}
            </div>
        </div>
    }
}

// ── PCAP Capture ────────────────────────────────────────────────

#[component]
pub fn PcapCaptureCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (duration, set_duration) = signal(10u32);
    let (capturing, set_capturing) = signal(false);
    let (pcap_result, set_pcap_result) = signal(Option::<crate::types::PcapResponse>::None);
    let (pcap_msg, set_pcap_msg) = signal(Option::<(String, &'static str)>::None);

    let do_capture = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let dur = duration.get_untracked();
        set_capturing.set(true);
        set_pcap_msg.set(None);
        set_pcap_result.set(None);
        leptos::task::spawn_local(async move {
            match api::capture_pcap(&token, &id, dur).await {
                Ok(r) => {
                    set_pcap_result.set(Some(r));
                    set_pcap_msg.set(Some(("Capture complete".into(), "ok")));
                }
                Err(e) => set_pcap_msg.set(Some((format!("Capture failed: {e}"), "err"))),
            }
            set_capturing.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h3 class="card-title text-base">"Packet Capture (PCAP)"</h3>
                <p class="text-sm text-base-content/60">
                    "Capture bonding interface traffic for Wireshark analysis."
                </p>

                {move || pcap_msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                <div class="flex items-end gap-3 mt-2">
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Duration (seconds)"</label>
                        <select class="select select-bordered select-sm"
                            on:change=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<u32>() { set_duration.set(v); } }
                        >
                            <option value="5">"5s"</option>
                            <option value="10" selected=true>"10s"</option>
                            <option value="30">"30s"</option>
                            <option value="60">"60s"</option>
                        </select>
                    </fieldset>
                    <button class="btn btn-ghost btn-sm" on:click=do_capture
                        disabled=move || !is_online.get() || capturing.get() || !auth.has_role("admin")
                    >
                        {move || if capturing.get() { "Capturing…" } else { "Start Capture" }}
                    </button>
                </div>

                {move || pcap_result.get().map(|r| {
                    let url = r.download_url.clone();
                    view! {
                        <div class="bg-base-300 rounded-lg p-3 mt-3 flex items-center justify-between">
                            <div>
                                <div class="font-mono text-sm">"Capture ready"</div>
                                {r.file_size_bytes.map(|s| view! {
                                    <div class="text-xs text-base-content/40">{format_bytes(s)}</div>
                                })}
                            </div>
                            <a href={url} target="_blank" class="btn btn-primary btn-sm">"Download .pcap"</a>
                        </div>
                    }
                })}
            </div>
        </div>
    }
}

// ── Alerting Rules ──────────────────────────────────────────────

#[component]
pub fn AlertingRulesCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (rules, set_rules) = signal(Vec::<crate::types::AlertRule>::new());
    let (loading, set_loading) = signal(false);
    let (show_create, set_show_create) = signal(false);
    let (alert_msg, set_alert_msg) = signal(Option::<(String, &'static str)>::None);

    // New rule form
    let (new_name, set_new_name) = signal(String::new());
    let (new_metric, set_new_metric) = signal(String::from("aggregate_capacity_bps"));
    let (new_condition, set_new_condition) = signal(String::from("below"));
    let (new_threshold, set_new_threshold) = signal(String::from("5000000"));

    let auth_load = auth.clone();
    let load_rules = move || {
        let token = auth_load.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_loading.set(true);
        leptos::task::spawn_local(async move {
            if let Ok(r) = api::get_alert_rules(&token, &id).await {
                set_rules.set(r);
            }
            set_loading.set(false);
        });
    };

    // Load on mount
    {
        let load = load_rules;
        Effect::new(move || {
            let _online = is_online.get();
            load();
        });
    }

    let auth_create = auth.clone();
    let load_after_create = load_rules;
    let on_create = move |_: web_sys::MouseEvent| {
        let token = auth_create.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let name = new_name.get_untracked();
        let metric = new_metric.get_untracked();
        let condition = new_condition.get_untracked();
        let threshold: f64 = new_threshold.get_untracked().parse().unwrap_or(0.0);
        let rule = crate::types::AlertRule {
            id: None,
            name,
            metric,
            condition,
            threshold,
            enabled: true,
        };
        set_alert_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::set_alert_rule(&token, &id, &rule).await {
                Ok(()) => {
                    set_show_create.set(false);
                    set_new_name.set(String::new());
                    set_new_threshold.set("5000000".into());
                    set_alert_msg.set(Some(("Rule created".into(), "ok")));
                }
                Err(e) => set_alert_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
        });
        let reload = load_after_create;
        // Reload after short delay
        let cb = Closure::<dyn Fn()>::wrap(Box::new(move || {
            reload();
        }));
        let _ = web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                500,
            );
        cb.forget();
    };

    let load_after_delete = load_rules;
    let on_delete = move |rule_id: String| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        leptos::task::spawn_local(async move {
            let _ = api::delete_alert_rule(&token, &id, &rule_id).await;
        });
        let reload = load_after_delete;
        let cb = Closure::<dyn Fn()>::wrap(Box::new(move || {
            reload();
        }));
        let _ = web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                500,
            );
        cb.forget();
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"Alerting Rules"</h3>
                    <button class="btn btn-ghost btn-sm" on:click=move |_| set_show_create.set(!show_create.get_untracked()) disabled={
                    let auth = auth.clone();
                    move || !auth.has_role("admin")
                }>
                        {move || if show_create.get() { "Cancel" } else { "+ Add Rule" }}
                    </button>
                </div>

                {move || alert_msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                // Create form
                {move || show_create.get().then(|| view! {
                    <div class="bg-base-300 rounded-lg p-4 mt-2 flex flex-col gap-2">
                        <input type="text" class="input input-bordered input-sm w-full" placeholder="Rule name"
                            prop:value=move || new_name.get()
                            on:input=move |ev| set_new_name.set(event_target_value(&ev))
                        />
                        <div class="flex gap-2">
                            <select class="select select-bordered select-sm flex-1"
                                on:change=move |ev| set_new_metric.set(event_target_value(&ev))
                            >
                                <option value="aggregate_capacity_bps">"Aggregate Capacity (bps)"</option>
                                <option value="pre_fec_loss_pct">"Pre-FEC Loss (%)"</option>
                                <option value="post_fec_loss_pct">"Post-FEC Loss (%)"</option>
                                <option value="rtt_ms">"RTT (ms)"</option>
                                <option value="link_count">"Active Link Count"</option>
                            </select>
                            <select class="select select-bordered select-sm"
                                on:change=move |ev| set_new_condition.set(event_target_value(&ev))
                            >
                                <option value="below">"Below"</option>
                                <option value="above">"Above"</option>
                            </select>
                            <input type="number" class="input input-bordered input-sm w-32" placeholder="Threshold"
                                prop:value=move || new_threshold.get()
                                on:input=move |ev| set_new_threshold.set(event_target_value(&ev))
                            />
                        </div>
                        <button class="btn btn-primary btn-sm self-end" on:click=on_create>"Create Rule"</button>
                    </div>
                })}

                // Rules list
                {move || {
                    let auth = auth.clone();
                    let r = rules.get();
                    if loading.get() {
                        return view! { <p class="text-sm text-base-content/40 mt-2">"Loading rules…"</p> }.into_any();
                    }
                    if r.is_empty() {
                        return view! { <p class="text-sm text-base-content/40 mt-2">"No alerting rules configured."</p> }.into_any();
                    }
                    view! {
                        <div class="flex flex-col gap-2 mt-2">
                            {r.iter().map(|rule| {
                                let rule_id = rule.id.clone().unwrap_or_default();
                                let rule_id2 = rule_id.clone();
                                let auth = auth.clone();
                                view! {
                                    <div class="flex items-center justify-between bg-base-300 rounded-lg p-3">
                                        <div>
                                            <div class="font-medium text-sm">{rule.name.clone()}</div>
                                            <div class="text-xs text-base-content/60">
                                                {format!("{} {} {}", rule.metric, rule.condition, rule.threshold)}
                                            </div>
                                        </div>
                                        <div class="flex items-center gap-2">
                                            <span class=if rule.enabled { "badge badge-success badge-sm" } else { "badge badge-ghost badge-sm" }>
                                                {if rule.enabled { "Active" } else { "Disabled" }}
                                            </span>
                                            <button class="btn btn-ghost btn-xs text-error"
                                                on:click=move |_| on_delete(rule_id2.clone())
                                                disabled=move || !auth.has_role("admin")
                                            >
                                                "✕"
                                            </button>
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// LIVE SETTINGS (encoder controls)
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn LiveSettingsCard(
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
            fec: None,
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
                        disabled=move || applying.get() || !auth.has_role("operator")
                    >
                        {move || if applying.get() { "Applying…" } else { "Apply" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// TRANSPORT TUNING
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn TransportTuningCard(
    sender_id: Memo<String>,
    stream_state: ReadSignal<String>,
    sender_metrics: ReadSignal<Option<crate::types::TransportSenderMetrics>>,
    live_links: ReadSignal<Vec<LinkStats>>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (scheduler_mode, set_scheduler_mode) = signal(String::from("redundancy_enabled"));
    let (capacity_floor, set_capacity_floor) = signal(5000u32); // kbps
    let (fec_overhead, set_fec_overhead) = signal(20u32); // %
    let (fec_layer, set_fec_layer) = signal(String::from("rlnc"));
    let (blest_threshold, set_blest_threshold) = signal(50u32); // ms
    let (sbd_enabled, set_sbd_enabled) = signal(false);
    let (applying, set_applying) = signal(false);
    let (apply_msg, set_apply_msg) = signal(Option::<(String, &'static str)>::None);

    let do_apply = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_applying.set(true);
        set_apply_msg.set(None);

        let req = StreamConfigUpdateRequest {
            encoder: None,
            scheduler: Some(serde_json::json!({
                "critical_broadcast": scheduler_mode.get_untracked() == "critical_broadcast",
                "redundancy_enabled": scheduler_mode.get_untracked() == "redundancy_enabled",
                "capacity_floor_bps": capacity_floor.get_untracked() * 1000,
                "fec_overhead_percent": fec_overhead.get_untracked(),
            })),
            fec: Some(crate::types::FecConfigUpdate {
                layer: Some(fec_layer.get_untracked()),
                blest_threshold_ms: Some(blest_threshold.get_untracked()),
                shared_bottleneck_detection: Some(sbd_enabled.get_untracked()),
            }),
        };

        leptos::task::spawn_local(async move {
            match api::update_stream_config(&token, &id, &req).await {
                Ok(()) => set_apply_msg.set(Some(("Transport settings applied".into(), "ok"))),
                Err(e) => set_apply_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_applying.set(false);
        });
    };

    view! {
        <div
            class="card bg-base-200 border border-base-300 mt-4"
            style:display=move || {
                let st = stream_state.get();
                if st == "live" || st == "starting" { "block" } else { "none" }
            }
        >
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"Transport & Protocol Tuning"</h3>
                    <span class="badge badge-ghost badge-sm">"Hot Reconfig"</span>
                </div>

                {move || apply_msg.get().map(|(msg, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm mt-2",
                        _ => "alert alert-error text-sm mt-2",
                    };
                    view! { <div class={cls}>{msg}</div> }
                })}

                // ── Live FEC Overhead display ──
                {move || {
                    let sm = sender_metrics.get();
                    sm.as_ref().and_then(|m| m.fec_overhead_ratio).map(|ratio| {
                        let layer_str = sm.as_ref().and_then(|m| m.fec_layer.clone()).unwrap_or_else(|| "—".into());
                        view! {
                            <div class="bg-base-300 rounded-lg p-3 mt-2 flex items-center justify-between">
                                <div>
                                    <div class="text-xs text-base-content/40 uppercase">"Current FEC Overhead"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{:.1}%", ratio * 100.0)}</div>
                                </div>
                                <div>
                                    <div class="text-xs text-base-content/40 uppercase">"Active Layer"</div>
                                    <div class="font-mono text-sm">{layer_str}</div>
                                </div>
                            </div>
                        }
                    })
                }}

                <div class="mt-3 grid grid-cols-1 md:grid-cols-2 gap-4">
                    // Scheduler Mode
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Scheduler Mode"</label>
                        <select
                            class="select select-bordered select-sm w-full"
                            on:change=move |ev| set_scheduler_mode.set(event_target_value(&ev))
                        >
                            <option value="redundancy_enabled" selected=move || scheduler_mode.get() == "redundancy_enabled">"Redundancy (Lowest Latency)"</option>
                            <option value="critical_broadcast" selected=move || scheduler_mode.get() == "critical_broadcast">"Critical Broadcast (Max Reliability)"</option>
                            <option value="failover_only" selected=move || scheduler_mode.get() == "failover_only">"Failover Only (Cost Saving)"</option>
                        </select>
                        <p class="text-xs text-base-content/40 mt-1">
                            "Controls how traffic is distributed across available links."
                        </p>
                    </fieldset>

                    // Capacity Floor
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Capacity Floor (kbps)"</label>
                        <div class="flex items-center gap-3">
                            <input
                                type="range" class="range range-sm range-primary flex-1"
                                min="1000" max="20000" step="500"
                                prop:value=move || capacity_floor.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_capacity_floor.set(v);
                                    }
                                }
                            />
                            <input
                                type="number" class="input input-bordered input-sm w-24 font-mono text-right"
                                min="1000" max="20000" step="500"
                                prop:value=move || capacity_floor.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_capacity_floor.set(v.clamp(1000, 20000));
                                    }
                                }
                            />
                        </div>
                        <p class="text-xs text-base-content/40 mt-1">
                            "Minimum aggregate bandwidth required before failing over."
                        </p>
                    </fieldset>

                    // TAROT FEC Overhead
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"TAROT FEC Target Overhead (%)"</label>
                        <div class="flex items-center gap-3">
                            <input
                                type="range" class="range range-sm range-primary flex-1"
                                min="0" max="100" step="5"
                                prop:value=move || fec_overhead.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_fec_overhead.set(v);
                                    }
                                }
                            />
                            <input
                                type="number" class="input input-bordered input-sm w-24 font-mono text-right"
                                min="0" max="100" step="5"
                                prop:value=move || fec_overhead.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_fec_overhead.set(v.clamp(0, 100));
                                    }
                                }
                            />
                        </div>
                        <p class="text-xs text-base-content/40 mt-1">
                            "Target FEC overhead. Higher values increase reliability at the cost of bandwidth."
                        </p>
                    </fieldset>

                    // FEC Layer Toggle
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"FEC Layer"</label>
                        <select
                            class="select select-bordered select-sm w-full"
                            on:change=move |ev| set_fec_layer.set(event_target_value(&ev))
                        >
                            <option value="rlnc" selected=move || fec_layer.get() == "rlnc">"Layer 1 — Sliding-Window RLNC"</option>
                            <option value="raptorq" selected=move || fec_layer.get() == "raptorq">"Layer 1b — UEP / RaptorQ"</option>
                        </select>
                        <p class="text-xs text-base-content/40 mt-1">
                            "RLNC for low-latency streaming; RaptorQ for higher-loss environments."
                        </p>
                    </fieldset>

                    // BLEST Head-of-Line Blocking Threshold
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"BLEST HoL Threshold (ms)"</label>
                        <div class="flex items-center gap-3">
                            <input
                                type="range" class="range range-sm range-primary flex-1"
                                min="10" max="500" step="10"
                                prop:value=move || blest_threshold.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_blest_threshold.set(v);
                                    }
                                }
                            />
                            <input
                                type="number" class="input input-bordered input-sm w-24 font-mono text-right"
                                min="10" max="500" step="10"
                                prop:value=move || blest_threshold.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_blest_threshold.set(v.clamp(10, 500));
                                    }
                                }
                            />
                        </div>
                        <p class="text-xs text-base-content/40 mt-1">
                            "BLEST Head-of-Line blocking threshold. Prevents scheduling to slow links."
                        </p>
                    </fieldset>

                    // Shared Bottleneck Detection
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Shared Bottleneck Detection (RFC 8382)"</label>
                        <label class="flex items-center gap-2 cursor-pointer mt-1">
                            <input
                                type="checkbox"
                                class="toggle toggle-sm toggle-primary"
                                prop:checked=move || sbd_enabled.get()
                                on:change=move |_| set_sbd_enabled.set(!sbd_enabled.get_untracked())
                            />
                            <span class="text-sm">{move || if sbd_enabled.get() { "Enabled" } else { "Disabled" }}</span>
                        </label>
                        <p class="text-xs text-base-content/40 mt-1">
                            "Groups links sharing the same tower backhaul to avoid congestion."
                        </p>
                    </fieldset>
                </div>

                // ── Thompson Sampling Scores ──
                {move || {
                    let links = live_links.get();
                    let has_scores = links.iter().any(|l| l.thompson_score.is_some());
                    has_scores.then(|| {
                        view! {
                            <div class="mt-4">
                                <h4 class="text-sm font-semibold mb-2">"Thompson Sampling Scores"</h4>
                                <div class="grid grid-cols-2 md:grid-cols-4 gap-2">
                                    {links.iter().filter_map(|l| {
                                        l.thompson_score.map(|score| {
                                            let iface = l.interface.clone();
                                            let pct = (score * 100.0).min(100.0);
                                            view! {
                                                <div class="bg-base-300 rounded-lg p-2">
                                                    <div class="text-xs text-base-content/40 truncate">{iface}</div>
                                                    <div class="font-mono text-sm font-semibold">{format!("{:.1}%", pct)}</div>
                                                    <progress class="progress progress-primary w-full h-1" value=format!("{}", pct as u32) max="100"></progress>
                                                </div>
                                            }
                                        })
                                    }).collect::<Vec<_>>()}
                                </div>
                            </div>
                        }
                    })
                }}

                <div class="card-actions justify-end mt-3">
                    <button
                        class="btn btn-primary btn-sm"
                        on:click=do_apply
                        disabled=move || applying.get() || !auth.has_role("admin")
                    >
                        {move || if applying.get() { "Applying…" } else { "Apply Transport Settings" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// MULTI-DESTINATION ROUTING
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn MultiDestRoutingCard(
    sender_id: Memo<String>,
    stream_state: ReadSignal<String>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (destinations, set_destinations) = signal(Vec::<crate::types::DestinationSummary>::new());
    let (active_ids, set_active_ids) = signal(Vec::<String>::new());
    let (applying, set_applying) = signal(false);
    let (msg, set_msg) = signal(Option::<(String, &'static str)>::None);
    let (loaded, set_loaded) = signal(false);

    // Load destinations when stream goes live
    let auth_load = auth.clone();
    Effect::new(move || {
        let st = stream_state.get();
        if (st == "live" || st == "starting") && !loaded.get_untracked() {
            let token = auth_load.token.get_untracked().unwrap_or_default();
            set_loaded.set(true);
            leptos::task::spawn_local(async move {
                if let Ok(dests) = api::list_destinations(&token).await {
                    set_destinations.set(dests);
                }
            });
        }
    });

    let toggle_dest = move |dest_id: String| {
        let mut ids = active_ids.get_untracked();
        if ids.contains(&dest_id) {
            ids.retain(|id| id != &dest_id);
        } else {
            ids.push(dest_id);
        }
        let ids_clone = ids.clone();
        set_active_ids.set(ids);

        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_applying.set(true);
        set_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::set_stream_destinations(&token, &id, &ids_clone).await {
                Ok(()) => set_msg.set(Some(("Destinations updated".into(), "ok"))),
                Err(e) => set_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_applying.set(false);
        });
    };

    view! {
        <div
            class="card bg-base-200 border border-base-300 mt-4"
            style:display=move || {
                let st = stream_state.get();
                if st == "live" || st == "starting" { "block" } else { "none" }
            }
        >
            <div class="card-body">
                <h3 class="card-title text-base">"Multi-Destination Routing"</h3>

                {move || msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                <p class="text-sm text-base-content/60 mb-2">
                    "Fan-out the bonded stream to multiple destinations simultaneously."
                </p>

                {move || {
                    let auth = auth.clone();
                    let dests = destinations.get();
                    if dests.is_empty() {
                        return view! {
                            <p class="text-sm text-base-content/40">"No destinations configured. Add destinations first."</p>
                        }.into_any();
                    }
                    let active = active_ids.get();
                    view! {
                        <div class="flex flex-col gap-2">
                            {dests.iter().map(|d| {
                                let d_id = d.id.clone();
                                let is_active = active.contains(&d.id);
                                let auth = auth.clone();
                                view! {
                                    <label class="flex items-center gap-3 p-3 bg-base-300 rounded cursor-pointer hover:bg-base-content/10">
                                        <input
                                            type="checkbox"
                                            class="checkbox checkbox-sm checkbox-primary"
                                            prop:checked=is_active
                                            on:change=move |_| toggle_dest(d_id.clone())
                                            disabled=move || applying.get() || !auth.has_role("operator")
                                        />
                                        <div class="flex-1">
                                            <div class="font-medium text-sm">{d.name.clone()}</div>
                                            <div class="text-xs text-base-content/60 font-mono">{d.platform.clone()} " · " {d.url.clone()}</div>
                                        </div>
                                        {is_active.then(|| view! {
                                            <span class="badge badge-success badge-sm">"Active"</span>
                                        })}
                                    </label>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}

                {move || applying.get().then(|| view! {
                    <p class="text-sm text-base-content/40 mt-2 animate-pulse">"Updating…"</p>
                })}
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// RECEIVER JITTER BUFFER
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn JitterBufferCard(
    sender_id: Memo<String>,
    stream_state: ReadSignal<String>,
    receiver_metrics: ReadSignal<Option<crate::types::TransportReceiverMetrics>>,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (jb_mode, set_jb_mode) = signal(String::from("adaptive"));
    let (static_ms, set_static_ms) = signal(100u32);
    let (applying, set_applying) = signal(false);
    let (jb_msg, set_jb_msg) = signal(Option::<(String, &'static str)>::None);

    let do_apply = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let mode = jb_mode.get_untracked();
        let ms = if mode == "static" {
            Some(static_ms.get_untracked())
        } else {
            None
        };
        set_applying.set(true);
        set_jb_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::set_jitter_buffer(&token, &id, &mode, ms).await {
                Ok(()) => set_jb_msg.set(Some(("Jitter buffer updated".into(), "ok"))),
                Err(e) => set_jb_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_applying.set(false);
        });
    };

    view! {
        <div
            class="card bg-base-200 border border-base-300 mt-4"
            style:display=move || {
                let st = stream_state.get();
                if st == "live" || st == "starting" { "block" } else { "none" }
            }
        >
            <div class="card-body">
                <h3 class="card-title text-base">"Receiver Jitter Buffer"</h3>

                {move || jb_msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                // Current jitter buffer depth
                {move || {
                    receiver_metrics.get().map(|rm| {
                        view! {
                            <div class="bg-base-300 rounded-lg p-3 mt-2">
                                <div class="text-xs text-base-content/40 uppercase">"Current Depth"</div>
                                <div class="font-mono font-semibold text-lg">{format!("{} pkts", rm.jitter_buffer_depth)}</div>
                            </div>
                        }
                    })
                }}

                <div class="mt-3 grid grid-cols-1 md:grid-cols-2 gap-4">
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Mode"</label>
                        <select
                            class="select select-bordered select-sm w-full"
                            on:change=move |ev| set_jb_mode.set(event_target_value(&ev))
                        >
                            <option value="adaptive" selected=move || jb_mode.get() == "adaptive">"Adaptive (Recommended)"</option>
                            <option value="static" selected=move || jb_mode.get() == "static">"Static"</option>
                        </select>
                        <p class="text-xs text-base-content/40 mt-1">
                            "Adaptive mode auto-adjusts to network conditions."
                        </p>
                    </fieldset>

                    <fieldset class="fieldset"
                        style:display=move || if jb_mode.get() == "static" { "block" } else { "none" }
                    >
                        <label class="fieldset-label">"Static Buffer Size (ms)"</label>
                        <div class="flex items-center gap-3">
                            <input
                                type="range" class="range range-sm range-primary flex-1"
                                min="20" max="1000" step="10"
                                prop:value=move || static_ms.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_static_ms.set(v);
                                    }
                                }
                            />
                            <input
                                type="number" class="input input-bordered input-sm w-24 font-mono text-right"
                                min="20" max="1000" step="10"
                                prop:value=move || static_ms.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                        set_static_ms.set(v.clamp(20, 1000));
                                    }
                                }
                            />
                        </div>
                    </fieldset>
                </div>

                <div class="card-actions justify-end mt-3">
                    <button
                        class="btn btn-primary btn-sm"
                        on:click=do_apply
                        disabled=move || applying.get() || !auth.has_role("admin")
                    >
                        {move || if applying.get() { "Applying…" } else { "Apply" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// POWER CONTROLS
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn PowerControlsCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (power_loading, set_power_loading) = signal(Option::<String>::None);
    let (power_msg, set_power_msg) = signal(Option::<(String, &'static str)>::None);
    let (confirm_action, set_confirm_action) = signal(Option::<String>::None);

    let do_power = move |action: String| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_power_loading.set(Some(action.clone()));
        set_power_msg.set(None);
        set_confirm_action.set(None);
        leptos::task::spawn_local(async move {
            match api::power_command(&token, &id, &action).await {
                Ok(()) => {
                    let msg = match action.as_str() {
                        "reboot" => "Reboot command sent",
                        "shutdown" => "Shutdown command sent",
                        "restart_agent" => "Agent restart command sent",
                        _ => "Command sent",
                    };
                    set_power_msg.set(Some((msg.into(), "ok")));
                }
                Err(e) => set_power_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_power_loading.set(None);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h3 class="card-title text-base">"Power Controls"</h3>

                {move || power_msg.get().map(|(msg, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{msg}</div> }
                })}

                {move || confirm_action.get().map(|action| {
                    let action2 = action.clone();
                    let label = match action.as_str() {
                        "reboot" => "reboot the device",
                        "shutdown" => "shut down the device",
                        _ => "restart the agent",
                    };
                    view! {
                        <div class="alert alert-warning text-sm mt-2">
                            <span>{format!("Are you sure you want to {label}?")}</span>
                            <div class="flex gap-2">
                                <button class="btn btn-warning btn-xs" on:click=move |_| do_power(action2.clone())>"Yes"</button>
                                <button class="btn btn-ghost btn-xs" on:click=move |_| set_confirm_action.set(None)>"Cancel"</button>
                            </div>
                        </div>
                    }
                })}

                <div class="flex flex-wrap gap-2 mt-2">
                    <button
                        class="btn btn-ghost btn-sm"
                        on:click=move |_| do_power("restart_agent".into())
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || power_loading.get().is_some() || !auth.has_role("admin")
                        }
                    >
                        {move || if power_loading.get().as_deref() == Some("restart_agent") { "Restarting…" } else { "Restart Agent" }}
                    </button>
                    <button
                        class="btn btn-warning btn-sm"
                        on:click=move |_| set_confirm_action.set(Some("reboot".into()))
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || power_loading.get().is_some() || !auth.has_role("admin")
                        }
                    >
                        "Reboot Device"
                    </button>
                    <button
                        class="btn btn-error btn-sm"
                        on:click=move |_| set_confirm_action.set(Some("shutdown".into()))
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || power_loading.get().is_some() || !auth.has_role("admin")
                        }
                    >
                        "Shutdown"
                    </button>
                </div>
                <p class="text-xs text-base-content/40 mt-1">
                    "Remote power commands are sent to the agent. Reboot and shutdown require confirmation."
                </p>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// CONFIG EXPORT / IMPORT
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn ConfigManagementCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (config_msg, set_config_msg) = signal(Option::<(String, &'static str)>::None);
    let (exporting, set_exporting) = signal(false);
    let (importing, set_importing) = signal(false);
    let (import_text, set_import_text) = signal(String::new());
    let (show_import, set_show_import) = signal(false);

    let auth_export = auth.clone();
    let do_export = move |_: web_sys::MouseEvent| {
        let token = auth_export.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_exporting.set(true);
        set_config_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::export_config(&token, &id).await {
                Ok(config) => {
                    let json = serde_json::to_string_pretty(&config).unwrap_or_default();
                    // Copy to clipboard via JS
                    if let Some(window) = web_sys::window() {
                        let clipboard = window.navigator().clipboard();
                        let promise = clipboard.write_text(&json);
                        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    }
                    set_import_text.set(json);
                    set_config_msg.set(Some((
                        "Configuration exported and copied to clipboard".into(),
                        "ok",
                    )));
                }
                Err(e) => set_config_msg.set(Some((format!("Export failed: {e}"), "err"))),
            }
            set_exporting.set(false);
        });
    };

    let do_import = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        let json_text = import_text.get_untracked();
        set_importing.set(true);
        set_config_msg.set(None);
        leptos::task::spawn_local(async move {
            match serde_json::from_str::<serde_json::Value>(&json_text) {
                Ok(config) => match api::import_config(&token, &id, &config).await {
                    Ok(()) => {
                        set_config_msg
                            .set(Some(("Configuration imported successfully".into(), "ok")));
                        set_show_import.set(false);
                    }
                    Err(e) => set_config_msg.set(Some((format!("Import failed: {e}"), "err"))),
                },
                Err(e) => set_config_msg.set(Some((format!("Invalid JSON: {e}"), "err"))),
            }
            set_importing.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <h3 class="card-title text-base">"Configuration Profiles"</h3>

                {move || config_msg.get().map(|(msg, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{msg}</div> }
                })}

                <p class="text-sm text-base-content/60 mb-2">
                    "Export or import device configuration profiles (JSON)."
                </p>

                <div class="flex gap-2">
                    <button class="btn btn-ghost btn-sm" on:click=do_export
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || exporting.get() || !auth.has_role("admin")
                        }
                    >
                        {move || if exporting.get() { "Exporting…" } else { "Export Config" }}
                    </button>
                    <button class="btn btn-ghost btn-sm" on:click=move |_| set_show_import.update(|v| *v = !*v)
                        disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || !auth.has_role("admin")
                        }
                    >
                        "Import Config"
                    </button>
                </div>

                <div style:display=move || if show_import.get() { "block" } else { "none" } class="mt-3">
                    <fieldset class="fieldset">
                        <label class="fieldset-label">"Paste configuration JSON"</label>
                        <textarea
                            class="textarea textarea-bordered w-full h-32 font-mono text-xs"
                            placeholder=r#"{"version": 1, ...}"#
                            prop:value=move || import_text.get()
                            on:input=move |ev| set_import_text.set(event_target_value(&ev))
                        />
                    </fieldset>
                    <div class="flex justify-end mt-2">
                        <button class="btn btn-primary btn-sm" on:click=do_import
                            disabled=move || importing.get() || import_text.get().is_empty()
                        >
                            {move || if importing.get() { "Importing…" } else { "Apply Import" }}
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// TLS CERTIFICATE MANAGEMENT
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn TlsManagementCard(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    let (tls_status, set_tls_status) = signal(Option::<crate::types::TlsStatus>::None);
    let (loading, set_loading) = signal(false);
    let (renewing, set_renewing) = signal(false);
    let (tls_msg, set_tls_msg) = signal(Option::<(String, &'static str)>::None);

    let is_admin = {
        let auth = auth.clone();
        move || auth.has_role("admin")
    };

    let auth_fetch = auth.clone();
    let do_fetch = move |_: web_sys::MouseEvent| {
        let token = auth_fetch.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_loading.set(true);
        leptos::task::spawn_local(async move {
            match api::get_tls_status(&token, &id).await {
                Ok(s) => set_tls_status.set(Some(s)),
                Err(e) => set_tls_msg.set(Some((format!("Failed: {e}"), "err"))),
            }
            set_loading.set(false);
        });
    };

    let do_renew = move |_: web_sys::MouseEvent| {
        let token = auth.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_renewing.set(true);
        set_tls_msg.set(None);
        leptos::task::spawn_local(async move {
            match api::renew_tls_cert(&token, &id).await {
                Ok(()) => set_tls_msg.set(Some(("Certificate renewed successfully".into(), "ok"))),
                Err(e) => set_tls_msg.set(Some((format!("Renewal failed: {e}"), "err"))),
            }
            set_renewing.set(false);
        });
    };

    view! {
        <div class="card bg-base-200 border border-base-300">
            <div class="card-body">
                <div class="flex justify-between items-center">
                    <h3 class="card-title text-base">"TLS / HTTPS"</h3>
                    <button class="btn btn-ghost btn-sm" on:click=do_fetch
                        disabled=move || !is_online.get() || loading.get()
                    >
                        {move || if loading.get() { "Loading…" } else { "Check Status" }}
                    </button>
                </div>

                {move || tls_msg.get().map(|(m, kind)| {
                    let cls = match kind {
                        "ok" => "alert alert-success text-sm",
                        _ => "alert alert-error text-sm",
                    };
                    view! { <div class={cls}>{m}</div> }
                })}

                {move || tls_status.get().map(|s| {
                    view! {
                        <div class="grid grid-cols-2 md:grid-cols-4 gap-2 mt-2">
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"HTTPS"</div>
                                <div class=if s.enabled { "font-semibold text-sm text-success" } else { "font-semibold text-sm text-error" }>
                                    {if s.enabled { "Enabled" } else { "Disabled" }}
                                </div>
                            </div>
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Type"</div>
                                <div class="font-mono text-sm">{if s.self_signed { "Self-Signed" } else { "CA-Signed" }}</div>
                            </div>
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Subject"</div>
                                <div class="font-mono text-xs truncate">{s.cert_subject.clone().unwrap_or_else(|| "—".into())}</div>
                            </div>
                            <div class="bg-base-300 rounded-lg p-3">
                                <div class="text-xs text-base-content/40 uppercase">"Expires"</div>
                                <div class="font-mono text-xs">{s.expiry.clone().unwrap_or_else(|| "—".into())}</div>
                            </div>
                        </div>
                        {(is_admin)().then(|| view! {
                            <div class="card-actions justify-end mt-3">
                                <button class="btn btn-warning btn-sm" on:click=do_renew
                                    disabled=move || renewing.get()
                                >
                                    {move || if renewing.get() { "Renewing…" } else { "Renew Certificate" }}
                                </button>
                            </div>
                        })}
                    }
                })}

                {move || tls_status.get().is_none().then(|| view! {
                    <p class="text-sm text-base-content/40 mt-2">"Click \"Check Status\" to view certificate details."</p>
                })}
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════
