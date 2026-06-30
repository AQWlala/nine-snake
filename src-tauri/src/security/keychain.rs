//! OS keychain integration (v1.0.1 P0#12).
//!
//! v1.0 stored the user's API keys in `localStorage` via
//! `Settings.tsx`, which meant the key was readable by any
//! JavaScript that ran in the WebView (including any malicious
//! skill that got XSS via a poisoned memory).  v1.0.1 moves
//! all secrets into the OS keychain:
//!
//! * **macOS** — Keychain (via the `security` CLI under the hood,
//!   through the `keyring` crate's `apple-native` feature).
//! * **Windows** — Credential Vault (via the `keyring` crate's
//!   `windows-native` feature; backed by the wincred API).
//! * **Linux** — Secret Service (via the `keyring` crate's
//!   `sync-secret-service` feature, backed by libsecret/gnome-
//!   keyring/kwallet over D-Bus).
//!
//! The Rust-side API is intentionally tiny: `set`, `get`,
//! `delete`.  All three return `anyhow::Result` so the caller
//! can use the same error-mapping pipeline as the rest of the
//! app.  `get` returns `Ok(None)` when the entry does not
//! exist (a normal "not configured yet" outcome), and `Err` only
//! for genuine OS errors (e.g. the user denied keychain access).
//!
//! All three entry points use a single, well-known service
//! name (`SERVICE`) and a per-purpose user name (e.g.
//! `"openai_api_key"`).  Callers compose the user name with the
//! keyring crate's `Entry::new` constructor.

use anyhow::{Context, Result};
use keyring::Entry;
use tracing::{debug, warn};

/// Service name used for every entry written by nine-snake.  The
/// OS keychain groups entries by `(service, user)`, so this
/// string shows up in the user's "Passwords" list.  It is also
/// the search key if the user wants to revoke the app's
/// keychain access from the OS UI.
pub const SERVICE: &str = "nine-snake";

/// OpenAI / OpenAI-compatible API key.  Used by
/// `commands::set_api_key` / `get_api_key` / `delete_api_key`.
pub const KEY_API_KEY: &str = "openai_api_key";

/// Stores `value` under `key` in the OS keychain.
///
/// v1.0.1 P0#12: replaces the v1.0 `localStorage.setItem` call
/// in `Settings.tsx`.  The JavaScript side now calls
/// `set_api_key` over the Tauri IPC, never the WebView's
/// persistent storage.
pub fn set(key: &str, value: &str) -> Result<()> {
    let entry =
        Entry::new(SERVICE, key).with_context(|| format!("opening keychain entry for {key}"))?;
    entry
        .set_password(value)
        .with_context(|| format!("writing keychain entry for {key}"))?;
    debug!(target: "nine_snake.security", key, "keychain set");
    Ok(())
}

/// Reads the value stored under `key`, or `None` if the entry
/// does not exist.  Returns `Err` only for OS-level errors
/// (e.g. the keychain is locked, the user denied access).
pub fn get(key: &str) -> Result<Option<String>> {
    let entry =
        Entry::new(SERVICE, key).with_context(|| format!("opening keychain entry for {key}"))?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => {
            warn!(target: "nine_snake.security", key, error = ?e, "keychain read failed");
            Err(anyhow::anyhow!("keychain get_password: {e}"))
        }
    }
}

/// Removes the entry for `key`.  Idempotent: deleting a missing
/// entry is treated as success so the front-end's "reset" button
/// doesn't have to special-case "not configured".
pub fn delete(key: &str) -> Result<()> {
    let entry =
        Entry::new(SERVICE, key).with_context(|| format!("opening keychain entry for {key}"))?;
    match entry.delete_credential() {
        Ok(()) => {
            debug!(target: "nine_snake.security", key, "keychain delete");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            // Already gone — that's a successful no-op.
            debug!(target: "nine_snake.security", key, "keychain delete: already absent");
            Ok(())
        }
        Err(e) => {
            warn!(target: "nine_snake.security", key, error = ?e, "keychain delete failed");
            Err(anyhow::anyhow!("keychain delete_credential: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1.0.1 P0#12: round-trip the canonical API key.  The
    /// test runs on whatever backend the host has (Keychain /
    /// Credential Vault / Secret Service).  If the backend is
    /// unavailable (e.g. headless CI without a Secret Service
    /// daemon), the test is a soft pass and prints a warning.
    #[test]
    fn keychain_roundtrip() {
        let key = "nine_snake_test_key_roundtrip";
        // Clean up any stale entry from a prior failed run.
        let _ = delete(key);

        match set(key, "secret-value-XYZ") {
            Ok(()) => {}
            Err(e) => {
                eprintln!("keychain not available on this host: {e}; skipping");
                return;
            }
        }

        let got = get(key).expect("get");
        assert_eq!(got.as_deref(), Some("secret-value-XYZ"));

        delete(key).expect("delete");
        let after = get(key).expect("get after delete");
        assert_eq!(after, None, "entry must be gone after delete");
    }

    #[test]
    fn get_missing_returns_none_not_err() {
        // The key `nine_snake_definitely_missing_<pid>` is
        // extremely unlikely to exist.
        let key = "nine_snake_definitely_missing_zzz";
        // Defensive: clean any leftover.
        let _ = delete(key);
        let got = get(key).expect("get should not error on missing");
        assert_eq!(got, None);
    }
}
