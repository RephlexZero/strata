//! Streams list page.

use leptos::prelude::*;

use crate::AuthState;
use crate::api;
use strata_protocol::api::StreamSummary;

/// Lists active and recent streams.
#[component]
pub fn StreamsPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (streams, set_streams) = signal(Vec::<StreamSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(true);

    let auth_load = auth.clone();
    Effect::new(move || {
        let token = auth_load.token.get();
        if let Some(token) = token {
            let token = token.clone();
            leptos::task::spawn_local(async move {
                match api::list_streams(&token).await {
                    Ok(data) => {
                        set_streams.set(data);
                        set_loading.set(false);
                    }
                    Err(e) => {
                        set_error.set(Some(e));
                        set_loading.set(false);
                    }
                }
            });
        }
    });

    view! {
        <div>
            <div class="flex justify-between items-center mb-6">
                <div>
                    <h2 class="text-2xl font-semibold">"Streams"</h2>
                    <p class="text-sm text-base-content/60 mt-1">"Active and recent broadcasts"</p>
                </div>
            </div>

            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm mb-4">{e}</div>
            })}

            {move || {
                if loading.get() {
                    view! { <p class="text-base-content/60">"Loading…"</p> }.into_any()
                } else if streams.get().is_empty() {
                    view! {
                        <div class="flex flex-col items-center justify-center py-16 text-center">
                            <div class="text-5xl mb-4">"📺"</div>
                            <h3 class="text-lg font-medium mb-2">"No streams yet"</h3>
                            <p class="text-sm text-base-content/60">"Start a stream from a sender's detail page to see it here."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="overflow-x-auto">
                            <table class="table table-sm">
                                <thead>
                                    <tr>
                                        <th>"Stream ID"</th>
                                        <th>"Sender"</th>
                                        <th>"State"</th>
                                        <th>"Reason"</th>
                                        <th>"Started"</th>
                                        <th>"Ended"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For
                                        each=move || streams.get()
                                        key=|s| s.id.clone()
                                        children=move |stream| {
                                            let (badge_cls, dot_cls) = match stream.state.as_str() {
                                                "live" => ("badge badge-error gap-1", "w-2 h-2 rounded-full bg-error animate-pulse-dot"),
                                                "starting" | "stopping" => ("badge badge-warning gap-1", "w-2 h-2 rounded-full bg-warning"),
                                                "failed" => ("badge badge-error gap-1", "w-2 h-2 rounded-full bg-error"),
                                                _ => ("badge badge-ghost gap-1", "w-2 h-2 rounded-full bg-base-content/30"),
                                            };
                                            // Reason column: end_reason slug → human text,
                                            // error detail as hover title (U2).
                                            let reason_view = match (stream.end_reason.as_deref(), stream.error_message.clone()) {
                                                (None, None) => view! { <span class="text-base-content/40">"—"</span> }.into_any(),
                                                (reason, detail) => {
                                                    let label = reason.map(crate::pages::end_reason_label).unwrap_or("ended").to_string();
                                                    let title = detail.clone().unwrap_or_default();
                                                    let is_bad = matches!(reason, Some("pipeline_crash") | Some("error") | Some("unobserved"));
                                                    let cls = if is_bad { "text-error" } else { "text-base-content/70" };
                                                    view! { <span class=cls title=title>{label}</span> }.into_any()
                                                }
                                            };
                                            let restart_marker = stream.restarted_from.clone().map(|prev| {
                                                let short: String = prev.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
                                                view! {
                                                    <div class="text-[10px] text-base-content/40" title=prev.clone()>
                                                        {format!("restart of …{short}")}
                                                    </div>
                                                }
                                            });
                                            view! {
                                                <tr>
                                                    <td class="font-mono text-xs">
                                                        {stream.id.clone()}
                                                        {restart_marker}
                                                    </td>
                                                    <td>
                                                        <a class="link link-primary" href=format!("/senders/{}", stream.sender_id)>
                                                            {stream.sender_id.clone()}
                                                        </a>
                                                    </td>
                                                    <td>
                                                        <span class=badge_cls>
                                                            <span class=dot_cls></span>
                                                            {stream.state.clone().to_uppercase()}
                                                        </span>
                                                    </td>
                                                    <td class="text-xs">{reason_view}</td>
                                                    <td class="text-xs">{crate::pages::format_local_time(stream.started_at.map(|t| t.to_rfc3339()).as_deref())}</td>
                                                    <td class="text-xs">{crate::pages::format_local_time(stream.ended_at.map(|t| t.to_rfc3339()).as_deref())}</td>
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
        </div>
    }
}
