use std::process::Command;

/// A Linux network namespace managed via `ip netns`.
///
/// Creates the namespace on construction, initializes loopback, and
/// deletes the namespace on drop. Supports executing commands inside
/// the namespace and creating veth links to other namespaces.
pub struct Namespace {
    pub name: String,
}

impl Namespace {
    pub fn new(name: &str) -> Result<Self, std::io::Error> {
        // cleanup any existing namespace with the same name
        let _ = Command::new("sudo")
            .args(["ip", "netns", "del", name])
            .output();

        let output = Command::new("sudo")
            .args(["ip", "netns", "add", name])
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to create netns: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // Initialize loopback
        let _ = Command::new("sudo")
            .args(["ip", "netns", "exec", name, "ip", "link", "set", "lo", "up"])
            .output();

        Ok(Self {
            name: name.to_string(),
        })
    }

    pub fn exec(&self, cmd: &str, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
        Command::new("sudo")
            .args(["ip", "netns", "exec", &self.name, cmd])
            .args(args)
            .output()
    }

    pub fn add_veth_link(
        &self,
        other: &Namespace,
        veth_name_local: &str,
        veth_name_peer: &str,
        ip_local: &str,
        ip_peer: &str,
    ) -> std::io::Result<()> {
        // Clean up potential leftovers in host
        let _ = Command::new("sudo")
            .args(["ip", "link", "del", veth_name_local])
            .output();

        // 1. Create veth pair in host
        let output = Command::new("sudo")
            .args([
                "ip",
                "link",
                "add",
                veth_name_local,
                "type",
                "veth",
                "peer",
                "name",
                veth_name_peer,
            ])
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to create veth pair: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // 2. Move veth_local to self
        let output = Command::new("sudo")
            .args(["ip", "link", "set", veth_name_local, "netns", &self.name])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to move local veth: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // 3. Move veth_peer to other
        let output = Command::new("sudo")
            .args(["ip", "link", "set", veth_name_peer, "netns", &other.name])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to move peer veth: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // 4. Configure self (local side)
        let output = self.exec("ip", &["addr", "add", ip_local, "dev", veth_name_local])?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to set local IP: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let output = self.exec("ip", &["link", "set", veth_name_local, "up"])?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to set local link up: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // 5. Configure other (peer side)
        let output = other.exec("ip", &["addr", "add", ip_peer, "dev", veth_name_peer])?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to set peer IP: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let output = other.exec("ip", &["link", "set", veth_name_peer, "up"])?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Failed to set peer link up: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        let _ = Command::new("sudo")
            .args(["ip", "netns", "del", &self.name])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::check_privileges;

    #[test]
    fn test_create_namespace_pair() {
        if !check_privileges() {
            eprintln!("Skipping test, unsufficient privileges or missing tools");
            return;
        }

        use crate::test_util::unique_ns_name;
        let ns1_name = unique_ns_name("rst_a");
        let ns2_name = unique_ns_name("rst_b");
        let ns1 = Namespace::new(&ns1_name).expect("Failed to create ns1");
        let _ns2 = Namespace::new(&ns2_name).expect("Failed to create ns2");

        let out1 = ns1.exec("ip", &["link"]).expect("Failed to exec ip link");
        let out1_str = String::from_utf8_lossy(&out1.stdout);
        assert!(out1_str.contains("lo"));
    }

    #[test]
    fn test_veth_link() {
        if !check_privileges() {
            eprintln!("Skipping test, unsufficient privileges or missing tools");
            return;
        }

        use crate::test_util::unique_ns_name;
        let ns1_name = unique_ns_name("rst_la");
        let ns2_name = unique_ns_name("rst_lb");
        let ns1 = Namespace::new(&ns1_name).expect("Failed to create ns1");
        let ns2 = Namespace::new(&ns2_name).expect("Failed to create ns2");

        // Use random suffix/distinct names to avoid parallel conflicts
        // Interface name limit is 15 chars. "veth_a_" is 7 chars. We have 8 chars left.
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let suffix = now % 100000; // 5 digits
        let v_a = format!("veth_a_{}", suffix);
        let v_b = format!("veth_b_{}", suffix);

        // Use distinct subnets or IPs to avoid conflicts with host or other tests if running in parallel
        // Using 10.200.1.0/24 for this test
        ns1.add_veth_link(&ns2, &v_a, &v_b, "10.200.1.1/24", "10.200.1.2/24")
            .expect("Failed to create veth link");

        // ping from ns1 to ns2
        let out = ns1
            .exec("ping", &["-c", "1", "-W", "1", "10.200.1.2"])
            .expect("Failed to exec ping");

        if !out.status.success() {
            panic!(
                "Ping failed:\nStdout: {}\nStderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
