//! P0#01 regression test: the v1.0.1 updater signing key rotation.
//!
//! Two invariants are checked here:
//!
//! 1. **`pubkey_in_tauri_conf_matches_keys_dir`** — the base64 value
//!    in `tauri.conf.json::plugins.updater.pubkey` must decode to
//!    the same 32 bytes as `keys/updater_public.b64`. If someone
//!    updates one but forgets the other, the in-app updater will
//!    reject every manifest the CI signs (or vice versa).
//!
//! 2. **`compromised_keys_list_does_not_contain_current_pubkey`** —
//!    the live pubkey is not one of the 5 known-compromised values
//!    from `updater_pubkey_test.rs::COMPROMISED_PUBKEYS`. This is
//!    a defence-in-depth check that lives in its own file so it
//!    is also discoverable by `cargo test -- key_rotation`.
//!
//! See `docs/SECURITY_KEY_ROTATION.md` for the full incident write-up.

use std::fs;
use std::path::PathBuf;

/// Mirror of the table in `updater_pubkey_test.rs`. Kept duplicated
/// (not `pub use`'d) so a regression test failure still produces a
/// self-contained, greppable error message in each test file.
const COMPROMISED_PUBKEYS: &[&str] = &[
    "1F44kpaO8aqD+6pQBCUlNhCBuMJ5hnAFEFCf3GFNKJY=",
    "REPLACE_WITH_RELEASE_SIGNING_PUBLIC_KEY",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    "Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8=",
    "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
];

fn read_pubkey_from_tauri_conf() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let body = fs::read_to_string(&path).expect("read tauri.conf.json");
    let conf: serde_json::Value = serde_json::from_str(&body).expect("parse tauri.conf.json");
    conf.pointer("/plugins/updater/pubkey")
        .and_then(|v| v.as_str())
        .expect("tauri.conf.json::plugins.updater.pubkey missing or not a string")
        .to_string()
}

fn read_pubkey_from_keys_dir() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("keys")
        .join("updater_public.b64");
    let body = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read keys/updater_public.b64 ({path:?}): {e}"));
    // Standard base64 (single line, with possible trailing newline).
    body.trim().to_string()
}

/// Tiny in-test base64 (standard alphabet). Mirrors the helper in
/// `updater_pubkey_test.rs` and `updater_pubkey_test` does not export
/// it. We deliberately re-implement it here so this file is
/// self-contained — no `mod` plumbing needed.
fn base64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in cleaned {
        if c == b'=' {
            break;
        }
        let v = val(c).unwrap_or_else(|| panic!("invalid base64 char: {c:?}"));
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    out
}

#[test]
fn pubkey_in_tauri_conf_matches_keys_dir() {
    let from_conf = read_pubkey_from_tauri_conf();
    let from_keys = read_pubkey_from_keys_dir();

    // String-level sanity: the two should be byte-equal (modulo
    // whitespace) once we trim.
    assert_eq!(
        from_conf.trim(),
        from_keys.trim(),
        "tauri.conf.json::plugins.updater.pubkey ({from_conf:?}) does not match \
         keys/updater_public.b64 ({from_keys:?}) — update one to match the other, \
         or re-run `python scripts/generate-updater-key.py` and update both"
    );

    // Byte-level sanity: both decode to exactly 32 raw bytes and
    // those bytes match. This catches the (rare) case where the
    // two files use different base64 encodings of the same key.
    let conf_bytes = base64_decode(&from_conf);
    let keys_bytes = base64_decode(&from_keys);
    assert_eq!(
        conf_bytes.len(),
        32,
        "tauri.conf.json pubkey does not decode to 32 bytes"
    );
    assert_eq!(
        keys_bytes.len(),
        32,
        "keys/updater_public.b64 does not decode to 32 bytes"
    );
    assert_eq!(
        conf_bytes, keys_bytes,
        "tauri.conf.json and keys/updater_public.b64 decode to different raw bytes"
    );
}

#[test]
fn compromised_keys_list_does_not_contain_current_pubkey() {
    let from_conf = read_pubkey_from_tauri_conf();
    let from_keys = read_pubkey_from_keys_dir();

    for (source, value) in [("tauri.conf.json", &from_conf), ("keys/", &from_keys)] {
        for (i, bad) in COMPROMISED_PUBKEYS.iter().enumerate() {
            assert_ne!(
                value.trim(),
                *bad,
                "{source} pubkey matches the compromised entry at index {i} ({bad}) — \
                 see docs/SECURITY_KEY_ROTATION.md"
            );
        }
    }
}

#[test]
fn no_private_key_files_in_keys_dir() {
    // The v1.0.0 incident was caused by `keys/updater_private.b64`
    // and `keys/updater_private_password.b64` being checked in.
    // This test fails the build if either file reappears, even
    // before any human review.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for forbidden in ["updater_private.b64", "updater_private_password.b64"] {
        let p = manifest.join("..").join("keys").join(forbidden);
        if p.exists() {
            let _body = fs::read_to_string(&p).unwrap_or_default();
            panic!(
                "forbidden private key file {p:?} exists in the working tree \
                 ({{body.len()}} bytes). Restore .gitignore's `keys/` entry and \
                 delete the file. See docs/SECURITY_KEY_ROTATION.md."
            );
        }
    }
}
