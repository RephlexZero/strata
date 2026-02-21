use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::AuthState;
use crate::api;
use crate::types::{
    FileEntry, LinkStats, MediaInput, NetworkInterface, SenderDetail, SourceSwitchRequest,
    TestRunResponse,
};

use super::cards::{
    AlertingRulesCard, BandwidthGraph, ConfigManagementCard, JitterBufferCard, LiveLogViewerCard,
    LiveSettingsCard, MultiDestRoutingCard, NetworkToolsCard, OtaUpdatesCard, PcapCaptureCard,
    PowerControlsCard, TlsManagementCard, TransportTuningCard,
};
use super::helpers::{format_bps, format_bytes};

#[component]
pub fn DestinationModal(
    show: ReadSignal<bool>,
    set_show: WriteSignal<bool>,
    destinations: ReadSignal<Vec<crate::types::DestinationSummary>>,
    selected_dest: ReadSignal<Option<String>>,
    set_selected_dest: WriteSignal<Option<String>>,
    dests_loading: ReadSignal<bool>,
    on_confirm: impl Fn(web_sys::MouseEvent) + 'static + Copy + Send,
) -> impl IntoView {
    let auth = expect_context::<AuthState>();

    view! {
        {move || show.get().then(|| {
            let auth = auth.clone();
            view! {
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
                        <button class="btn btn-primary" on:click=on_confirm disabled=move || dests_loading.get() || !auth.has_role("operator")>"Go Live"</button>
                    </div>
                </div>
                <div class="modal-backdrop" on:click=move |_| set_show.set(false)><button>"close"</button></div>
            </div>
        }})}
    }
}

