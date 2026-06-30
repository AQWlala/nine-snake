//! Integration tests for the v0.5 cross-device sync module.
//!
//! Covers the E2EE primitives (X25519 + HKDF + AES-256-GCM) and
//! the local transport (envelope write / read / ack).
//!
//! The test is end-to-end: two simulated devices each generate an
//! identity, derive a shared session key, encrypt a message, drop
//! it into the inbox, and the receiver decrypts it.

use nine_snake_lib::sync::{
    recv_all_unsealed, send_sealed, E2eeIdentity, EncryptedEnvelope, LocalTransport, Pair,
};

#[test]
fn x25519_identities_have_32_byte_keys() {
    let id = E2eeIdentity::generate();
    let pk = id.public_key_b64();
    // secret_bytes() is pub(crate), not accessible from integration tests
    use base64::Engine as _;
    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(&pk)
        .unwrap();
    assert_eq!(pk_bytes.len(), 32);
}

#[test]
fn pair_establishes_derivable_session() {
    let alice = E2eeIdentity::generate();
    let bob = E2eeIdentity::generate();
    let pair_a = Pair::new(alice.clone(), &bob.public_key_b64()).expect("pair a");
    let pair_b = Pair::new(bob.clone(), &alice.public_key_b64()).expect("pair b");
    // The two sides should agree on the same fingerprint (which
    // depends only on the public keys, not the session key).
    assert_eq!(pair_a.fingerprint, pair_b.fingerprint);
}

#[test]
fn encrypt_decrypt_across_two_identities() {
    let alice = E2eeIdentity::generate();
    let bob = E2eeIdentity::generate();
    let pair_a = Pair::new(alice.clone(), &bob.public_key_b64()).expect("pair a");
    let pair_b = Pair::new(bob, &alice.public_key_b64()).expect("pair b");

    let env = pair_a.encrypt(b"hello bob from alice").expect("encrypt");
    let pt = pair_b.decrypt(&env).expect("decrypt");
    assert_eq!(pt, b"hello bob from alice");
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let id = E2eeIdentity::generate();
    let pair = Pair::new(id.clone(), &id.public_key_b64()).expect("self pair");
    let mut env = pair.encrypt(b"secret").expect("encrypt");
    let last = env.ciphertext.len() - 1;
    env.ciphertext[last] ^= 0xff;
    let err = pair.decrypt(&env).expect_err("must reject tampering");
    assert!(err.to_string().contains("AES-GCM"));
}

#[test]
fn local_transport_send_recv_ack_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let transport = LocalTransport::new(dir.path()).expect("transport");
    let local = E2eeIdentity::generate();
    let pair = Pair::new(local.clone(), &local.public_key_b64()).expect("pair");

    let env_id = send_sealed(&transport, &pair, b"payload").expect("send");
    let unsealed = recv_all_unsealed(&transport, &pair).expect("recv");
    assert_eq!(unsealed.len(), 1);
    assert_eq!(unsealed[0].0, env_id);
    assert_eq!(unsealed[0].1, b"payload");

    assert!(transport.ack(&env_id).expect("ack"));
    let after = recv_all_unsealed(&transport, &pair).expect("recv after ack");
    assert!(after.is_empty());
}

#[test]
fn wire_format_round_trip() {
    let id = E2eeIdentity::generate();
    let pair = Pair::new(id.clone(), &id.public_key_b64()).expect("pair");
    let env = pair.encrypt(b"wire-format").expect("encrypt");
    let s = env.to_b64_json().expect("to_b64_json");
    let back = EncryptedEnvelope::from_b64_json(&s).expect("from_b64_json");
    let pt = pair.decrypt(&back).expect("decrypt");
    assert_eq!(pt, b"wire-format");
}

#[test]
fn envelope_id_traversal_is_sanitised() {
    let dir = tempfile::tempdir().expect("dir");
    let transport = LocalTransport::new(dir.path()).expect("transport");
    let evil = "../../../etc/passwd";
    let result = transport.ack(evil);
    // Ack should return Ok(false) because the file does not exist
    // after the path is sanitised.
    assert!(result.is_ok());
    assert!(!result.unwrap());
}
