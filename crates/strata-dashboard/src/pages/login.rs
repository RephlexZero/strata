//! Login page component.

use leptos::ev;
use leptos::prelude::*;

use crate::api;
use crate::AuthState;

/// Login page — email/password form.
///
/// In dev mode, credentials are pre-filled for quick access.
#[component]
pub fn LoginPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (email, set_email) = signal("dev@strata.local".to_string());
    let (password, set_password) = signal("development".to_string());
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
                    auth.login(resp.token);
                }
                Err(e) => {
                    set_error.set(Some(e));
                    set_loading.set(false);
                }
            }
        });
    };

    let do_login_form = do_login.clone();
    let on_submit = move |ev: ev::SubmitEvent| {
        ev.prevent_default();
        let email_val = email.get_untracked();
        let password_val = password.get_untracked();
        if email_val.is_empty() || password_val.is_empty() {
            set_error.set(Some("Email and password are required".into()));
            return;
        }
        do_login_form(email_val, password_val);
    };

    // Auto-login on mount with dev credentials
    let do_login_auto = do_login.clone();
    Effect::new(move || {
        do_login_auto("dev@strata.local".to_string(), "development".to_string());
    });

    view! {
        <div class="flex items-center justify-center min-h-screen bg-base-100">
            <div class="card bg-base-200 border border-base-300 w-full max-w-sm">
                <div class="card-body">
                    <h1 class="text-2xl font-bold text-center">"Strata"</h1>
                    <p class="text-center text-sm text-base-content/60 mb-2">"Bonded streaming control plane"</p>
                    <p class="text-center mb-4"><span class="badge badge-ghost badge-sm">"Dev — auto-login"</span></p>

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
                                placeholder="dev@strata.local"
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
