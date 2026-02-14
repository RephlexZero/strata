//! Authentication primitives for the Strata platform.
//!
//! - **Passwords**: Argon2id hashing and verification
//! - **JWT**: Ed25519-signed tokens for session auth
//! - **Device keys**: Ed25519 keypair generation for sender identity

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid password")]
    InvalidPassword,
    #[error("password hashing failed: {0}")]
    HashError(String),
    #[error("JWT error: {0}")]
    JwtError(#[from] jsonwebtoken::errors::Error),
    #[error("invalid device key")]
    InvalidKey,
}

// ── Password Hashing (Argon2id) ─────────────────────────────────────

/// Hash a password using Argon2id with a random salt.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    use argon2::{
        password_hash::{rand_core::OsRng as PhcOsRng, SaltString},
        Argon2, PasswordHasher,
    };

    let salt = SaltString::generate(&mut PhcOsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verify a password against an Argon2id hash.
pub fn verify_password(password: &str, hash: &str) -> Result<bool, AuthError> {
    use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};

    let parsed_hash = PasswordHash::new(hash).map_err(|e| AuthError::HashError(e.to_string()))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

// ── JWT (Ed25519-signed) ────────────────────────────────────────────

/// Claims embedded in a JWT token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — user ID (`usr_...`) or sender ID (`snd_...`).
    pub sub: String,
    /// Issuer — always "strata-control".
    pub iss: String,
    /// Expiration time (Unix timestamp).
    pub exp: i64,
    /// Issued-at time (Unix timestamp).
    pub iat: i64,
    /// Role: "admin", "operator", "viewer", or "sender".
    pub role: String,
    /// Owner user ID (for sender tokens, the user who owns this sender).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// JWT signing/verification context.
pub struct JwtContext {
    encoding_key: jsonwebtoken::EncodingKey,
    decoding_key: jsonwebtoken::DecodingKey,
}

impl JwtContext {
    /// Create a JWT context from an Ed25519 private key (32 bytes, base64-encoded).
    pub fn from_ed25519_seed(seed_b64: &str) -> Result<Self, AuthError> {
        let seed_bytes = BASE64.decode(seed_b64).map_err(|_| AuthError::InvalidKey)?;
        if seed_bytes.len() != 32 {
            return Err(AuthError::InvalidKey);
        }

        let signing_key = SigningKey::from_bytes(
            seed_bytes
                .as_slice()
                .try_into()
                .map_err(|_| AuthError::InvalidKey)?,
        );
        let verifying_key = signing_key.verifying_key();

        // jsonwebtoken expects PKCS8v2 DER encoding for Ed25519.
        // PKCS8v2 wraps the 32-byte seed as:
        //   SEQUENCE {
        //     INTEGER 0  (version)
        //     SEQUENCE { OID 1.3.101.112 }  (Ed25519 algorithm)
        //     OCTET STRING { OCTET STRING { <32 seed bytes> } }
        //     [1] { BIT STRING { <32 public key bytes> } }  (optional)
        //   }
        let pkcs8_prefix: &[u8] = &[
            0x30, 0x2e, // SEQUENCE, 46 bytes
            0x02, 0x01, 0x00, // INTEGER 0 (version)
            0x30, 0x05, // SEQUENCE, 5 bytes
            0x06, 0x03, 0x2b, 0x65, 0x70, // OID 1.3.101.112 (Ed25519)
            0x04, 0x22, // OCTET STRING, 34 bytes
            0x04, 0x20, // OCTET STRING, 32 bytes (the seed)
        ];
        let mut pkcs8_der = Vec::with_capacity(48);
        pkcs8_der.extend_from_slice(pkcs8_prefix);
        pkcs8_der.extend_from_slice(&seed_bytes);

        let encoding_key = jsonwebtoken::EncodingKey::from_ed_der(&pkcs8_der);

        // For the public key, jsonwebtoken expects raw 32-byte Ed25519 public key
        let decoding_key = jsonwebtoken::DecodingKey::from_ed_der(verifying_key.as_bytes());

        Ok(Self {
            encoding_key,
            decoding_key,
        })
    }

    /// Generate a new random Ed25519 seed and create a JWT context.
    /// Returns `(context, seed_b64)` — store the seed securely.
    pub fn generate() -> (Self, String) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let seed_b64 = BASE64.encode(signing_key.to_bytes());
        let ctx =
            Self::from_ed25519_seed(&seed_b64).expect("freshly generated key should be valid");
        (ctx, seed_b64)
    }

    /// Create and sign a JWT token.
    pub fn create_token(&self, claims: &Claims) -> Result<String, AuthError> {
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        let token = jsonwebtoken::encode(&header, claims, &self.encoding_key)?;
        Ok(token)
    }

    /// Validate and decode a JWT token.
    pub fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
        validation.set_issuer(&["strata-control"]);
        validation.validate_exp = true;

        let token_data = jsonwebtoken::decode::<Claims>(token, &self.decoding_key, &validation)?;
        Ok(token_data.claims)
    }
}

