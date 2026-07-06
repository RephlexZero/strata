pub mod destinations;
pub mod login;
pub mod receivers;
pub mod sender_detail;
pub mod senders;
pub mod streams;

/// Render an RFC3339 timestamp in the viewer's local timezone as
/// "YYYY-MM-DD HH:MM". Timestamps used to render hard-coded UTC, which a
/// field operator misreads by their offset (UX_TRUST_AUDIT U10).
pub fn format_local_time(rfc3339: Option<&str>) -> String {
    let Some(s) = rfc3339 else {
        return "—".into();
    };
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(s));
    if d.get_time().is_nan() {
        return s.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes()
    )
}

/// Human text for a stream end-reason slug (protocol `StreamEndReason`
/// strings plus the control plane's inferred slugs).
pub fn end_reason_label(reason: &str) -> &'static str {
    match reason {
        "pipeline_crash" => "pipeline crashed",
        "error" => "error",
        "user_stop" => "stopped by operator",
        "control_plane_stop" => "stopped by control plane",
        "agent_shutdown" => "device shut down",
        "timeout" => "stop timed out",
        "reconciled" => "pipeline found not running",
        "unobserved" => "connection to device lost",
        _ => "ended",
    }
}
