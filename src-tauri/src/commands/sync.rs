//! Sync (E2EE) commands — identity, encrypt, decrypt, send, recv, ack.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::sync::{self as sync_ops, E2eeIdentity, EncryptedEnvelope, Pair};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakeIdentityResponse {
    pub public_key: String,
    pub secret_key: String,
}

#[tauri::command]
#[instrument(skip(), fields(otel.kind = "sync_make_identity"))]
pub async fn sync_make_identity() -> Result<MakeIdentityResponse, CommandError> {
    let id = E2eeIdentity::generate();
    Ok(MakeIdentityResponse {
        public_key: id.public_key_b64(),
        secret_key: base64::engine::general_purpose::STANDARD.encode(id.secret_bytes()),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptRequest {
    pub plaintext_b64: String,
    pub local_secret_b64: String,
    pub peer_public_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptResponse {
    pub envelope: EncryptedEnvelope,
    pub envelope_b64: String,
    pub fingerprint: String,
}

#[tauri::command]
#[instrument(skip(request), fields(otel.kind = "sync_encrypt"))]
pub async fn sync_encrypt(request: EncryptRequest) -> Result<EncryptResponse, CommandError> {
    let local = identity_from_secret_b64(&request.local_secret_b64)
        .map_err(|e| CommandError::validation("sync_encrypt").with_details(e.to_string()))?;
    let plaintext = base64::engine::general_purpose::STANDARD
        .decode(request.plaintext_b64.as_bytes())
        .map_err(|e| {
            CommandError::validation("sync_encrypt").with_details(format!("plaintext: {e}"))
        })?;
    let (env, fingerprint) =
        sync_ops::encrypt_for_peer(&local, &request.peer_public_b64, &plaintext)
            .map_err(|e| CommandError::validation("sync_encrypt").with_details(e.to_string()))?;
    let b64 = env
        .to_b64_json()
        .map_err(|e| CommandError::internal("sync_encrypt", &e))?;
    Ok(EncryptResponse {
        envelope: env,
        envelope_b64: b64,
        fingerprint,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptRequest {
    pub envelope: EncryptedEnvelope,
    pub local_secret_b64: String,
    pub peer_public_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptResponse {
    pub plaintext_b64: String,
}

#[tauri::command]
#[instrument(skip(request), fields(otel.kind = "sync_decrypt"))]
pub async fn sync_decrypt(request: DecryptRequest) -> Result<DecryptResponse, CommandError> {
    let local = identity_from_secret_b64(&request.local_secret_b64)
        .map_err(|e| CommandError::validation("sync_decrypt").with_details(e.to_string()))?;
    let pair = Pair::new(local, &request.peer_public_b64)
        .map_err(|e| CommandError::validation("sync_decrypt").with_details(e.to_string()))?;
    let pt = pair
        .decrypt(&request.envelope)
        .map_err(|e| CommandError::validation("sync_decrypt").with_details(e.to_string()))?;
    Ok(DecryptResponse {
        plaintext_b64: base64::engine::general_purpose::STANDARD.encode(&pt),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendSealedRequest {
    pub plaintext_b64: String,
    pub local_secret_b64: String,
    pub peer_public_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendSealedResponse {
    pub envelope_id: String,
    pub fingerprint: String,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "sync_send"))]
pub async fn sync_send(
    state: State<'_, AppState>,
    request: SendSealedRequest,
) -> Result<SendSealedResponse, CommandError> {
    let local = identity_from_secret_b64(&request.local_secret_b64)
        .map_err(|e| CommandError::validation("sync_send").with_details(e.to_string()))?;
    let pair = Pair::new(local, &request.peer_public_b64)
        .map_err(|e| CommandError::validation("sync_send").with_details(e.to_string()))?;
    let pt = base64::engine::general_purpose::STANDARD
        .decode(request.plaintext_b64.as_bytes())
        .map_err(|e| {
            CommandError::validation("sync_send").with_details(format!("plaintext: {e}"))
        })?;
    let transport = state.sync_transport.clone();
    let fingerprint = pair.fingerprint.clone();
    let id = tokio::task::spawn_blocking(move || {
        sync_ops::send_sealed(&transport, &pair, &pt)
            .map_err(|e| CommandError::internal("sync_send", &e))
    })
    .await
    .map_err(|e| CommandError::internal("sync_send", &anyhow::anyhow!("{e}")))??;
    Ok(SendSealedResponse {
        envelope_id: id,
        fingerprint,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecvRequest {
    pub local_secret_b64: String,
    pub peer_public_b64: String,
    pub ack: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecvResponse {
    pub messages: Vec<sync_ops::InboxMessage>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "sync_recv"))]
pub async fn sync_recv(
    state: State<'_, AppState>,
    request: RecvRequest,
) -> Result<RecvResponse, CommandError> {
    let transport = state.sync_transport.clone();
    let inbox_msgs: Vec<sync_ops::InboxMessage> = tokio::task::spawn_blocking(move || {
        transport
            .recv()
            .map_err(|e| CommandError::internal("sync_recv", &e))
    })
    .await
    .map_err(|e| CommandError::internal("sync_recv", &anyhow::anyhow!("{e}")))?
    .map_err(|e| CommandError::internal("sync_recv", &anyhow::anyhow!("{e}")))?;

    let local = identity_from_secret_b64(&request.local_secret_b64)
        .map_err(|e| CommandError::validation("sync_recv").with_details(e.to_string()))?;
    let pair = Pair::new(local, &request.peer_public_b64)
        .map_err(|e| CommandError::validation("sync_recv").with_details(e.to_string()))?;

    let mut messages = Vec::new();
    for msg in inbox_msgs {
        match pair.decrypt(&msg.envelope) {
            Ok(_pt) => {
                if request.ack {
                    let _ = state.sync_transport.ack(&msg.id);
                }
                messages.push(msg);
            }
            Err(e) => {
                tracing::warn!(target: "nine_snake.sync", error = %e, id = %msg.id, "failed to decrypt envelope");
                messages.push(msg);
            }
        }
    }
    Ok(RecvResponse { messages })
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "sync_ack"))]
pub async fn sync_ack(
    state: State<'_, AppState>,
    envelope_id: String,
) -> Result<bool, CommandError> {
    let transport = state.sync_transport.clone();
    tokio::task::spawn_blocking(move || {
        transport
            .ack(&envelope_id)
            .map_err(|e| CommandError::internal("sync_ack", &e))
    })
    .await
    .map_err(|e| CommandError::internal("sync_ack", &anyhow::anyhow!("{e}")))?
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

pub(crate) fn identity_from_secret_b64(b64: &str) -> anyhow::Result<E2eeIdentity> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let bytes = B64.decode(b64.as_bytes())?;
    if bytes.len() != 32 {
        anyhow::bail!("secret must be 32 bytes, got {}", bytes.len());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(E2eeIdentity::from_bytes(arr))
}
