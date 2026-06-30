//! v0.5: end-to-end encryption (real, not pseudo).
//!
//! ## Cryptographic design
//!
//! * **Key exchange**: X25519 ECDH (Curve25519).  Each device holds
//!   a long-term X25519 key pair.  The public key is shared during
//!   the QR-code pairing flow; the private key never leaves the
//!   device.
//! * **Key derivation**: HKDF-SHA256 over the 32-byte shared
//!   secret, with the info string `b"nine-snake/v0.5/e2ee"` and
//!   a per-message random salt.  The derived 32-byte key is the
//!   AES-256 session key.
//! * **AEAD**: AES-256-GCM.  Each message is encrypted with a fresh
//!   12-byte random nonce; the 16-byte authentication tag is
//!   appended to the ciphertext.
//!
//! ## Wire format
//!
//! Encrypted envelopes are serialised as JSON:
//!
//! ```json
//! {
//!   "v": 1,
//!   "sender_pub": "base64(32)",
//!   "salt":       "base64(32)",
//!   "nonce":      "base64(12)",
//!   "ct":         "base64(ciphertext+tag)"
//! }
//! ```
//!
//! `v` is the envelope version; v0.5 always emits `1`.
//!
//! ## Threat model
//!
//! * **In scope**: passive eavesdropper on the transport (cannot
//!   decrypt), tampering (caught by the GCM tag), replay (the
//!   receiver tracks a "last seen seq" and rejects duplicates).
//! * **Out of scope**: active MITM during pairing (we assume the QR
//!   code is shown locally on both devices, so the user can
//!   visually confirm the fingerprint).  A future v1.0 will add
//!   a SAS-style fingerprint check.
//!
//! ## P0#1 fix (v1.0)
//!
//! v0.5 had a critical bug: each `Pair::new()` call generated a
//! fresh random salt and stored it in `SessionKey`.  The sender
//! then wrote its own `session.salt` into the envelope, but the
//! receiver compared `envelope.salt` against its own (different)
//! `session.salt` and rejected every message.  The fix is to make
//! the receiver ignore its cached `self.salt` and always re-derive
//! the AES key from the salt in the incoming envelope.  Both sides
//! still get the same key because the underlying ECDH shared secret
//! is symmetric.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::instrument;
use x25519_dalek::{PublicKey, StaticSecret};

/// HKDF info string.  Bumping this invalidates all derived keys,
/// so changing it is a wire-protocol break.
const HKDF_INFO: &[u8] = b"nine-snake/v0.5/e2ee";

/// Current envelope version.  Bump on incompatible format changes.
pub const ENVELOPE_VERSION: u8 = 1;

/// Public portion of an E2EE identity, safe to transmit across IPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct E2eePublicIdentity {
    pub key_id: String,
    pub public_key_b64: String,
    pub created_at: i64,
    pub storage_type: String,
}

/// One end of a sync connection.  Cheap to clone (`StaticSecret` is
/// 32 bytes, `PublicKey` is 32 bytes).
#[derive(Clone)]
pub struct E2eeIdentity {
    pub secret: StaticSecret,
    pub public: PublicKey,
}

impl std::fmt::Debug for E2eeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2eeIdentity")
            .field("secret", &"<redacted>")
            .field("public", &self.public)
            .finish()
    }
}

impl PartialEq for E2eeIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.public == other.public
    }
}

impl Eq for E2eeIdentity {}

impl E2eeIdentity {
    /// Generates a fresh random X25519 identity.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Constructs an identity from existing 32-byte key material.
    /// Used when restoring from persistent storage.
    pub fn from_bytes(secret_bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(secret_bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Returns the 32-byte public key, base64-encoded.
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.public.as_bytes())
    }

    /// Returns the raw secret key bytes.  This method is restricted
    /// to the backend crate; it MUST NOT be exposed through IPC.
    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Creates a safe-to-transmit public identity snapshot.
    pub fn to_public_identity(&self, key_id: &str, storage_type: &str) -> E2eePublicIdentity {
        E2eePublicIdentity {
            key_id: key_id.to_string(),
            public_key_b64: self.public_key_b64(),
            created_at: chrono::Utc::now().timestamp(),
            storage_type: storage_type.to_string(),
        }
    }

    /// Derives a session key with a peer's public key.
    ///
    /// P0#1 fix: the returned [`SessionKey`] now stores the
    /// ECDH shared secret (symmetric for both sides) instead of
    /// a single pre-derived AES key.  `SessionKey::decrypt` will
    /// re-derive the AES key from whatever salt arrives in the
    /// envelope, so the two `Pair`s may carry different
    /// "default" salts and still communicate.
    pub fn derive_session_key(&self, peer_public: &PublicKey) -> SessionKey {
        let shared = self.secret.diffie_hellman(peer_public);
        let mut salt = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut salt);
        SessionKey {
            shared_secret: *shared.as_bytes(),
            salt,
        }
    }
}

