//! v0.5: cross-device transport (local-only).
//!
//! For v0.5 the transport is intentionally local: encrypted
//! envelopes are written to a per-pair "inbox" directory on disk
//! and polled by the receiver.  There is no network, no relay
//! server, no cloud.  The threat model is "data at rest on a
//! shared filesystem" (e.g. a phone and a laptop both pointed at
//! the same synced folder).
//!
//! The v1.0 transport will add a QUIC peer-to-peer channel that
//! can run over LAN or a relay.  The wire envelope format
//! (`EncryptedEnvelope`) is the same; only the transport
//! changes.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, instrument, warn};

use super::e2ee::{EncryptedEnvelope, Pair};

/// One entry in a transport inbox.  Returned to the front-end so
/// it can decide what to do (apply, ignore, surface to the user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub id: String,
    pub received_at: i64,
    pub envelope: EncryptedEnvelope,
}

/// A local-only transport rooted at `inbox_root`.  Filenames are
/// the envelope id (uuid v4) + `.json`; the contents are the
/// `EncryptedEnvelope` serialised in the b64 wire format.
pub struct LocalTransport {
    inbox_root: PathBuf,
}

impl LocalTransport {
    /// Creates a new transport rooted at `inbox_root`.  The
    /// directory is created if it doesn't exist.
    pub fn new<P: AsRef<Path>>(inbox_root: P) -> Result<Self> {
        let root = inbox_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating inbox dir: {}", root.display()))?;
        Ok(Self { inbox_root: root })
    }

    /// Drops a sealed envelope into the inbox.  The caller has
    /// already encrypted the payload with the peer's session key.
    #[instrument(skip(self, envelope), fields(envelope_id = %envelope_id))]
    pub fn send(&self, envelope_id: &str, envelope: &EncryptedEnvelope) -> Result<()> {
        let path = self.path_for(envelope_id);
        let json = envelope.to_b64_json()?;
        // Write to a tmp file first so a partial write doesn't
        // corrupt the receiver's view.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing tmp envelope: {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming envelope: {}", path.display()))?;
        info!(target: "nine_snake.sync", envelope_id, "envelope written to inbox");
        Ok(())
    }

    /// Polls the inbox for new envelopes.  Returns one entry per
    /// file, sorted by mtime (oldest first).  The caller is
    /// expected to `ack` each message after successful processing.
    pub fn recv(&self) -> Result<Vec<InboxMessage>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.inbox_root)
            .with_context(|| format!("reading inbox: {}", self.inbox_root.display()))?
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!(target: "nine_snake.sync", error = ?e, "inbox entry error");
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warn!(target: "nine_snake.sync", error = ?e, path = %path.display(), "inbox read failed");
                    continue;
                }
            };
            let envelope = match EncryptedEnvelope::from_b64_json(
                std::str::from_utf8(&bytes).unwrap_or(""),
            ) {
                Ok(e) => e,
                Err(e) => {
                    warn!(target: "nine_snake.sync", error = ?e, path = %path.display(), "inbox parse failed");
                    continue;
                }
            };
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(InboxMessage {
                id,
                received_at: mtime,
                envelope,
            });
        }
        out.sort_by_key(|m| m.received_at);
        Ok(out)
    }

    /// Acknowledges a message by removing it from the inbox.
    pub fn ack(&self, envelope_id: &str) -> Result<bool> {
        let path = self.path_for(envelope_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow!("ack failed: {e}")),
        }
    }

    fn path_for(&self, envelope_id: &str) -> PathBuf {
        // Defence in depth: refuse ids that escape the inbox.
        if envelope_id.is_empty()
            || envelope_id.contains('/')
            || envelope_id.contains('\\')
            || envelope_id.contains("..")
        {
            // Fall back to a safe synthetic id; the envelope will
            // still be written but with a hashed name.
            let safe = format!("unsafe-{:x}", hash_envelope_id(envelope_id));
            self.inbox_root.join(format!("{safe}.json"))
        } else {
            self.inbox_root.join(format!("{envelope_id}.json"))
        }
    }
}

fn hash_envelope_id(id: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// Convenience: encrypts `plaintext` with the pair's session key
/// and drops the resulting envelope into the transport.  Returns
/// the envelope id used (uuid v4).
pub fn send_sealed(transport: &LocalTransport, pair: &Pair, plaintext: &[u8]) -> Result<String> {
    let env = pair.encrypt(plaintext)?;
    let id = uuid::Uuid::new_v4().to_string();
    transport.send(&id, &env)?;
    Ok(id)
}

/// Convenience: reads the inbox, decrypts each envelope, and
/// returns `(envelope_id, plaintext)` pairs in arrival order.
pub fn recv_all_unsealed(
    transport: &LocalTransport,
    pair: &Pair,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for msg in transport.recv()? {
        let pt = pair.decrypt(&msg.envelope)?;
        out.push((msg.id, pt));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::e2ee::E2eeIdentity;

    #[test]
    fn send_and_recv_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let transport = LocalTransport::new(dir.path()).unwrap();
        let local = E2eeIdentity::generate();
        let pair = Pair::new(local.clone(), &local.public_key_b64()).unwrap();

        let id = send_sealed(&transport, &pair, b"hello").unwrap();
        let msgs = recv_all_unsealed(&transport, &pair).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, id);
        assert_eq!(msgs[0].1, b"hello");

        // Ack removes the file.
        assert!(transport.ack(&id).unwrap());
        let msgs2 = transport.recv().unwrap();
        assert!(msgs2.is_empty());
    }

    #[test]
    fn envelope_id_with_path_traversal_is_sanitised() {
        let dir = tempfile::tempdir().unwrap();
        let transport = LocalTransport::new(dir.path()).unwrap();
        let path = transport.path_for("../../../etc/passwd");
        // The result must be rooted inside the inbox.
        assert!(path.starts_with(&transport.inbox_root));
        // And the filename must not contain traversal characters.
        let s = path.to_string_lossy();
        assert!(!s.contains(".."));
    }

    #[test]
    fn timestamp_is_recent() {
        use std::time::SystemTime;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let dir = tempfile::tempdir().unwrap();
        let transport = LocalTransport::new(dir.path()).unwrap();
        let local = E2eeIdentity::generate();
        let pair = Pair::new(local.clone(), &local.public_key_b64()).unwrap();
        let id = send_sealed(&transport, &pair, b"t").unwrap();
        let msgs = transport.recv().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].received_at <= now + 1);
        assert!(msgs[0].received_at >= now - 5);
        let _ = id; // silence unused
    }
}
