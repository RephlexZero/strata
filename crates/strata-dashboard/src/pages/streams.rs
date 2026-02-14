//! Streams list page.

use leptos::prelude::*;

use crate::api;
use crate::types::StreamSummary;
use crate::AuthState;

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
            <div class="page-header">
                <div>
                    <h2>"Streams"</h2>
                    <p class="subtitle">"Active and recent broadcasts"</p>
                </div>
            </div>

            {move || error.get().map(|e| view! {
                <div class="error-msg">{e}</div>
            })}

            {move || {
                if loading.get() {
                    view! { <p style="color: var(--text-secondary);">"Loading…"</p> }.into_any()
                } else if streams.get().is_empty() {
                    view! {
                        <div class="empty-state">
                            <div class="empty-icon">"📺"</div>
                            <h3>"No streams yet"</h3>
                            <p>"Start a stream from a sender's detail page to see it here."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="table-wrap">
                            <table class="data-table">
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
                                            let state_class = match stream.state.as_str() {
                                                "live" => "badge badge-live",
                                                "starting" | "stopping" => "badge badge-starting",
                                                "ended" => "badge badge-idle",
                                                "failed" => "badge badge-live",
                                                _ => "badge badge-idle",
                                            };
                                            let dot_class = match stream.state.as_str() {
                                                "live" => "dot dot-red",
                                                "starting" | "stopping" => "dot dot-yellow",
                                                _ => "dot dot-gray",
                                            };
                                            view! {
                                                <tr>
                                                    <td style="font-family: var(--font-mono); font-size: 12px;">{stream.id.clone()}</td>
                                                    <td>
                                                        <a href=format!("/senders/{}", stream.sender_id)>
                                                            {stream.sender_id.clone()}
                                                        </a>
                                                    </td>
                                                    <td>
                                                        <span class=state_class>
                                                            <span class=dot_class></span>
                                                            {stream.state.clone().to_uppercase()}
                                                        </span>
                                                    </td>
                                                    <td style="font-size: 12px;">{stream.started_at.clone().unwrap_or_else(|| "—".into())}</td>
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
