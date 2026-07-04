//! The outer envelope for all WebSocket messages.

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire protocol schema version. Bump on any breaking change to message
/// shapes; peers log a warning on mismatch (they do not disconnect — the
/// version exists so a mixed-version fleet is *visible*, not to gate it).
pub const PROTOCOL_VERSION: u32 = 1;

fn default_proto_version() -> u32 {
    // Envelopes from peers that predate versioning carry no field — treat
    // them as version 1, the schema in effect when the field was added.
    1
}

/// The outer envelope for all WebSocket messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Unique message ID (UUIDv7, time-ordered).
    pub id: String,
    /// Message type (dotted namespace, e.g. "device.status").
    #[serde(rename = "type")]
    pub msg_type: String,
    /// ISO 8601 timestamp.
    pub ts: DateTime<Utc>,
    /// Protocol schema version ([`PROTOCOL_VERSION`]).
    #[serde(default = "default_proto_version")]
    pub proto_version: u32,
    /// Type-specific payload.
    pub payload: serde_json::Value,
}

impl Envelope {
    /// Create a new envelope with a fresh UUIDv7 and current timestamp.
    ///
    /// Panics if the payload cannot be serialized to JSON (see [`Envelope::try_new`]).
    pub fn new(msg_type: impl Into<String>, payload: impl Serialize) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            msg_type: msg_type.into(),
            ts: Utc::now(),
            proto_version: PROTOCOL_VERSION,
            payload: serde_json::to_value(payload).expect("payload serialization"),
        }
    }

    /// Fallible version of [`Envelope::new`] that returns an error on
    /// serialization failure instead of panicking.
    pub fn try_new(
        msg_type: impl Into<String>,
        payload: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            id: Uuid::now_v7().to_string(),
            msg_type: msg_type.into(),
            ts: Utc::now(),
            proto_version: PROTOCOL_VERSION,
            payload: serde_json::to_value(payload)?,
        })
    }

    /// Build an envelope from a direction-enum message (e.g.
    /// [`crate::AgentMessage`]). The enum's serde tag becomes `msg_type` and
    /// its content becomes `payload`, so the wire type string exists in
    /// exactly one place — the enum definition.
    pub fn from_message<M: Serialize>(msg: &M) -> Result<Self, serde_json::Error> {
        use serde::ser::Error;
        let value = serde_json::to_value(msg)?;
        let serde_json::Value::Object(mut map) = value else {
            return Err(serde_json::Error::custom(
                "message must serialize to a tagged object",
            ));
        };
        let msg_type = match map.remove("type") {
            Some(serde_json::Value::String(s)) => s,
            _ => return Err(serde_json::Error::custom("message missing \"type\" tag")),
        };
        let payload = map.remove("payload").unwrap_or(serde_json::Value::Null);
        Ok(Self {
            id: Uuid::now_v7().to_string(),
            msg_type,
            ts: Utc::now(),
            proto_version: PROTOCOL_VERSION,
            payload,
        })
    }

    /// Parse the payload into a concrete type.
    pub fn parse_payload<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    /// Parse the whole envelope into a direction enum (e.g.
    /// [`crate::AgentMessage`]) — the counterpart of
    /// [`Envelope::from_message`]. Fails on unknown `msg_type` (the caller
    /// decides whether that's a warning or an error).
    pub fn parse_message<M: DeserializeOwned>(&self) -> Result<M, serde_json::Error> {
        serde_json::from_value(serde_json::json!({
            "type": self.msg_type,
            "payload": self.payload,
        }))
    }
}