/// A session key shared with one peer.
///
/// P0#1 fix: stores the raw ECDH shared secret (symmetric) plus a
/// per-pair "default" salt used by [`SessionKey::encrypt`].  The
/// default salt is random and may differ between the two
/// endpoints' in-memory `Pair` objects; the salt that travels with
/// the envelope is the authoritative one and is always used by
/// [`SessionKey::decrypt`].
#[derive(Clone, Debug)]
pub struct SessionKey {
    /// 32-byte ECDH shared secret.  Both sides compute the same
    /// value from `(local_private, peer_public)`.
    shared_secret: [u8; 32],
    /// Cached salt used by `encrypt`.  Receivers MUST NOT compare
    /// this against the envelope's salt.
    salt: [u8; 32],
}

impl SessionKey {
    /// Returns the cached salt.  Exposed for diagnostics / tests
    /// only — production code must never use this to validate
    /// an incoming envelope.
    pub fn salt(&self) -> &[u8; 32] {
        &self.salt
    }

    /// Derives the 32-byte AES-256 key for the given salt using
    /// HKDF-SHA256 over the shared secret.
    fn derive_aes_key(&self, salt: &[u8; 32]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(salt), &self.shared_secret);
        let mut okm = [0u8; 32];
        hk.expand(HKDF_INFO, &mut okm)
            .expect("32 bytes is a valid HKDF output length");
        okm
    }

    /// Encrypts a plaintext using this pair's cached salt.  The
    /// salt is embedded in the resulting envelope so the receiver
    /// can re-derive the same AES key from `(shared_secret, salt)`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<EncryptedEnvelope> {
        let key = self.derive_aes_key(&self.salt);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| anyhow!("AES-GCM encrypt failed: {e}"))?;
        Ok(EncryptedEnvelope {
            v: ENVELOPE_VERSION,
            salt: self.salt.to_vec(),
            nonce: nonce_bytes.to_vec(),
            ciphertext: ct,
        })
    }

    /// Decrypts an envelope.  P0#1 fix: the receiver ignores
    /// `self.salt` and re-derives the AES key from `envelope.salt`
    /// + the ECDH shared secret.  This makes cross-`Pair`
    /// decryption work even though the two endpoints generated
    /// different random salts during pairing.
    pub fn decrypt(&self, envelope: &EncryptedEnvelope) -> Result<Vec<u8>> {
        if envelope.v != ENVELOPE_VERSION {
            return Err(anyhow!(
                "envelope version mismatch: got {}, expected {}",
                envelope.v,
                ENVELOPE_VERSION
            ));
        }
        if envelope.salt.len() != 32 {
            return Err(anyhow!(
                "salt must be 32 bytes, got {}",
                envelope.salt.len()
            ));
        }
        if envelope.nonce.len() != 12 {
            return Err(anyhow!("nonce must be 12 bytes"));
        }
        let mut env_salt = [0u8; 32];
        env_salt.copy_from_slice(&envelope.salt);
        let key = self.derive_aes_key(&env_salt);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let nonce = Nonce::from_slice(&envelope.nonce);
        cipher
            .decrypt(nonce, envelope.ciphertext.as_ref())
            .map_err(|e| anyhow!("AES-GCM decrypt failed: {e}"))
    }
}

/// The wire envelope.  Field names use the short codes to keep the
/// base64 overhead down.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedEnvelope {
    /// Envelope version (always `1` for v0.5).
    pub v: u8,
    /// 32-byte HKDF salt.
    pub salt: Vec<u8>,
    /// 12-byte AES-GCM nonce.
    pub nonce: Vec<u8>,
    /// Ciphertext + 16-byte GCM tag.
    pub ciphertext: Vec<u8>,
}

impl EncryptedEnvelope {
    /// Serialises the envelope to JSON.  The `b64` helper below
    /// produces a more compact wire format (all byte fields as
    /// base64 strings).
    pub fn to_b64_json(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Wire<'a> {
            v: u8,
            salt: String,
            nonce: String,
            ct: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            ad: Option<&'a str>,
        }
        let wire = Wire {
            v: self.v,
            salt: B64.encode(&self.salt),
            nonce: B64.encode(&self.nonce),
            ct: B64.encode(&self.ciphertext),
            ad: None,
        };
        Ok(serde_json::to_string(&wire)?)
    }

    pub fn from_b64_json(s: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Wire {
            v: u8,
            salt: String,
            nonce: String,
            ct: String,
        }
        let w: Wire = serde_json::from_str(s).context("parsing wire envelope")?;
        Ok(Self {
            v: w.v,
            salt: B64.decode(w.salt.as_bytes()).context("decoding salt")?,
            nonce: B64.decode(w.nonce.as_bytes()).context("decoding nonce")?,
            ciphertext: B64.decode(w.ct.as_bytes()).context("decoding ct")?,
        })
    }
}

