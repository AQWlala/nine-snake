//! P0#11 regression test: the updater public key in `tauri.conf.json`
//! is a real Ed25519 public key, not the literal placeholder that
//! shipped with the previous commit.
//!
//! Tauri's updater requires the key to be exactly 32 raw bytes
//! (encoded as standard base64, no padding lines) so that
//! `ed25519-dalek::VerifyingKey::from_bytes` accepts it on the
//! client. We validate the byte length here at build time so a
//! future careless edit to `tauri.conf.json` is caught before
//! shipping a release.
//!
//! P0#01 (key rotation): the current pubkey must NOT be one of the
//! known-compromised keys. See `docs/SECURITY_KEY_ROTATION.md`.

use std::fs;
use std::path::PathBuf;

const PLACEHOLDER: &str = "REPLACE_WITH_RELEASE_SIGNING_PUBLIC_KEY";

/// Public keys that MUST NEVER be re-introduced into the build.
///
/// Index 0 is the v1.0.0 key whose private counterpart was committed
/// to the repository on 2026-06-21. Indices 1..=4 are placeholders
/// and well-known dev/test keys that have appeared in forks and
/// forum snippets. Adding a new entry requires a code review and a
/// doc update in `docs/SECURITY_KEY_ROTATION.md`.
const COMPROMISED_PUBKEYS: &[&str] = &[
    // v1.0.0 误提交 — 旧私钥 public counterpart
    "1F44kpaO8aqD+6pQBCUlNhCBuMJ5hnAFEFCf3GFNKJY=",
    // 历史 placeholder
    "REPLACE_WITH_RELEASE_SIGNING_PUBLIC_KEY",
    // 全 0x00 32 字节（无效，但常见于生成器 bug）
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    // 重复 "foobar" 模式（开发者常误用的 "test key"）
    "Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8=",
    // 0..9 / a..f 数字字面量（论坛贴过的"示例"）
    "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
];

fn read_tauri_conf() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let body = fs::read_to_string(&path).expect("read tauri.conf.json");
    serde_json::from_str(&body).expect("parse tauri.conf.json")
}

#[test]
fn updater_pubkey_is_not_placeholder() {
    let conf = read_tauri_conf();
    let pubkey = conf
        .pointer("/plugins/updater/pubkey")
        .and_then(|v| v.as_str())
        .expect("tauri.conf.json::plugins.updater.pubkey must be a string");
    assert_ne!(
        pubkey, PLACEHOLDER,
        "updater pubkey is still the placeholder string — auto-update will fail verification"
    );
    assert!(!pubkey.is_empty(), "updater pubkey is empty");
}

#[test]
fn updater_pubkey_is_valid_32_byte_ed25519_key() {
    let conf = read_tauri_conf();
    let pubkey = conf
        .pointer("/plugins/updater/pubkey")
        .and_then(|v| v.as_str())
        .expect("updater.pubkey missing");

    // Standard base64 decode (Tauri's `ed25519-dalek` integration
    // expects this exact alphabet).
    let decoded = base64_decode(pubkey);
    assert_eq!(
        decoded.len(),
        32,
        "updater pubkey must decode to 32 raw bytes (got {})",
        decoded.len()
    );

    // Ed25519 public keys are 32 bytes where the top bit of the
    // last byte is always 0 (canonical encoding, see RFC 8032
    // §5.1.2 step 3). We don't fail on a non-canonical key — Tauri
    // would still accept it — but we do sanity-check that the
    // key is not all-zero.
    assert!(
        decoded.iter().any(|b| *b != 0),
        "updater pubkey decodes to all-zero bytes (invalid)"
    );
}

/// P0#01: the live `tauri.conf.json::plugins.updater.pubkey` must
/// not be one of the 5 known-compromised keys. If this test fails,
/// someone has either reverted the v1.0.1 rotation or pasted one of
/// the placeholder values from a forum / blog post.
#[test]
fn updater_pubkey_is_not_in_compromised_list() {
    let conf = read_tauri_conf();
    let pubkey = conf
        .pointer("/plugins/updater/pubkey")
        .and_then(|v| v.as_str())
        .expect("updater.pubkey missing");

    for (i, bad) in COMPROMISED_PUBKEYS.iter().enumerate() {
        assert_ne!(
            pubkey, *bad,
            "updater pubkey matches the compromised entry at index {i} ({bad}) — \
             see docs/SECURITY_KEY_ROTATION.md and re-run \
             `python scripts/generate-updater-key.py`"
        );
    }
}

fn base64_decode(s: &str) -> Vec<u8> {
    // Tiny in-test base64 (standard alphabet) so the test does not
    // pull in the `base64` crate. Tauri's own `ed25519-dalek`
    // integration uses the same alphabet.
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
