use std::net::IpAddr;

/// Resolve a network interface name (e.g., "eth0") to its first IPv4 address.
/// Returns `None` if the interface doesn't exist or has no IPv4 address.
pub fn resolve_iface_ipv4(iface: &str) -> Option<IpAddr> {
    let path = format!("/sys/class/net/{}/", iface);
    if !std::path::Path::new(&path).exists() {
        return None;
    }

    // Use libc getifaddrs for reliable interface address resolution.
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return None;
        }

        let mut current = ifaddrs;
        let mut result = None;

        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                if name == iface && (*ifa.ifa_addr).sa_family == libc::AF_INET as u16 {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let ip =
                        IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)));
                    result = Some(ip);
                    break;
                }
            }
            current = ifa.ifa_next;
        }

        libc::freeifaddrs(ifaddrs);
        result
    }
}

/// Modify a RIST URL to bind to a specific local IP address.
/// e.g., "rist://1.2.3.4:5000" + iface "eth0" (IP 10.0.0.1) -> "rist://10.0.0.1@1.2.3.4:5000"
/// This tells librist to use the specified local address for the socket,
/// effectively binding traffic to that interface.
pub fn bind_url_to_iface(url: &str, iface: &str) -> Option<String> {
    let local_ip = resolve_iface_ipv4(iface)?;

    // RIST URL format: rist://[local_ip@]remote_ip:port[?params]
    // We need to insert local_ip@ before the remote address.
    if let Some(rest) = url.strip_prefix("rist://") {
        // Check if there's already a local binding (@)
        if rest.contains('@') {
            // Already has a local binding, don't override
            return Some(url.to_string());
        }
        Some(format!("rist://{}@{}", local_ip, rest))
    } else {
        None
    }
}
