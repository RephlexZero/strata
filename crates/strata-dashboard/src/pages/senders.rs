//! Sender list page.

use leptos::prelude::*;

use crate::api;
use crate::types::SenderSummary;
use crate::AuthState;

/// Displays all senders belonging to the authenticated user.
#[component]
pub fn SendersPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (senders, set_senders) = signal(Vec::<SenderSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(true);
    let (show_create, set_show_create) = signal(false);
    let (new_name, set_new_name) = signal(String::new());
    let (enrollment_info, set_enrollment_info) = signal(Option::<(String, String)>::None);

    // Load senders on mount
    let auth_load = auth.clone();
    Effect::new(move || {
        let token = auth_load.token.get();
        if let Some(token) = token {
            let token = token.clone();
            leptos::task::spawn_local(async move {
                match api::list_senders(&token).await {
                    Ok(data) => {
                        set_senders.set(data);
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

    let auth_create = auth.clone();
    let on_create = move |_| {
        let name_val = new_name.get_untracked();
        let name = if name_val.is_empty() {
            None
        } else {
            Some(name_val)
        };
        let token = auth_create.token.get_untracked().unwrap_or_default();
        leptos::task::spawn_local(async move {
            match api::create_sender(&token, name).await {
                Ok(resp) => {
                    set_enrollment_info.set(Some((
                        resp.sender_id.clone(),
                        resp.enrollment_token.clone(),
                    )));
                    // Reload senders
                    if let Ok(data) = api::list_senders(&token).await {
                        set_senders.set(data);
                    }
                    set_show_create.set(false);
                    set_new_name.set(String::new());
                }
                Err(e) => set_error.set(Some(e)),
            }
        });
    };

    view! {
        <div>
            <div class="page-header">
                <div>
                    <h2>"Senders"</h2>
                    <p class="subtitle">"Manage your field encoder units"</p>
                </div>
                <button class="btn btn-primary" on:click=move |_| set_show_create.set(true)>
                    "+ Add Sender"
                </button>
            </div>

            {move || error.get().map(|e| view! {
                <div class="error-msg">{e}</div>
            })}

            {move || enrollment_info.get().map(|(sid, token)| view! {
                <div class="card" style="border-color: var(--green); margin-bottom: 16px;">
                    <div class="card-header">
                        <h3>"Sender Created"</h3>
                        <button class="btn btn-ghost btn-sm" on:click=move |_| set_enrollment_info.set(None)>
                            "Dismiss"
                        </button>
                    </div>
                    <p style="font-size: 13px; color: var(--text-secondary); margin-bottom: 8px;">
                        "Save this enrollment token — it will not be shown again."
                    </p>
                    <div style="font-family: var(--font-mono); font-size: 13px; background: var(--bg-tertiary); padding: 8px 12px; border-radius: 4px; word-break: break-all;">
                        <div>"Sender ID: " {sid}</div>
                        <div>"Token: " {token}</div>
                    </div>
                </div>
            })}

            // Create modal
            {move || show_create.get().then(|| view! {
                <div class="modal-backdrop" on:click=move |_| set_show_create.set(false)>
                    <div class="modal" on:click=move |ev| ev.stop_propagation()>
                        <h3>"Add Sender"</h3>
                        <div class="form-group">
                            <label>"Name (optional)"</label>
                            <input
                                class="form-input"
                                type="text"
                                placeholder="e.g. Camera 1"
                                prop:value=move || new_name.get()
                                on:input=move |ev| set_new_name.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="modal-actions">
                            <button class="btn btn-ghost" on:click=move |_| set_show_create.set(false)>
                                "Cancel"
                            </button>
                            <button class="btn btn-primary" on:click=on_create>
                                "Create"
                            </button>
                        </div>
                    </div>
                </div>
            })}

            {move || {
                if loading.get() {
                    view! { <p style="color: var(--text-secondary);">"Loading…"</p> }.into_any()
                } else if senders.get().is_empty() {
                    view! {
                        <div class="empty-state">
                            <div class="empty-icon">"📡"</div>
                            <h3>"No senders yet"</h3>
                            <p>"Add a sender to get started. Each sender represents a field encoder unit."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="card-grid">
                            <For
                                each=move || senders.get()
                                key=|s| s.id.clone()
                                children=move |sender| {
                                    let id = sender.id.clone();
                                    let href = format!("/senders/{}", id);
                                    view! {
                                        <a href=href style="text-decoration: none; color: inherit;">
                                            <div class="card card-hover sender-card">
                                                <div class="sender-header">
                                                    <div>
                                                        <div class="sender-name">
                                                            {sender.name.clone().unwrap_or_else(|| sender.id.clone())}
                                                        </div>
                                                        <div class="sender-hostname">
                                                            {sender.hostname.clone().unwrap_or_else(|| "—".into())}
                                                        </div>
                                                    </div>
                                                    <div class={if sender.online { "badge badge-online" } else { "badge badge-offline" }}>
                                                        <span class={if sender.online { "dot dot-green" } else { "dot dot-gray" }}></span>
                                                        {if sender.online { "Online" } else { "Offline" }}
                                                    </div>
                                                </div>
                                                <div class="sender-meta">
                                                    <span>"ID: " {sender.id.clone()}</span>
                                                </div>
                                            </div>
                                        </a>
                                    }
                                }
                            />
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}
