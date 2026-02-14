//! Destinations management page.

use leptos::prelude::*;

use crate::api;
use crate::types::DestinationSummary;
use crate::AuthState;

/// CRUD page for streaming destinations.
#[component]
pub fn DestinationsPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (destinations, set_destinations) = signal(Vec::<DestinationSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(true);
    let (show_create, set_show_create) = signal(false);

    // Create form fields
    let (new_name, set_new_name) = signal(String::new());
    let (new_platform, set_new_platform) = signal("youtube".to_string());
    let (new_url, set_new_url) = signal(String::new());
    let (new_key, set_new_key) = signal(String::new());

    // Load destinations
    let auth_load = auth.clone();
    Effect::new(move || {
        let token = auth_load.token.get();
        if let Some(token) = token {
            let token = token.clone();
            leptos::task::spawn_local(async move {
                match api::list_destinations(&token).await {
                    Ok(data) => {
                        set_destinations.set(data);
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
        let token = auth_create.token.get_untracked().unwrap_or_default();
        let platform = new_platform.get_untracked();
        let name = new_name.get_untracked();
        let url = new_url.get_untracked();
        let key = new_key.get_untracked();
        let stream_key = if key.is_empty() { None } else { Some(key) };

        if name.is_empty() || url.is_empty() {
            set_error.set(Some("Name and URL are required".into()));
            return;
        }

        leptos::task::spawn_local(async move {
            match api::create_destination(&token, &platform, &name, &url, stream_key).await {
                Ok(_) => {
                    // Reload
                    if let Ok(data) = api::list_destinations(&token).await {
                        set_destinations.set(data);
                    }
                    set_show_create.set(false);
                    set_new_name.set(String::new());
                    set_new_url.set(String::new());
                    set_new_key.set(String::new());
                }
                Err(e) => set_error.set(Some(e)),
            }
        });
    };

    let auth_delete = auth.clone();
    let on_delete = move |id: String| {
        let token = auth_delete.token.get_untracked().unwrap_or_default();
        leptos::task::spawn_local(async move {
            match api::delete_destination(&token, &id).await {
                Ok(()) => {
                    if let Ok(data) = api::list_destinations(&token).await {
                        set_destinations.set(data);
                    }
                }
                Err(e) => set_error.set(Some(e)),
            }
        });
    };

    view! {
        <div>
            <div class="page-header">
                <div>
                    <h2>"Destinations"</h2>
                    <p class="subtitle">"Streaming endpoints for your broadcasts"</p>
                </div>
                <button class="btn btn-primary" on:click=move |_| set_show_create.set(true)>
                    "+ Add Destination"
                </button>
            </div>

            {move || error.get().map(|e| view! {
                <div class="error-msg">{e}</div>
            })}

            // Create modal
            {move || show_create.get().then(|| view! {
                <div class="modal-backdrop" on:click=move |_| set_show_create.set(false)>
                    <div class="modal" on:click=move |ev| ev.stop_propagation()>
                        <h3>"Add Destination"</h3>
                        <div class="form-group">
                            <label>"Platform"</label>
                            <select
                                class="form-input"
                                on:change=move |ev| set_new_platform.set(event_target_value(&ev))
                            >
                                <option value="youtube">"YouTube"</option>
                                <option value="twitch">"Twitch"</option>
                                <option value="custom_rtmp">"Custom RTMP"</option>
                                <option value="srt">"SRT"</option>
                            </select>
                        </div>
                        <div class="form-group">
                            <label>"Name"</label>
                            <input
                                class="form-input"
                                type="text"
                                placeholder="e.g. My YouTube Channel"
                                prop:value=move || new_name.get()
                                on:input=move |ev| set_new_name.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="form-group">
                            <label>"URL"</label>
                            <input
                                class="form-input"
                                type="text"
                                placeholder="rtmp://a.rtmp.youtube.com/live2"
                                prop:value=move || new_url.get()
                                on:input=move |ev| set_new_url.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="form-group">
                            <label>"Stream Key (optional)"</label>
                            <input
                                class="form-input"
                                type="password"
                                placeholder="xxxx-xxxx-xxxx-xxxx"
                                prop:value=move || new_key.get()
                                on:input=move |ev| set_new_key.set(event_target_value(&ev))
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
                } else if destinations.get().is_empty() {
                    view! {
                        <div class="empty-state">
                            <div class="empty-icon">"🎯"</div>
                            <h3>"No destinations"</h3>
                            <p>"Add a streaming destination like YouTube or Twitch to broadcast to."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="card-grid">
                            <For
                                each=move || destinations.get()
                                key=|d| d.id.clone()
                                children=move |dest| {
                                    let id = dest.id.clone();
                                    let on_del = on_delete;
                                    view! {
                                        <div class="card">
                                            <div class="card-header">
                                                <h3>{dest.name.clone()}</h3>
                                                <button
                                                    class="btn btn-ghost btn-sm"
                                                    on:click=move |_| on_del(id.clone())
                                                >
                                                    "Delete"
                                                </button>
                                            </div>
                                            <div style="display: flex; flex-direction: column; gap: 6px; font-size: 13px;">
                                                <div>
                                                    <span style="color: var(--text-secondary);">"Platform: "</span>
                                                    <span style="text-transform: capitalize;">{platform_label(&dest.platform).to_string()}</span>
                                                </div>
                                                <div>
                                                    <span style="color: var(--text-secondary);">"URL: "</span>
                                                    <span style="font-family: var(--font-mono); font-size: 12px; word-break: break-all;">{dest.url.clone()}</span>
                                                </div>
                                            </div>
                                        </div>
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

fn platform_label(p: &str) -> &str {
    match p {
        "youtube" => "YouTube",
        "twitch" => "Twitch",
        "custom_rtmp" => "Custom RTMP",
        "srt" => "SRT",
        _ => p,
    }
}