// ── Device Keys ─────────────────────────────────────────────────────

/// Generate a new Ed25519 keypair for a sender device.
/// Returns `(private_key_b64, public_key_b64)`.
pub fn generate_device_keypair() -> (String, String) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key: VerifyingKey = signing_key.verifying_key();

    let private_b64 = BASE64.encode(signing_key.to_bytes());
    let public_b64 = BASE64.encode(verifying_key.to_bytes());

    (private_b64, public_b64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn password_hash_and_verify() {
        let hash = hash_password("test-password-123").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("test-password-123", &hash).unwrap());
        assert!(!verify_password("wrong-password", &hash).unwrap());
    }

    #[test]
    fn jwt_create_and_verify() {
        let (ctx, _seed) = JwtContext::generate();

        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: "usr_test123".into(),
            iss: "strata-control".into(),
            exp: now + 3600,
            iat: now,
            role: "admin".into(),
            owner: None,
        };

        let token = ctx.create_token(&claims).unwrap();
        let recovered = ctx.verify_token(&token).unwrap();

        assert_eq!(recovered.sub, "usr_test123");
        assert_eq!(recovered.role, "admin");
    }

    #[test]
    fn jwt_expired_token_rejected() {
        let (ctx, _seed) = JwtContext::generate();

        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: "usr_test".into(),
            iss: "strata-control".into(),
            exp: now - 100, // expired
            iat: now - 200,
            role: "viewer".into(),
            owner: None,
        };

        let token = ctx.create_token(&claims).unwrap();
        assert!(ctx.verify_token(&token).is_err());
    }

    #[test]
    fn jwt_wrong_key_rejected() {
        let (ctx1, _) = JwtContext::generate();
        let (ctx2, _) = JwtContext::generate();

        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: "usr_test".into(),
            iss: "strata-control".into(),
            exp: now + 3600,
            iat: now,
            role: "admin".into(),
            owner: None,
        };

        let token = ctx1.create_token(&claims).unwrap();
        // Different key should fail to verify
        assert!(ctx2.verify_token(&token).is_err());
    }

    #[test]
    fn device_keypair_generation() {
        let (private_key, public_key) = generate_device_keypair();
        // Both should be valid base64
        assert!(BASE64.decode(&private_key).is_ok());
        assert!(BASE64.decode(&public_key).is_ok());
        // Private key = 32 bytes, public key = 32 bytes
        assert_eq!(BASE64.decode(&private_key).unwrap().len(), 32);
        assert_eq!(BASE64.decode(&public_key).unwrap().len(), 32);
    }

    #[test]
    fn jwt_sender_token_with_owner() {
        let (ctx, _seed) = JwtContext::generate();

        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: "snd_device001".into(),
            iss: "strata-control".into(),
            exp: now + 3600,
            iat: now,
            role: "sender".into(),
            owner: Some("usr_owner123".into()),
        };

        let token = ctx.create_token(&claims).unwrap();
        let recovered = ctx.verify_token(&token).unwrap();

        assert_eq!(recovered.sub, "snd_device001");
        assert_eq!(recovered.role, "sender");
        assert_eq!(recovered.owner.as_deref(), Some("usr_owner123"));
    }
}
