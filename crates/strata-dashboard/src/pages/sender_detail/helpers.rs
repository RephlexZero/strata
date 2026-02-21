use leptos::prelude::*;

use crate::types::{MediaInput, NetworkInterface, SenderFullStatus};

pub fn apply_full_status(
    status: &SenderFullStatus,
    set_ifaces: &WriteSignal<Vec<NetworkInterface>>,
    set_inputs: &WriteSignal<Vec<MediaInput>>,
    set_cpu: &WriteSignal<Option<f32>>,
    set_mem: &WriteSignal<Option<u32>>,
    set_uptime: &WriteSignal<Option<u64>>,
    set_receiver_url: &WriteSignal<Option<String>>,
) {
    if let Some(ifaces) = &status.network_interfaces {
        set_ifaces.set(ifaces.clone());
    }
    if let Some(inputs) = &status.media_inputs {
        set_inputs.set(inputs.clone());
    }
    if status.cpu_percent.is_some() {
        set_cpu.set(status.cpu_percent);
    }
    if status.mem_used_mb.is_some() {
        set_mem.set(status.mem_used_mb);
    }
    if status.uptime_s.is_some() {
        set_uptime.set(status.uptime_s);
    }
    if status.receiver_url.is_some() {
        set_receiver_url.set(status.receiver_url.clone());
    }
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn format_bps(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.0} kbps", bps as f64 / 1_000.0)
    } else {
        format!("{bps} bps")
    }
}

pub fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}
