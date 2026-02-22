//! Destinations management page.

use leptos::prelude::*;

use crate::AuthState;
use crate::api;
use crate::types::DestinationSummary;

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

    // Derived: URL placeholder and help text based on selected platform
    let url_placeholder = Memo::new(move |_| match new_platform.get().as_str() {
        "youtube" => "rtmp://a.rtmp.youtube.com/live2".to_string(),
        "youtube_hls" => {
            "https://a.upload.youtube.com/http_upload_hls?cid=STREAM_KEY&copy=0&file=".to_string()
        }
        "twitch" => "rtmp://live.twitch.tv/app".to_string(),
        "srt" => "srt://host:port".to_string(),
        _ => "rtmp://your-server/live".to_string(),
    });
    let platform_help = Memo::new(move |_| {
        match new_platform.get().as_str() {
            "youtube" => "Standard RTMP ingest. H.265 requires Enhanced RTMP (eflvmux) — not all YouTube channels support this. Use YouTube HLS for reliable H.265.".to_string(),
            "youtube_hls" => "Paste the full HLS ingest URL from YouTube Studio → Go Live → Stream settings. It looks like: https://a.upload.youtube.com/http_upload_hls?cid=xxxx&copy=0&file=".to_string(),
            "twitch" => "Twitch only supports H.264 via RTMP. H.265 is not supported.".to_string(),
            "srt" => "SRT transport — supports both H.264 and H.265.".to_string(),
            _ => "Enter your RTMP server URL.".to_string(),
        }
    });
    // Whether the current platform needs a separate stream key field
    let show_stream_key = Memo::new(move |_| new_platform.get().as_str() != "youtube_hls");

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
            <div class="flex justify-between items-center mb-6">
                <div>
                    <h2 class="text-2xl font-semibold">"Destinations"</h2>
                    <p class="text-sm text-base-content/60 mt-1">"Streaming endpoints for your broadcasts"</p>
                </div>
                <button class="btn btn-primary" on:click=move |_| set_show_create.set(true)>
                    "+ Add Destination"
                </button>
            </div>

            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm mb-4">{e}</div>
            })}

            // Create modal
            {move || show_create.get().then(|| view! {
                <div class="modal modal-open">
                    <div class="modal-box">
                        <h3 class="text-lg font-semibold mb-4">"Add Destination"</h3>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Platform"</label>
                            <select
                                class="select select-bordered w-full"
                                on:change=move |ev| set_new_platform.set(event_target_value(&ev))
                            >
                                <option value="youtube">"YouTube (RTMP)"</option>
                                <option value="youtube_hls">"YouTube (HLS — H.265 Native)"</option>
                                <option value="twitch">"Twitch (RTMP — H.264 Only)"</option>
                                <option value="custom_rtmp">"Custom RTMP"</option>
                                <option value="srt">"SRT"</option>
                            </select>
                        </fieldset>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Name"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder="e.g. My YouTube Channel"
                                prop:value=move || new_name.get()
                                on:input=move |ev| set_new_name.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"URL"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder=move || url_placeholder.get()
                                prop:value=move || new_url.get()
                                on:input=move |ev| set_new_url.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <p class="text-xs text-base-content/50 -mt-2 mb-3 px-1">
                            {move || platform_help.get()}
                        </p>
                        {move || show_stream_key.get().then(|| view! {
                            <fieldset class="fieldset mb-3">
                                <label class="fieldset-label">"Stream Key (optional)"</label>
                                <input
                                    class="input input-bordered w-full"
                                    type="password"
                                    placeholder="xxxx-xxxx-xxxx-xxxx"
                                    prop:value=move || new_key.get()
                                    on:input=move |ev| set_new_key.set(event_target_value(&ev))
                                />
                            </fieldset>
                        })}
                        <div class="modal-action">
                            <button class="btn btn-ghost" on:click=move |_| set_show_create.set(false)>
                                "Cancel"
                            </button>
                            <button class="btn btn-primary" on:click=on_create>
                                "Create"
                            </button>
                        </div>
                    </div>
                    <div class="modal-backdrop" on:click=move |_| set_show_create.set(false)></div>
                </div>
            })}

            {move || {
                if loading.get() {
                    view! { <p class="text-base-content/60">"Loading…"</p> }.into_any()
                } else if destinations.get().is_empty() {
                    view! {
                        <div class="flex flex-col items-center justify-center py-16 text-center">
                            <div class="text-5xl mb-4">"🎯"</div>
                            <h3 class="text-lg font-medium mb-2">"No destinations"</h3>
                            <p class="text-sm text-base-content/60">"Add a streaming destination like YouTube or Twitch to broadcast to."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                            <For
                                each=move || destinations.get()
                                key=|d| d.id.clone()
                                children=move |dest| {
                                    let id = dest.id.clone();
                                    let on_del = on_delete;
                                    view! {
                                        <div class="card bg-base-200 border border-base-300">
                                            <div class="card-body">
                                                <div class="flex justify-between items-start">
                                                    <h3 class="card-title text-base">{dest.name.clone()}</h3>
                                                    <button
                                                        class="btn btn-ghost btn-sm"
                                                        on:click=move |_| on_del(id.clone())
                                                    >
                                                        "Delete"
                                                    </button>
                                                </div>
                                                <div class="flex flex-col gap-1.5 text-sm">
                                                    <div>
                                                        <span class="text-base-content/60">"Platform: "</span>
                                                        <span class="capitalize">{platform_label(&dest.platform).to_string()}</span>
                                                    </div>
                                                    <div>
                                                        <span class="text-base-content/60">"URL: "</span>
                                                        <span class="font-mono text-xs break-all">{dest.url.clone()}</span>
                                                    </div>
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
        "youtube" => "YouTube (RTMP)",
        "youtube_hls" => "YouTube (HLS)",
        "twitch" => "Twitch",
        "custom_rtmp" => "Custom RTMP",
        "srt" => "SRT",
        _ => p,
    }
}


