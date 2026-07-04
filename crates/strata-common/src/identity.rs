//! Persistent device identity for sender/receiver daemons.
//!
//! An ed25519 keypair generated on first run, plus the device id learned at
//! enrollment. Enrollment tokens are single-use (E4) — this file is the
//! device's only reconnect credential, so it is created and persisted
//! *before* the token is spent.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::auth;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Device id (`snd_...` / `rcv_...`) learned from the enrollment
    /// response; `None` until first successful enrollment.
    pub device_id: Option<String>,
    /// ed25519 private key seed, base64.
    pub private_key: String,
    /// ed25519 public key, base64.
    pub public_key: String,
}

impl DeviceIdentity {
    /// Load the identity file, or generate a fresh keypair and persist it.
    /// Fails (rather than continuing key-less) if the path is unwritable —
    /// enrolling with an unpersistable key would consume the one-time token
    /// and leave the device unable to ever reconnect.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let identity: Self = serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("corrupt identity file {}: {e}", path.display()))?;
            return Ok(identity);
        }

        let (private_key, public_key) = auth::generate_device_keypair();
        let identity = Self {
            device_id: None,
            private_key,
            public_key,
        };
        identity.save(path)?;
        Ok(identity)
    }

    /// Persist to `path` (0600 on unix — it holds a private key).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_persist_reload_round_trip() {
        let dir = std::env::temp_dir().join(format!("strata-id-test-{}", std::process::id()));
        let path = dir.join("identity.json");

        let fresh = DeviceIdentity::load_or_generate(&path).unwrap();
        assert!(fresh.device_id.is_none());

        let mut enrolled = fresh.clone();
        enrolled.device_id = Some("snd_test".into());
        enrolled.save(&path).unwrap();

        let reloaded = DeviceIdentity::load_or_generate(&path).unwrap();
        assert_eq!(reloaded.device_id.as_deref(), Some("snd_test"));
        assert_eq!(reloaded.private_key, fresh.private_key);
        assert_eq!(reloaded.public_key, fresh.public_key);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
