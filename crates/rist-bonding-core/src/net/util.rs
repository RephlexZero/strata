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

/// Modify a RIST URL to bind to a specific local IP address via the `miface`
/// query parameter. librist uses `miface` for source-address binding on
/// sender sockets (calls `bind()` with the IP or `SO_BINDTODEVICE` with a
/// device name).
///
/// e.g., "rist://1.2.3.4:5000" + iface "eth0" (IP 10.0.0.1)
///    -> "rist://1.2.3.4:5000?miface=10.0.0.1"
///
/// If the URL already has query parameters the new param is appended with `&`.
pub fn bind_url_to_iface(url: &str, iface: &str) -> Option<String> {
    let local_ip = resolve_iface_ipv4(iface)?;

    if !url.starts_with("rist://") {
        return None;
    }

    // If there's already a miface parameter, don't override.
    if url.contains("miface=") {
        return Some(url.to_string());
    }

    let separator = if url.contains('?') { '&' } else { '?' };
    Some(format!("{}{}miface={}", url, separator, local_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_loopback_returns_127() {
        // `lo` exists on every Linux box
        let ip = resolve_iface_ipv4("lo");
        assert_eq!(
            ip,
            Some(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
            "lo should resolve to 127.0.0.1"
        );
    }

    #[test]
    fn resolve_nonexistent_returns_none() {
        let ip = resolve_iface_ipv4("does_not_exist_xyz99");
        assert_eq!(ip, None, "Non-existent interface should return None");
    }

    #[test]
    fn bind_url_appends_miface() {
        // Use `lo` since it always resolves to 127.0.0.1
        let result = bind_url_to_iface("rist://1.2.3.4:5000", "lo");
        assert_eq!(
            result,
            Some("rist://1.2.3.4:5000?miface=127.0.0.1".to_string()),
            "Should append miface query parameter"
        );
    }

    #[test]
    fn bind_url_appends_to_existing_params() {
        let result = bind_url_to_iface("rist://1.2.3.4:5000?buffer=2000", "lo");
        assert_eq!(
            result,
            Some("rist://1.2.3.4:5000?buffer=2000&miface=127.0.0.1".to_string()),
            "Should append with & when query params already exist"
        );
    }

    #[test]
    fn bind_url_preserves_existing_miface() {
        let url = "rist://1.2.3.4:5000?miface=10.0.0.1";
        let result = bind_url_to_iface(url, "lo");
        assert_eq!(
            result,
            Some(url.to_string()),
            "Already-bound URL should be returned unchanged"
        );
    }

    #[test]
    fn bind_url_non_rist_scheme_returns_none() {
        let result = bind_url_to_iface("http://1.2.3.4:5000", "lo");
        assert_eq!(result, None, "Non-rist scheme should return None");
    }
}
