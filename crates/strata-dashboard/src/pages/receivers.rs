//! Receivers (relay) management page — register, list, and remove the
//! receiver fleet. Streams are assigned to the least-loaded online
//! receiver automatically; this page is where new relays get their
//! one-time enrollment token.

use leptos::prelude::*;

use crate::AuthState;
use crate::api;
use crate::api::ReceiverSummary;

#[component]
pub fn ReceiversPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (receivers, set_receivers) = signal(Vec::<ReceiverSummary>::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(true);
    let (show_create, set_show_create) = signal(false);
    // (receiver_id, one-time token) from the last create — shown once.
    let (new_token, set_new_token) = signal(Option::<(String, String)>::None);

    let (new_name, set_new_name) = signal(String::new());
    let (new_bind_host, set_new_bind_host) = signal(String::new());
    let (new_region, set_new_region) = signal(String::new());
    let (new_max_streams, set_new_max_streams) = signal("6".to_string());

    let auth_load = auth.clone();
    Effect::new(move || {
        let token = auth_load.token.get();
        if let Some(token) = token {
            let token = token.clone();
            leptos::task::spawn_local(async move {
                match api::list_receivers(&token).await {
                    Ok(data) => {
                        set_receivers.set(data);
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
        let name = new_name.get_untracked();
        let bind_host = new_bind_host.get_untracked();
        let region = new_region.get_untracked();
        let max_streams: i32 = new_max_streams.get_untracked().parse().unwrap_or(6);

        if bind_host.is_empty() {
            set_error.set(Some(
                "Bind host is required — the public IP/hostname senders will stream to".into(),
            ));
            return;
        }

        leptos::task::spawn_local(async move {
            match api::create_receiver(
                &token,
                (!name.is_empty()).then_some(name),
                &bind_host,
                (!region.is_empty()).then_some(region),
                max_streams,
            )
            .await
            {
                Ok(resp) => {
                    set_new_token.set(Some((resp.receiver_id, resp.enrollment_token)));
                    if let Ok(data) = api::list_receivers(&token).await {
                        set_receivers.set(data);
                    }
                    set_show_create.set(false);
                    set_new_name.set(String::new());
                    set_new_bind_host.set(String::new());
                    set_new_region.set(String::new());
                }
                Err(e) => set_error.set(Some(e)),
            }
        });
    };

    let auth_delete = auth.clone();
    let on_delete = move |id: String| {
        let token = auth_delete.token.get_untracked().unwrap_or_default();
        leptos::task::spawn_local(async move {
            match api::delete_receiver(&token, &id).await {
                Ok(()) => {
                    if let Ok(data) = api::list_receivers(&token).await {
                        set_receivers.set(data);
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
                    <h2 class="text-2xl font-semibold">"Receivers"</h2>
                    <p class="text-sm text-base-content/60 mt-1">"Relay fleet — streams are assigned to the least-loaded online receiver"</p>
                </div>
                <button class="btn btn-primary" on:click=move |_| set_show_create.set(true)>
                    "+ Register Receiver"
                </button>
            </div>

            {move || error.get().map(|e| view! {
                <div class="alert alert-error text-sm mb-4">{e}</div>
            })}

            // One-time enrollment token after create
            {move || new_token.get().map(|(rcv_id, tok)| view! {
                <div class="alert alert-warning text-sm mb-4 flex-col items-start gap-2">
                    <div>
                        <strong>"Enrollment token for " {rcv_id.clone()} " — shown once, copy it now."</strong>
                    </div>
                    <code class="font-mono text-xs break-all bg-base-300 p-2 rounded w-full">{tok.clone()}</code>
                    <div class="text-xs text-base-content/70">
                        "On the receiver box: strata-receiver --control-url ws://<control-host>:3000/receiver/ws --enrollment-token <token> (or set it in /etc/strata/receiver.env)"
                    </div>
                    <button class="btn btn-ghost btn-xs" on:click=move |_| set_new_token.set(None)>"Dismiss"</button>
                </div>
            })}

            // Create modal
            {move || show_create.get().then(|| view! {
                <div class="modal modal-open">
                    <div class="modal-box">
                        <h3 class="text-lg font-semibold mb-4">"Register Receiver"</h3>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Name (optional)"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder="e.g. Hetzner Helsinki"
                                prop:value=move || new_name.get()
                                on:input=move |ev| set_new_name.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Bind host"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder="public IP or hostname senders can reach"
                                prop:value=move || new_bind_host.get()
                                on:input=move |ev| set_new_bind_host.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <p class="text-xs text-base-content/50 -mt-2 mb-3 px-1">
                            "Note: the daemon's own --bind-host/--link-ports overwrite these values when it connects."
                        </p>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Region (optional)"</label>
                            <input
                                class="input input-bordered w-full"
                                type="text"
                                placeholder="e.g. eu-north"
                                prop:value=move || new_region.get()
                                on:input=move |ev| set_new_region.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <fieldset class="fieldset mb-3">
                            <label class="fieldset-label">"Max concurrent streams"</label>
                            <input
                                class="input input-bordered w-full"
                                type="number"
                                prop:value=move || new_max_streams.get()
                                on:input=move |ev| set_new_max_streams.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <div class="modal-action">
                            <button class="btn btn-ghost" on:click=move |_| set_show_create.set(false)>
                                "Cancel"
                            </button>
                            <button class="btn btn-primary" on:click=on_create>
                                "Register"
                            </button>
                        </div>
                    </div>
                    <div class="modal-backdrop" on:click=move |_| set_show_create.set(false)></div>
                </div>
            })}

            {move || {
                if loading.get() {
                    view! { <p class="text-base-content/60">"Loading…"</p> }.into_any()
                } else if receivers.get().is_empty() {
                    view! {
                        <div class="flex flex-col items-center justify-center py-16 text-center">
                            <div class="text-5xl mb-4">"📥"</div>
                            <h3 class="text-lg font-medium mb-2">"No receivers"</h3>
                            <p class="text-sm text-base-content/60">"Register a receiver to get an enrollment token for the relay box."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="overflow-x-auto">
                            <table class="table table-sm">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th>"Host"</th>
                                        <th>"Region"</th>
                                        <th>"Status"</th>
                                        <th>"Streams"</th>
                                        <th>"Last seen"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For
                                        each=move || receivers.get()
                                        key=|r| r.id.clone()
                                        children=move |rcv| {
                                            let id = rcv.id.clone();
                                            let on_del = on_delete;
                                            let name = rcv.name.clone().or(rcv.hostname.clone()).unwrap_or_else(|| rcv.id.clone());
                                            view! {
                                                <tr>
                                                    <td class="font-medium">{name}</td>
                                                    <td class="font-mono text-xs">{rcv.bind_host.clone()}</td>
                                                    <td>{rcv.region.clone().unwrap_or_else(|| "—".into())}</td>
                                                    <td>
                                                        {if rcv.online {
                                                            view! { <span class="badge badge-success badge-sm">"Online"</span> }.into_any()
                                                        } else {
                                                            view! { <span class="badge badge-ghost badge-sm">"Offline"</span> }.into_any()
                                                        }}
                                                    </td>
                                                    <td>{format!("{}/{}", rcv.active_streams, rcv.max_streams)}</td>
                                                    <td class="text-xs text-base-content/60">
                                                        {crate::pages::format_local_time(rcv.last_seen_at.as_deref())}
                                                    </td>
                                                    <td>
                                                        <button
                                                            class="btn btn-ghost btn-xs"
                                                            on:click=move |_| on_del(id.clone())
                                                        >
                                                            "Delete"
                                                        </button>
                                                    </td>
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
