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
    let (creating, set_creating) = signal(false);
    // After creation, the modal transitions to show the enrollment token
    let (created_info, set_created_info) = signal(Option::<(String, String)>::None);

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
        set_creating.set(true);
        leptos::task::spawn_local(async move {
            match api::create_sender(&token, name).await {
                Ok(resp) => {
                    // Transition the modal to show enrollment info
                    set_created_info.set(Some((
                        resp.sender_id.clone(),
                        resp.enrollment_token.clone(),
                    )));
                    set_creating.set(false);
                    set_new_name.set(String::new());
                    if let Ok(data) = api::list_senders(&token).await {
                        set_senders.set(data);
                    }
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_creating.set(false);
                }
            }
        });
    };

    let close_modal = move |_| {
        set_show_create.set(false);
        set_created_info.set(None);
    };

    view! {
        <div>
            <div class="flex justify-between items-center mb-6">
                <div>
                    <h2 class="text-2xl font-semibold">"Senders"</h2>
                    <p class="text-sm text-base-content/60 mt-1">"Manage your field encoder units"</p>
                </div>
                <button class="btn btn-primary" on:click=move |_| set_show_create.set(true)>
                    "+ Add Sender"
                </button>
            </div>

            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm mb-4">{e}</div>
            })}

            // Create modal — transitions between form and enrollment token display
            {move || show_create.get().then(|| {
                let info = created_info.get();
                if let Some((sid, token)) = info {
                    // ── Post-creation: show enrollment token ─────────
                    view! {
                        <div class="modal modal-open">
                            <div class="modal-box">
                                <h3 class="font-bold text-lg text-success">"✓ Sender Created"</h3>
                                <div class="mt-4">
                                    <div class="alert alert-warning text-sm mb-4">
                                        "Save this enrollment token — it will not be shown again."
                                    </div>
                                    <div class="font-mono text-sm bg-base-300 p-4 rounded space-y-2">
                                        <div class="flex justify-between">
                                            <span class="text-base-content/60">"Sender ID"</span>
                                            <span class="font-semibold">{sid}</span>
                                        </div>
                                        <div class="flex justify-between">
                                            <span class="text-base-content/60">"Token"</span>
                                            <span class="font-semibold tracking-wider">{token}</span>
                                        </div>
                                    </div>
                                    <p class="text-sm text-base-content/60 mt-4">
                                        "Enter this token on the sender device's portal to enroll it."
                                    </p>
                                </div>
                                <div class="modal-action">
                                    <button class="btn btn-primary" on:click=close_modal>
                                        "Done"
                                    </button>
                                </div>
                            </div>
                            <div class="modal-backdrop" on:click=close_modal>
                                <button>"close"</button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    // ── Create form ─────────────────────────────────
                    view! {
                        <div class="modal modal-open">
                            <div class="modal-box">
                                <h3 class="font-bold text-lg">"Add Sender"</h3>
                                <fieldset class="fieldset mt-4">
                                    <label class="fieldset-label">"Name"</label>
                                    <input
                                        class="input input-bordered w-full"
                                        type="text"
                                        placeholder="e.g. Camera 1"
                                        prop:value=move || new_name.get()
                                        on:input=move |ev| set_new_name.set(event_target_value(&ev))
                                    />
                                    <p class="text-xs text-base-content/40 mt-1">"A friendly name for this encoder unit"</p>
                                </fieldset>
                                <div class="modal-action">
                                    <button class="btn btn-ghost" on:click=close_modal>
                                        "Cancel"
                                    </button>
                                    <button class="btn btn-primary" on:click=on_create disabled=move || creating.get()>
                                        {move || if creating.get() { "Creating…" } else { "Create Sender" }}
                                    </button>
                                </div>
                            </div>
                            <div class="modal-backdrop" on:click=close_modal>
                                <button>"close"</button>
                            </div>
                        </div>
                    }.into_any()
                }
            })}

            {move || {
                if loading.get() {
                    view! { <p class="text-base-content/60">"Loading…"</p> }.into_any()
                } else if senders.get().is_empty() {
                    view! {
                        <div class="text-center py-16 text-base-content/60">
                            <div class="text-5xl mb-4">"📡"</div>
                            <h3 class="text-lg font-semibold text-base-content mb-2">"No senders yet"</h3>
                            <p class="text-sm max-w-sm mx-auto mb-5">"Add a sender to get started. Each sender represents a field encoder unit."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                            <For
                                each=move || senders.get()
                                key=|s| s.id.clone()
                                children=move |sender| {
                                    let id = sender.id.clone();
                                    let href = format!("/senders/{}", id);
                                    view! {
                                        <a href=href class="no-underline text-base-content">
                                            <div class="card bg-base-200 border border-base-300 hover:bg-base-300 cursor-pointer transition-colors">
                                                <div class="card-body gap-3">
                                                    <div class="flex justify-between items-start">
                                                        <div>
                                                            <div class="font-semibold">
                                                                {sender.name.clone().unwrap_or_else(|| sender.id.clone())}
                                                            </div>
                                                            <div class="text-sm text-base-content/60 font-mono">
                                                                {sender.hostname.clone().unwrap_or_else(|| "—".into())}
                                                            </div>
                                                        </div>
                                                        <div class={if sender.online { "badge badge-success gap-1" } else { "badge badge-ghost gap-1" }}>
                                                            <span class={if sender.online { "w-2 h-2 rounded-full bg-success" } else { "w-2 h-2 rounded-full bg-base-content/30" }}></span>
                                                            {if sender.online { "Online" } else { "Offline" }}
                                                        </div>
                                                    </div>
                                                    <div class="text-sm text-base-content/60">
                                                        <span>"ID: " {sender.id.clone()}</span>
                                                    </div>
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