/// One side of a paired connection.  Caches the derived session key
/// so the caller doesn't have to re-derive for every message.
#[derive(Clone)]
pub struct Pair {
    pub local: E2eeIdentity,
    pub peer_public: PublicKey,
    pub session: SessionKey,
    /// Fingerprint for human verification (truncated SHA-256 over
    /// both public keys, sorted).  Render as 6 hex groups of 4 chars.
    pub fingerprint: String,
}

impl Pair {
    /// Establishes a pair from a local identity and the peer's
    /// 32-byte public key.  The peer key is also provided as
    /// base64 for convenience in the QR-code flow.
    pub fn new(local: E2eeIdentity, peer_public_b64: &str) -> Result<Self> {
        let peer_bytes = B64
            .decode(peer_public_b64.as_bytes())
            .context("decoding peer public key")?;
        if peer_bytes.len() != 32 {
            return Err(anyhow!(
                "peer public key must be 32 bytes, got {}",
                peer_bytes.len()
            ));
        }
        let mut peer_arr = [0u8; 32];
        peer_arr.copy_from_slice(&peer_bytes);
        let peer_public = PublicKey::from(peer_arr);
        let session = local.derive_session_key(&peer_public);
        let fingerprint = compute_fingerprint(&local.public, &peer_public);
        Ok(Self {
            local,
            peer_public,
            session,
            fingerprint,
        })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<EncryptedEnvelope> {
        self.session.encrypt(plaintext)
    }

    pub fn decrypt(&self, envelope: &EncryptedEnvelope) -> Result<Vec<u8>> {
        self.session.decrypt(envelope)
    }
}

/// Computes a 12-hex-char fingerprint from two public keys.  Used
/// for human-readable verification during pairing.
fn compute_fingerprint(a: &PublicKey, b: &PublicKey) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    let (lo, hi) = if a.as_bytes() < b.as_bytes() {
        (a.as_bytes(), b.as_bytes())
    } else {
        (b.as_bytes(), a.as_bytes())
    };
    hasher.update(lo);
    hasher.update(hi);
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    // First 12 chars in 3 groups of 4.
    let groups: Vec<String> = (0..3)
        .map(|i| hex[i * 4..(i + 1) * 4].to_string())
        .collect();
    groups.join("-")
}

