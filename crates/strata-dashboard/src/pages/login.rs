//! Login page component.

use leptos::ev;
use leptos::prelude::*;

use crate::AuthState;
use crate::api;

/// Login page — email/password form.
#[component]
pub fn LoginPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (email, set_email) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(false);

    let auth_submit = auth.clone();
    let do_login = move |email_val: String, password_val: String| {
        set_loading.set(true);
        set_error.set(None);
        let auth = auth_submit.clone();
        leptos::task::spawn_local(async move {
            match api::login(&email_val, &password_val).await {
                Ok(resp) => {
                    auth.login(resp.token, resp.role);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_loading.set(false);
                }
            }
        });
    };

    let on_submit = move |ev: ev::SubmitEvent| {
        ev.prevent_default();
        let email_val = email.get_untracked();
        let password_val = password.get_untracked();
        if email_val.is_empty() || password_val.is_empty() {
            set_error.set(Some("Email and password are required".into()));
            return;
        }
        do_login(email_val, password_val);
    };

    view! {
        <div class="flex items-center justify-center min-h-screen bg-base-100">
            <div class="card bg-base-200 border border-base-300 w-full max-w-sm">
                <div class="card-body">
                    <h1 class="text-2xl font-bold text-center">"Strata"</h1>
                    <p class="text-center text-sm text-base-content/60 mb-4">"Bonded streaming control plane"</p>

                    {move || error.get().map(|e| view! {
                        <div class="alert alert-error text-sm mb-4">{e}</div>
                    })}

                    <form on:submit=on_submit>
                        <fieldset class="fieldset">
                            <label class="fieldset-label" for="email">"Email"</label>
                            <input
                                id="email"
                                class="input input-bordered w-full"
                                type="email"
                                placeholder="email@example.com"
                                prop:value=move || email.get()
                                on:input=move |ev| set_email.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <fieldset class="fieldset">
                            <label class="fieldset-label" for="password">"Password"</label>
                            <input
                                id="password"
                                class="input input-bordered w-full"
                                type="password"
                                placeholder="••••••••"
                                prop:value=move || password.get()
                                on:input=move |ev| set_password.set(event_target_value(&ev))
                            />
                        </fieldset>
                        <button
                            class="btn btn-primary w-full mt-4"
                            type="submit"
                            disabled=move || loading.get()
                        >
                            {move || if loading.get() { "Signing in…" } else { "Sign in" }}
                        </button>
                    </form>
                </div>
            </div>
        </div>
    }
}
