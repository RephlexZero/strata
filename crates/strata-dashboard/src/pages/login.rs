//! Login page component.

use leptos::ev;
use leptos::prelude::*;

use crate::api;
use crate::AuthState;

/// Login page — email/password form.
#[component]
pub fn LoginPage() -> impl IntoView {
    let auth = expect_context::<AuthState>();
    let (email, set_email) = signal(String::new());
    let (password, set_password) = signal(String::new());
    let (error, set_error) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(false);

    let on_submit = move |ev: ev::SubmitEvent| {
        ev.prevent_default();
        let email_val = email.get_untracked();
        let password_val = password.get_untracked();
        if email_val.is_empty() || password_val.is_empty() {
            set_error.set(Some("Email and password are required".into()));
            return;
        }
        set_loading.set(true);
        set_error.set(None);
        let auth = auth.clone();
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

    view! {
        <div class="login-page">
            <div class="login-card">
                <h1>"Strata"</h1>
                <p class="login-subtitle">"Bonded streaming control plane"</p>

                {move || error.get().map(|e| view! {
                    <div class="error-msg">{e}</div>
                })}

                <form on:submit=on_submit>
                    <div class="form-group">
                        <label for="email">"Email"</label>
                        <input
                            id="email"
                            class="form-input"
                            type="email"
                            placeholder="admin@strata.local"
                            prop:value=move || email.get()
                            on:input=move |ev| set_email.set(event_target_value(&ev))
                        />
                    </div>
                    <div class="form-group">
                        <label for="password">"Password"</label>
                        <input
                            id="password"
                            class="form-input"
                            type="password"
                            placeholder="••••••••"
                            prop:value=move || password.get()
                            on:input=move |ev| set_password.set(event_target_value(&ev))
                        />
                    </div>
                    <button
                        class="btn btn-primary"
                        type="submit"
                        disabled=move || loading.get()
                    >
                        {move || if loading.get() { "Signing in…" } else { "Sign in" }}
                    </button>
                </form>
            </div>
        </div>
    }
}
