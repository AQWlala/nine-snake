//! v1.1: KeyVault — secure private key storage abstraction.
//!
//! Private keys (E2EE, API keys, etc.) must never be transmitted
//! across the Tauri IPC boundary.  The `KeyVault` provides a
//! backend-agnostic interface for storing and retrieving secrets:
//!
//! 1. **OS Keychain** (preferred) — uses the platform's native
//!    credential store (macOS Keychain, Windows Credential Vault,
//!    Linux Secret Service / gnome-keyring).
//! 2. **Encrypted file fallback** — when the OS keychain is
//!    unavailable, secrets are encrypted with AES-256-GCM using
//!    a machine-specific key derived from system properties, and
//!    stored under `<data_dir>/e2ee_keys/<key_id>.enc`.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

use crate::security::keychain;

const VAULT_DIR_NAME: &str = "e2ee_keys";

pub struct KeyVault {
    data_dir: PathBuf,
}

impl KeyVault {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    pub async fn store(&self, key_id: &str, secret: &str) -> Result<()> {
        match keychain::set(&format!("nine_snake.vault.{key_id}"), secret) {
            Ok(()) => {
                info!(target: "nine_snake.vault", key_id, "stored in OS keychain");
                Ok(())
            }
            Err(e) => {
                warn!(target: "nine_snake.vault", key_id, error = ?e, "OS keychain unavailable; falling back to encrypted file");
                self.store_file(key_id, secret).await
            }
        }
    }

    pub async fn retrieve(&self, key_id: &str) -> Result<Option<String>> {
        match keychain::get(&format!("nine_snake.vault.{key_id}")) {
            Ok(Some(val)) => {
                info!(target: "nine_snake.vault", key_id, "retrieved from OS keychain");
                Ok(Some(val))
            }
            Ok(None) => self.retrieve_file(key_id).await,
            Err(e) => {
                warn!(target: "nine_snake.vault", key_id, error = ?e, "OS keychain read failed; trying encrypted file fallback");
                self.retrieve_file(key_id).await
            }
        }
    }

    pub async fn delete(&self, key_id: &str) -> Result<()> {
        let _ = keychain::delete(&format!("nine_snake.vault.{key_id}"));
        let path = self.file_path(key_id);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("deleting key file for {key_id}"))?;
        }
        Ok(())
    }

    fn file_path(&self, key_id: &str) -> PathBuf {
        self.data_dir
            .join(VAULT_DIR_NAME)
            .join(format!("{key_id}.enc"))
    }

    async fn store_file(&self, key_id: &str, secret: &str) -> Result<()> {
        let dir = self.data_dir.join(VAULT_DIR_NAME);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating vault dir: {}", dir.display()))?;

        let sealed = Self::seal(secret)?;
        let path = self.file_path(key_id);
        std::fs::write(&path, &sealed).with_context(|| format!("writing key file for {key_id}"))?;
        info!(target: "nine_snake.vault", key_id, "stored in encrypted file fallback");
        Ok(())
    }

    async fn retrieve_file(&self, key_id: &str) -> Result<Option<String>> {
        let path = self.file_path(key_id);
        if !path.exists() {
            return Ok(None);
        }
        let sealed =
            std::fs::read(&path).with_context(|| format!("reading key file for {key_id}"))?;
        let secret = Self::unseal(&sealed)?;
        Ok(Some(secret))
    }

    fn seal(plaintext: &str) -> Result<Vec<u8>> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        use rand::RngCore;

        let key_bytes = Self::machine_key();
        let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);

        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow!("encryption error: {e}"))?;

        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn unseal(sealed: &[u8]) -> Result<String> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};

        if sealed.len() < 12 + 16 {
            return Err(anyhow!("sealed data too short"));
        }

        let key_bytes = Self::machine_key();
        let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);

        let nonce = Nonce::from_slice(&sealed[..12]);
        let plaintext = cipher
            .decrypt(nonce, &sealed[12..])
            .map_err(|e| anyhow!("decryption error: {e}"))?;

        String::from_utf8(plaintext).map_err(|e| anyhow!("utf8 error: {e}"))
    }

    fn machine_key() -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"nine-snake-key-vault-v1");
        if let Ok(hostname) = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .or_else(|_| std::env::var("USER"))
        {
            hasher.update(hostname.as_bytes());
        }
        if let Ok(user) = std::env::var("USERNAME").or_else(|_| std::env::var("USER")) {
            hasher.update(user.as_bytes());
        }
        hasher.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_retrieve_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let vault = KeyVault::new(dir.path());
        vault.store("test-key", "secret-value").await.unwrap();
        let retrieved = vault.retrieve("test-key").await.unwrap();
        assert_eq!(retrieved, Some("secret-value".to_string()));
    }

    #[tokio::test]
    async fn retrieve_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let vault = KeyVault::new(dir.path());
        let result = vault.retrieve("nonexistent").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn delete_removes_key() {
        let dir = tempfile::tempdir().unwrap();
        let vault = KeyVault::new(dir.path());
        vault.store("to-delete", "value").await.unwrap();
        vault.delete("to-delete").await.unwrap();
        let result = vault.retrieve("to-delete").await.unwrap();
        assert_eq!(result, None);
    }
}