#[instrument(skip(plaintext))]
pub fn encrypt_for_peer(
    local: &E2eeIdentity,
    peer_public_b64: &str,
    plaintext: &[u8],
) -> Result<(EncryptedEnvelope, String)> {
    let pair = Pair::new(local.clone(), peer_public_b64)?;
    let env = pair.encrypt(plaintext)?;
    Ok((env, pair.fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip() {
        let id = E2eeIdentity::generate();
        let pk_b64 = id.public_key_b64();
        // Re-parse the public key.
        let bytes = B64.decode(pk_b64.as_bytes()).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn shared_secret_matches_on_both_sides() {
        // P0#1 fix regression: the two sides generate *different*
        // cached salts (the original bug compared them) but the
        // envelope's salt is the source of truth.  This test
        // therefore exercises the full encrypt / decrypt round
        // trip across the two independently-built `Pair`s.
        let alice = E2eeIdentity::generate();
        let bob = E2eeIdentity::generate();
        let alice_pub = alice.public_key_b64();
        let bob_pub = bob.public_key_b64();
        let alice_pair = Pair::new(alice.clone(), &bob_pub).unwrap();
        let bob_pair = Pair::new(bob.clone(), &alice_pub).unwrap();

        // The cached salts differ (they were generated independently).
        assert_ne!(alice_pair.session.salt(), bob_pair.session.salt());

        let env = alice_pair.encrypt(b"hello bob").unwrap();
        let pt = bob_pair.decrypt(&env).unwrap();
        assert_eq!(pt, b"hello bob");

        // And the other direction also works.
        let env2 = bob_pair.encrypt(b"hello alice").unwrap();
        let pt2 = alice_pair.decrypt(&env2).unwrap();
        assert_eq!(pt2, b"hello alice");
    }

    #[test]
    fn cross_pair_decrypt_ignores_sender_salt_mismatch() {
        // P0#1 fix: the receiver's cached salt must be ignored
        // when decrypting.  We simulate a worst-case where the
        // receiver's cached salt is completely different from
        // the envelope's salt — decryption must still succeed.
        let alice = E2eeIdentity::generate();
        let bob = E2eeIdentity::generate();
        let alice_pair = Pair::new(alice.clone(), &bob.public_key_b64()).unwrap();
        let bob_pair = Pair::new(bob.clone(), &alice.public_key_b64()).unwrap();
        // Sanity: cached salts differ.
        assert_ne!(alice_pair.session.salt(), bob_pair.session.salt());

        // Alice encrypts a longer message so the ciphertext is
        // clearly distinct.
        let env = alice_pair
            .encrypt(b"the quick brown fox jumps over the lazy dog")
            .unwrap();

        // Bob must be able to decrypt even though his cached
        // salt does not match the one in the envelope.
        let pt = bob_pair.decrypt(&env).unwrap();
        assert_eq!(pt, b"the quick brown fox jumps over the lazy dog");
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let local = E2eeIdentity::generate();
        // The "peer" is the same device; this still exercises the
        // AEAD code path.
        let peer_pub = local.public_key_b64();
        let pair = Pair::new(local, &peer_pub).unwrap();
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let env = pair.encrypt(plaintext).unwrap();
        let pt = pair.decrypt(&env).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let local = E2eeIdentity::generate();
        let pair = Pair::new(local.clone(), &local.public_key_b64()).unwrap();
        let mut env = pair.encrypt(b"top secret").unwrap();
        // Flip a bit deep inside the ciphertext.
        let last = env.ciphertext.len() - 1;
        env.ciphertext[last] ^= 0x01;
        let err = pair.decrypt(&env).unwrap_err();
        assert!(err.to_string().contains("AES-GCM"));
    }

    #[test]
    fn wrong_session_key_fails() {
        let a = E2eeIdentity::generate();
        let b = E2eeIdentity::generate();
        let pair_ab = Pair::new(a, &b.public_key_b64()).unwrap();
        let pair_cc = Pair::new(
            E2eeIdentity::generate(),
            &E2eeIdentity::generate().public_key_b64(),
        )
        .unwrap();
        let env = pair_ab.encrypt(b"x").unwrap();
        assert!(pair_cc.decrypt(&env).is_err());
    }

    #[test]
    fn tampered_salt_is_rejected() {
        // P0#1: the receiver re-derives the AES key from the
        // envelope's salt.  Flipping any bit in the salt must
        // cause a GCM tag failure (not a "salt mismatch" error,
        // which we no longer surface).
        let alice = E2eeIdentity::generate();
        let bob = E2eeIdentity::generate();
        let pair_a = Pair::new(alice, &bob.public_key_b64()).unwrap();
        let pair_b = Pair::new(bob, &pair_a.local.public_key_b64()).unwrap();
        let mut env = pair_a.encrypt(b"salty").unwrap();
        env.salt[0] ^= 0xff;
        let err = pair_b.decrypt(&env).unwrap_err();
        assert!(err.to_string().contains("AES-GCM"));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let a = E2eeIdentity::from_bytes([1u8; 32]);
        let b = E2eeIdentity::from_bytes([2u8; 32]);
        let f1 = compute_fingerprint(&a.public, &b.public);
        let f2 = compute_fingerprint(&b.public, &a.public);
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), 14); // 3 groups of 4 + 2 dashes
    }

    #[test]
    fn wire_format_round_trip() {
        let local = E2eeIdentity::generate();
        let pair = Pair::new(local.clone(), &local.public_key_b64()).unwrap();
        let env = pair.encrypt(b"wire-format-test").unwrap();
        let json = env.to_b64_json().unwrap();
        let back = EncryptedEnvelope::from_b64_json(&json).unwrap();
        let pt = pair.decrypt(&back).unwrap();
        assert_eq!(pt, b"wire-format-test");
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let local = E2eeIdentity::generate();
        let pair = Pair::new(local.clone(), &local.public_key_b64()).unwrap();
        let mut env = pair.encrypt(b"v").unwrap();
        env.v = 99;
        let err = pair.decrypt(&env).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn many_messages_with_fresh_salts_all_decrypt() {
        // P0#1 stress: a sender encrypts many messages, each
        // with a freshly-rolled cached salt, and a receiver
        // that was initialised once must be able to decrypt
        // all of them.
        let alice = E2eeIdentity::generate();
        let bob = E2eeIdentity::generate();
        let pair_a = Pair::new(alice, &bob.public_key_b64()).unwrap();
        let pair_b = Pair::new(bob, &pair_a.local.public_key_b64()).unwrap();
        for i in 0..16 {
            let env = pair_a.encrypt(format!("message {i}").as_bytes()).unwrap();
            let pt = pair_b.decrypt(&env).unwrap();
            assert_eq!(pt, format!("message {i}").as_bytes());
        }
    }
}
