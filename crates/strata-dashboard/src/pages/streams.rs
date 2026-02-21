//! Streams list page.

use leptos::prelude::*;

use crate::AuthState;
use crate::api;
use crate::types::StreamSummary;

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
                                        <th>"Started"</th>
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
                                            view! {
                                                <tr>
                                                    <td class="font-mono text-xs">{stream.id.clone()}</td>
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
                                                    <td class="text-xs">{stream.started_at.clone().unwrap_or_else(|| "—".into())}</td>
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