// ═══════════════════════════════════════════════════════════════════
// STREAM TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn StreamTab(
    stream_state: ReadSignal<String>,
    live_links: ReadSignal<Vec<LinkStats>>,
    live_bitrate: ReadSignal<u32>,
    stats_history: ReadSignal<std::collections::VecDeque<(f64, Vec<LinkStats>)>>,
    sender_metrics: ReadSignal<Option<crate::types::TransportSenderMetrics>>,
    receiver_metrics: ReadSignal<Option<crate::types::TransportReceiverMetrics>>,
    sender_id: Memo<String>,
    stream_detail: ReadSignal<Option<crate::types::StreamDetail>>,
) -> impl IntoView {
    view! {
        <div>
            // Glass-to-Glass Health
            <div class="card bg-base-200 border border-base-300 mb-4">
                <div class="card-body">
                    <h3 class="card-title text-base">"Glass-to-Glass Health"</h3>
                    {move || {
                        let st = stream_state.get();
                        if st != "live" && st != "starting" {
                            return view! {
                                <p class="text-sm text-base-content/40">"Start a stream to see health metrics"</p>
                            }.into_any();
                        }

                        let sm = sender_metrics.get();
                        let rm = receiver_metrics.get();

                        if sm.is_none() && rm.is_none() {
                            return view! {
                                <p class="text-sm text-base-content/40">"Waiting for telemetry…"</p>
                            }.into_any();
                        }

                        let rtt_ms = sm.as_ref().map(|m| m.last_rtt_us as f64 / 1000.0).unwrap_or(0.0);
                        let jitter_depth = rm.as_ref().map(|m| m.jitter_buffer_depth).unwrap_or(0);

                        let pre_fec_loss = if let Some(m) = sm.as_ref() {
                            if m.packets_sent > 0 {
                                (m.retransmissions as f64 / m.packets_sent as f64) * 100.0
                            } else { 0.0 }
                        } else { 0.0 };

                        let post_fec_loss = if let (Some(s), Some(r)) = (sm.as_ref(), rm.as_ref()) {
                            if s.packets_sent > 0 {
                                let lost = s.packets_sent.saturating_sub(r.packets_delivered);
                                (lost as f64 / s.packets_sent as f64) * 100.0
                            } else { 0.0 }
                        } else { 0.0 };

                        view! {
                            <div class="grid grid-cols-2 md:grid-cols-4 gap-3 mt-2">
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Est. Latency"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{:.1} ms", rtt_ms)}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Pre-FEC Loss"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{:.2}%", pre_fec_loss)}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Post-FEC Loss"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{:.2}%", post_fec_loss)}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Jitter Buffer"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{} pkts", jitter_depth)}</div>
                                </div>
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>

            // Stream Metadata
            <div class="card bg-base-200 border border-base-300 mb-4">
                <div class="card-body">
                    <h3 class="card-title text-base">"Stream Metadata"</h3>
                    {move || {
                        let st = stream_state.get();
                        if st != "live" && st != "starting" {
                            return view! {
                                <p class="text-sm text-base-content/40">"Start a stream to see metadata"</p>
                            }.into_any();
                        }

                        let detail = stream_detail.get();
                        if detail.is_none() {
                            return view! {
                                <p class="text-sm text-base-content/40">"Loading metadata…"</p>
                            }.into_any();
                        }

                        let detail = detail.unwrap();
                        let config_json = detail.config_json.unwrap_or_else(|| "{}".to_string());
                        let config: serde_json::Value = serde_json::from_str(&config_json).unwrap_or_default();
                        let request = config.get("request").unwrap_or(&serde_json::Value::Null);
                        let source = request.get("source").unwrap_or(&serde_json::Value::Null);
                        let encoder = request.get("encoder").unwrap_or(&serde_json::Value::Null);

                        let resolution = source.get("resolution").and_then(|v| v.as_str()).unwrap_or("Unknown");
                        let framerate = source.get("framerate").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_else(|| "Unknown".to_string());
                        let codec = encoder.get("codec").and_then(|v| v.as_str()).unwrap_or("Unknown");
                        let tune = encoder.get("tune").and_then(|v| v.as_str()).unwrap_or("Unknown");

                        view! {
                            <div class="grid grid-cols-2 md:grid-cols-4 gap-3 mt-2">
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Resolution"</div>
                                    <div class="font-mono font-semibold text-lg">{resolution.to_string()}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Framerate"</div>
                                    <div class="font-mono font-semibold text-lg">{format!("{} fps", framerate)}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Codec"</div>
                                    <div class="font-mono font-semibold text-lg">{codec.to_string().to_uppercase()}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-base-content/40 uppercase text-xs">"Tune"</div>
                                    <div class="font-mono font-semibold text-lg">{tune.to_string()}</div>
                                </div>
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>

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
                            <div class="mb-4">
                                <BandwidthGraph history=stats_history />
                            </div>
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
                                                <div class="grid grid-cols-2 gap-2 text-xs mt-2 pt-2 border-t border-base-content/10">
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"BBRv3 BtlBw"</div>
                                                        <div class="font-mono font-semibold">{link.btlbw_bps.map(format_bps).unwrap_or_else(|| "—".into())}</div>
                                                    </div>
                                                    <div>
                                                        <div class="text-base-content/40 uppercase">"BBRv3 RTprop"</div>
                                                        <div class="font-mono font-semibold">{link.rtprop_ms.map(|v| format!("{v:.1} ms")).unwrap_or_else(|| "—".into())}</div>
                                                    </div>
                                                </div>
                                                {(link.link_kind.as_deref() == Some("cellular")).then(|| view! {
                                                    <div class="grid grid-cols-4 gap-2 text-xs mt-2 pt-2 border-t border-base-content/10">
                                                        <div>
                                                            <div class="text-base-content/40 uppercase">"RSRP"</div>
                                                            <div class="font-mono font-semibold">{link.rsrp.map(|v| format!("{v:.1} dBm")).unwrap_or_else(|| "—".into())}</div>
                                                        </div>
                                                        <div>
                                                            <div class="text-base-content/40 uppercase">"RSRQ"</div>
                                                            <div class="font-mono font-semibold">{link.rsrq.map(|v| format!("{v:.1} dB")).unwrap_or_else(|| "—".into())}</div>
                                                        </div>
                                                        <div>
                                                            <div class="text-base-content/40 uppercase">"SINR"</div>
                                                            <div class="font-mono font-semibold">{link.sinr.map(|v| format!("{v:.1} dB")).unwrap_or_else(|| "—".into())}</div>
                                                        </div>
                                                        <div>
                                                            <div class="text-base-content/40 uppercase">"CQI"</div>
                                                            <div class="font-mono font-semibold">{link.cqi.map(|v| format!("{v}")).unwrap_or_else(|| "—".into())}</div>
                                                        </div>
                                                    </div>
                                                })}
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

            // Media Awareness Stats — NAL unit classification counters
            <div
                class="card bg-base-200 border border-base-300 mt-4"
                style:display=move || {
                    let st = stream_state.get();
                    if st == "live" || st == "starting" { "block" } else { "none" }
                }
            >
                <div class="card-body">
                    <h3 class="card-title text-base">"Media Awareness"</h3>
                    {move || {
                        let sm = sender_metrics.get();
                        let has_nal = sm.as_ref().map(|m| m.nal_critical_sent.is_some()).unwrap_or(false);
                        if !has_nal {
                            return view! {
                                <p class="text-sm text-base-content/40">"Waiting for NAL unit telemetry…"</p>
                            }.into_any();
                        }
                        let m = sm.unwrap();
                        let critical = m.nal_critical_sent.unwrap_or(0);
                        let reference = m.nal_reference_sent.unwrap_or(0);
                        let standard = m.nal_standard_sent.unwrap_or(0);
                        let disposable_sent = m.nal_disposable_sent.unwrap_or(0);
                        let disposable_dropped = m.nal_disposable_dropped.unwrap_or(0);
                        let total = critical + reference + standard + disposable_sent + disposable_dropped;

                        view! {
                            <div class="grid grid-cols-2 md:grid-cols-5 gap-2 mt-2">
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-xs text-base-content/40 uppercase">"Critical (IDR/SPS/PPS)"</div>
                                    <div class="font-mono font-semibold">{critical.to_string()}</div>
                                    {(total > 0).then(|| {
                                        let pct = (critical as f64 / total as f64) * 100.0;
                                        view! { <div class="text-xs text-base-content/40">{format!("{:.1}%", pct)}</div> }
                                    })}
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-xs text-base-content/40 uppercase">"Reference"</div>
                                    <div class="font-mono font-semibold">{reference.to_string()}</div>
                                    {(total > 0).then(|| {
                                        let pct = (reference as f64 / total as f64) * 100.0;
                                        view! { <div class="text-xs text-base-content/40">{format!("{:.1}%", pct)}</div> }
                                    })}
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-xs text-base-content/40 uppercase">"Standard"</div>
                                    <div class="font-mono font-semibold">{standard.to_string()}</div>
                                    {(total > 0).then(|| {
                                        let pct = (standard as f64 / total as f64) * 100.0;
                                        view! { <div class="text-xs text-base-content/40">{format!("{:.1}%", pct)}</div> }
                                    })}
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-xs text-base-content/40 uppercase">"Disposable Sent"</div>
                                    <div class="font-mono font-semibold">{disposable_sent.to_string()}</div>
                                </div>
                                <div class="bg-base-300 rounded-lg p-3">
                                    <div class="text-xs text-base-content/40 uppercase">"Disposable Dropped"</div>
                                    <div class="font-mono font-semibold text-warning">{disposable_dropped.to_string()}</div>
                                </div>
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>

            // Transport Tuning controls
            <TransportTuningCard sender_id=sender_id stream_state=stream_state sender_metrics=sender_metrics live_links=live_links />

            // Multi-Destination Routing
            <MultiDestRoutingCard sender_id=sender_id stream_state=stream_state />

            // Receiver Jitter Buffer
            <JitterBufferCard sender_id=sender_id stream_state=stream_state receiver_metrics=receiver_metrics />
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// SOURCE TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn SourceTab(
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

    // File browser modal
    let (show_file_browser, set_show_file_browser) = signal(false);
    let (browser_path, set_browser_path) = signal(String::new());
    let (browser_entries, set_browser_entries) = signal(Vec::<FileEntry>::new());
    let (browser_loading, set_browser_loading) = signal(false);
    let (browser_error, set_browser_error) = signal(Option::<String>::None);

    // Load directory when browser opens or user navigates
    let auth_browse = auth.clone();
    let browse_dir = move |path: Option<String>| {
        let token = auth_browse.token.get_untracked().unwrap_or_default();
        let id = sender_id.get_untracked();
        set_browser_loading.set(true);
        set_browser_error.set(None);
        let path_clone = path.clone();
        leptos::task::spawn_local(async move {
            match api::list_files(&token, &id, path_clone.as_deref()).await {
                Ok(resp) => {
                    set_browser_path.set(resp.path);
                    set_browser_entries.set(resp.entries);
                    if let Some(err) = resp.error {
                        set_browser_error.set(Some(err));
                    }
                }
                Err(e) => set_browser_error.set(Some(e)),
            }
            set_browser_loading.set(false);
        });
    };
    let browse_dir2 = browse_dir;

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
                                <div class="flex gap-2">
                                    <input
                                        type="text"
                                        class="input input-bordered flex-1"
                                        placeholder="file:///media/video.mp4 or https://example.com/stream.mp4"
                                        prop:value=move || source_uri.get()
                                        on:input=move |ev| set_source_uri.set(event_target_value(&ev))
                                    />
                                    <button
                                        class="btn btn-outline btn-sm self-end mb-0.5"
                                        type="button"
                                        on:click=move |_| {
                                            set_show_file_browser.set(true);
                                            browse_dir2(None);
                                        }
                                    >
                                        "Browse…"
                                    </button>
                                </div>
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
                    disabled=move || switching.get() || !is_live.get() || !auth.has_role("operator")
                >
                    {move || if switching.get() { "Switching…" } else { "Switch Source" }}
                </button>
            </div>

            // File browser modal
            <FileBrowserModal
                show=show_file_browser
                set_show=set_show_file_browser
                path=browser_path
                entries=browser_entries
                loading=browser_loading
                error=browser_error
                on_navigate=move |p: String| browse_dir(Some(p))
                on_select=move |p: String| {
                    set_source_uri.set(format!("file://{p}"));
                    set_source_type.set("uri".into());
                    set_show_file_browser.set(false);
                }
            />
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// FILE BROWSER MODAL
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn FileBrowserModal(
    show: ReadSignal<bool>,
    set_show: WriteSignal<bool>,
    path: ReadSignal<String>,
    entries: ReadSignal<Vec<FileEntry>>,
    loading: ReadSignal<bool>,
    error: ReadSignal<Option<String>>,
    on_navigate: impl Fn(String) + 'static + Clone + Send + Sync,
    on_select: impl Fn(String) + 'static + Clone + Send + Sync,
) -> impl IntoView {
    let on_navigate2 = on_navigate.clone();

    // Navigate up one directory level
    let go_up = move |_: web_sys::MouseEvent| {
        let p = path.get_untracked();
        if let Some(parent) = std::path::Path::new(&p).parent() {
            on_navigate2(parent.to_string_lossy().into_owned());
        }
    };

    view! {
        // Backdrop
        <div
            class="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
            style:display=move || if show.get() { "flex" } else { "none" }
            on:click=move |ev| {
                // Close on backdrop click (not when clicking modal content)
                if let Some(target) = ev.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_list().contains("fixed")
                {
                    set_show.set(false);
                }
            }
        >
            <div class="card bg-base-100 shadow-xl w-full max-w-lg mx-4">
                <div class="card-body p-0">
                    // Header
                    <div class="flex items-center gap-2 p-4 border-b border-base-300">
                        <button class="btn btn-ghost btn-sm btn-square" on:click=go_up title="Up">
                            "\u{2191}"
                        </button>
                        <span class="flex-1 font-mono text-sm truncate min-w-0">
                            {move || if path.get().is_empty() { "/".to_string() } else { path.get() }}
                        </span>
                        <button class="btn btn-ghost btn-sm btn-square" on:click=move |_| set_show.set(false)>
                            "\u{2715}"
                        </button>
                    </div>

                    // Body
                    <div class="overflow-y-auto max-h-80 p-2">
                        {move || loading.get().then(||
                            view! { <div class="flex justify-center py-8"><span class="loading loading-spinner"/></div> }
                        )}
                        {move || error.get().map(|e|
                            view! { <div class="alert alert-error text-sm m-2">{e}</div> }
                        )}
                        {move || {
                            let nav = on_navigate.clone();
                            let sel = on_select.clone();
                            let items = entries.get();
                            if !loading.get() && items.is_empty() && error.get().is_none() {
                                return view! { <p class="text-sm text-base-content/40 p-4 text-center">"Directory is empty"</p> }.into_any();
                            }
                            view! {
                                <div class="flex flex-col">
                                    {items.into_iter().map(|entry| {
                                        let nav = nav.clone();
                                        let sel = sel.clone();
                                        let p = entry.path.clone();
                                        let p2 = entry.path.clone();
                                        let is_dir = entry.is_dir;
                                        let size_str = entry.size.map(|s| {
                                            if s >= 1_073_741_824 { format!("{:.1} GB", s as f64 / 1_073_741_824.0) }
                                            else if s >= 1_048_576 { format!("{:.1} MB", s as f64 / 1_048_576.0) }
                                            else if s >= 1_024 { format!("{:.1} KB", s as f64 / 1_024.0) }
                                            else { format!("{s} B") }
                                        });
                                        view! {
                                            <button
                                                class="flex items-center gap-3 px-3 py-2 hover:bg-base-200 rounded text-left w-full"
                                                on:click=move |_| {
                                                    if is_dir {
                                                        nav(p.clone());
                                                    } else {
                                                        sel(p2.clone());
                                                    }
                                                }
                                            >
                                                <span class="text-lg shrink-0">
                                                    {if is_dir { "\u{1F4C1}" } else { "\u{1F4C4}" }}
                                                </span>
                                                <span class="flex-1 font-mono text-sm truncate">{entry.name}</span>
                                                {size_str.map(|s| view! {
                                                    <span class="text-xs text-base-content/40 shrink-0">{s}</span>
                                                })}
                                            </button>
                                        }
                                    }).collect::<Vec<_>>()}
                                </div>
                            }.into_any()
                        }}
                    </div>

                    // Footer
                    <div class="flex justify-end gap-2 p-3 border-t border-base-300">
                        <button class="btn btn-ghost btn-sm" on:click=move |_| set_show.set(false)>
                            "Cancel"
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// NETWORK TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn NetworkTab(
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
                    disabled={
                        let auth = auth.clone();
                        move || !is_online.get() || !auth.has_role("admin")
                    }
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
                            if let Some(b) = &iface.band { meta.push(format!("Band {b}")); }
                            if let Some(cid) = &iface.cell_id { meta.push(format!("Cell {cid}")); }
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

                            let is_cellular = iface.iface_type == "cellular";
                            let current_band = iface.band.clone();
                            let current_priority = iface.priority;
                            let name_lock = iface.name.clone();
                            let auth_lock = auth.clone();
                            let lock_band = move |ev: web_sys::Event| {
                                let sid = sender_id.get_untracked();
                                let iface_name = name_lock.clone();
                                let token = auth_lock.token.get_untracked().unwrap_or_default();
                                let val = event_target_value(&ev);
                                let band = if val.is_empty() || val == "auto" { None } else { Some(val) };
                                set_iface_loading.set(Some(iface_name.clone()));
                                leptos::task::spawn_local(async move {
                                    if let Err(e) = api::lock_band(&token, &sid, &iface_name, band).await {
                                        set_error.set(Some(e));
                                    }
                                    set_iface_loading.set(None);
                                });
                            };

                            let name_prio = iface.name.clone();
                            let auth_prio = auth.clone();
                            let set_priority = move |ev: web_sys::Event| {
                                let sid = sender_id.get_untracked();
                                let iface_name = name_prio.clone();
                                let token = auth_prio.token.get_untracked().unwrap_or_default();
                                let val = event_target_value(&ev);
                                if let Ok(prio) = val.parse::<u32>() {
                                    set_iface_loading.set(Some(iface_name.clone()));
                                    leptos::task::spawn_local(async move {
                                        if let Err(e) = api::set_priority(&token, &sid, &iface_name, prio).await {
                                            set_error.set(Some(e));
                                        }
                                        set_iface_loading.set(None);
                                    });
                                }
                            };

                            let current_apn = iface.apn.clone();
                            let current_roaming = iface.roaming;
                            let name_apn = iface.name.clone();
                            let auth_apn = auth.clone();
                            let set_apn = move |ev: web_sys::Event| {
                                let sid = sender_id.get_untracked();
                                let iface_name = name_apn.clone();
                                let token = auth_apn.token.get_untracked().unwrap_or_default();
                                let val = event_target_value(&ev);
                                let apn = if val.is_empty() { None } else { Some(val) };
                                set_iface_loading.set(Some(iface_name.clone()));
                                leptos::task::spawn_local(async move {
                                    if let Err(e) = api::set_apn(&token, &sid, &iface_name, apn, None, Some(current_roaming)).await {
                                        set_error.set(Some(e));
                                    }
                                    set_iface_loading.set(None);
                                });
                            };

                            let name_roaming = iface.name.clone();
                            let auth_roaming = auth.clone();
                            let current_apn_roaming = iface.apn.clone();
                            let toggle_roaming = move |ev: web_sys::Event| {
                                let sid = sender_id.get_untracked();
                                let iface_name = name_roaming.clone();
                                let token = auth_roaming.token.get_untracked().unwrap_or_default();
                                let target = ev.target().unwrap().unchecked_into::<web_sys::HtmlInputElement>();
                                let roaming = target.checked();
                                set_iface_loading.set(Some(iface_name.clone()));
                                let apn = current_apn_roaming.clone();
                                leptos::task::spawn_local(async move {
                                    if let Err(e) = api::set_apn(&token, &sid, &iface_name, apn, None, Some(roaming)).await {
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
                                    "flex flex-col p-3 bg-base-200 rounded-lg border border-base-300 gap-3"
                                } else {
                                    "flex flex-col p-3 bg-base-200 rounded-lg border border-base-300 opacity-50 gap-3"
                                }>
                                    <div class="flex items-center justify-between">
                                        <div class="flex items-center gap-3">
                                            <input
                                                type="checkbox"
                                                class=move || if is_loading() { "toggle toggle-success toggle-sm animate-pulse" } else { "toggle toggle-success toggle-sm" }
                                                checked=enabled
                                                on:change=toggle
                                                disabled={
                                                    let auth = auth.clone();
                                                    move || is_loading2() || !is_online.get() || !auth.has_role("admin")
                                                }
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
                                    {is_cellular.then(|| {
                                        let bands = ["2", "4", "5", "12", "13", "14", "25", "26", "41", "66", "71"];
                                        let cap_mb = iface.data_cap_mb;
                                        let used_mb = iface.data_used_mb;
                                        let auth1 = auth.clone();
                                        let auth2 = auth.clone();
                                        let auth3 = auth.clone();
                                        let auth4 = auth.clone();
                                        view! {
                                            <div class="flex flex-col gap-2 border-t border-base-content/10 pt-2 mt-2">
                                                <div class="flex items-center gap-4 text-xs">
                                                    <div class="flex items-center gap-2">
                                                        <span class="text-base-content/60">"Band Lock:"</span>
                                                        <select
                                                            class="select select-bordered select-xs w-32"
                                                            on:change=lock_band
                                                            disabled=move || !is_online.get() || !auth1.has_role("admin")
                                                        >
                                                            <option value="auto" selected=current_band.is_none()>"Auto"</option>
                                                            {bands.into_iter().map(|b| {
                                                                let is_selected = current_band.as_deref() == Some(b);
                                                                view! {
                                                                    <option value=b selected=is_selected>{format!("Band {b}")}</option>
                                                                }
                                                            }).collect::<Vec<_>>()}
                                                        </select>
                                                    </div>
                                                    <div class="flex items-center gap-2">
                                                        <span class="text-base-content/60">"Priority:"</span>
                                                        <select
                                                            class="select select-bordered select-xs w-32"
                                                            on:change=set_priority
                                                            disabled=move || !is_online.get() || !auth2.has_role("admin")
                                                        >
                                                            <option value="1" selected=current_priority == 1>"Primary (1)"</option>
                                                            <option value="2" selected=current_priority == 2>"Secondary (2)"</option>
                                                            <option value="3" selected=current_priority == 3>"Backup (3)"</option>
                                                            <option value="100" selected=current_priority == 100>"Standby (100)"</option>
                                                        </select>
                                                    </div>
                                                </div>
                                                <div class="flex items-center gap-4 text-xs">
                                                    <div class="flex items-center gap-2">
                                                        <span class="text-base-content/60">"APN:"</span>
                                                        <input
                                                            type="text"
                                                            class="input input-bordered input-xs w-32"
                                                            placeholder="auto"
                                                            prop:value=current_apn.clone().unwrap_or_default()
                                                            on:change=set_apn
                                                            disabled=move || !is_online.get() || !auth3.has_role("admin")
                                                        />
                                                    </div>
                                                    <div class="flex items-center gap-2">
                                                        <span class="text-base-content/60">"Roaming:"</span>
                                                        <input
                                                            type="checkbox"
                                                            class="toggle toggle-xs"
                                                            checked=current_roaming
                                                            on:change=toggle_roaming
                                                            disabled=move || !is_online.get() || !auth4.has_role("admin")
                                                        />
                                                    </div>
                                                </div>
                                                {cap_mb.map(|cap| {
                                                    let used = used_mb.unwrap_or(0);
                                                    let pct = (used as f64 / cap as f64 * 100.0).min(100.0);
                                                    let progress_cls = if pct > 90.0 {
                                                        "progress progress-error w-full"
                                                    } else if pct > 75.0 {
                                                        "progress progress-warning w-full"
                                                    } else {
                                                        "progress progress-primary w-full"
                                                    };
                                                    view! {
                                                        <div class="flex flex-col gap-1 text-xs">
                                                            <div class="flex justify-between text-base-content/60">
                                                                <span>"Data Usage"</span>
                                                                <span>{format!("{:.1} GB / {:.1} GB", used as f64 / 1000.0, cap as f64 / 1000.0)}</span>
                                                            </div>
                                                            <progress class=progress_cls value=pct max="100"></progress>
                                                        </div>
                                                    }
                                                })}
                                            </div>
                                        }
                                    })}
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
// DIAGNOSTICS TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn DiagnosticsTab(sender_id: Memo<String>, is_online: Memo<bool>) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-4">
            <OtaUpdatesCard sender_id=sender_id is_online=is_online />
            <LiveLogViewerCard sender_id=sender_id is_online=is_online />
            <NetworkToolsCard sender_id=sender_id is_online=is_online />
            <PcapCaptureCard sender_id=sender_id is_online=is_online />
            <AlertingRulesCard sender_id=sender_id is_online=is_online />
        </div>
    }
}

// ═══════════════════════════════════════════════════════════════════
// SETTINGS TAB
// ═══════════════════════════════════════════════════════════════════

#[component]
pub fn SettingsTab(
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
    let auth = expect_context::<AuthState>();

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
                                disabled={
                                    let auth = auth.clone();
                                    move || !is_online.get() || !auth.has_role("admin")
                                }
                                on:input=move |ev| set_receiver_input.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <button class="btn btn-primary" on:click=save_config disabled={
                            let auth = auth.clone();
                            move || !is_online.get() || !auth.has_role("admin")
                        }>
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

            // ── Power Controls ──
            <PowerControlsCard sender_id=sender_id is_online=is_online />

            // ── Config Export/Import ──
            <ConfigManagementCard sender_id=sender_id is_online=is_online />

            // ── TLS Certificate Management ──
            <TlsManagementCard sender_id=sender_id is_online=is_online />

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
                            let auth = auth.clone();
                            let enrolled = is_enrolled.get();
                            if !enrolled && unenroll_token.get().is_none() {
                                view! {
                                    <button class="btn btn-disabled" disabled=true>"Not Enrolled"</button>
                                }.into_any()
                            } else if show_unenroll_confirm.get() {
                                let auth = auth.clone();
                                view! {
                                    <div class="flex gap-2">
                                        <button class="btn btn-error" on:click=do_unenroll disabled=move || action_loading.get() || !auth.has_role("admin")>
                                            "Confirm"
                                        </button>
                                        <button class="btn btn-ghost" on:click=move |_| set_show_unenroll_confirm.set(false)>
                                            "Cancel"
                                        </button>
                                    </div>
                                }.into_any()
                            } else {
                                let auth = auth.clone();
                                view! {
                                    <button class="btn btn-error" on:click=move |_| set_show_unenroll_confirm.set(true) disabled=move || action_loading.get() || !auth.has_role("admin")>
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
