//! DID identity commands — generate, resolve.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::commands::error::CommandError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateDidResponse {
    pub did: String,
    pub public_key_b64: String,
    pub document: crate::identity::DidDocument,
}

#[tauri::command]
#[instrument(fields(otel.kind = "generate_did"))]
pub async fn generate_did(
    public_key_b64: Option<String>,
) -> Result<GenerateDidResponse, CommandError> {
    let did_key = match public_key_b64 {
        Some(b64) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .map_err(|e| {
                    CommandError::validation("generate_did")
                        .with_details(format!("invalid base64: {e}"))
                })?;
            if bytes.len() != 32 {
                return Err(CommandError::validation("generate_did")
                    .with_details(format!("public key must be 32 bytes, got {}", bytes.len())));
            }
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&bytes);
            crate::identity::DidKey::from_public_key(&pk)
        }
        None => {
            let mut pk = [0u8; 32];

            getrandom::getrandom(&mut pk)
                .map_err(|e| CommandError::internal("generate_did", &anyhow::anyhow!("{e}")))?;
            crate::identity::DidKey::from_public_key(&pk)
        }
    };
    let document = crate::identity::DidDocument::from_did_key(&did_key);
    Ok(GenerateDidResponse {
        did: did_key.did.clone(),
        public_key_b64: did_key.public_key_b64(),
        document,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveDidResponse {
    pub did: String,
    pub document: crate::identity::DidDocument,
}

#[tauri::command]
#[instrument(fields(otel.kind = "resolve_did"))]
pub async fn resolve_did(did: String) -> Result<ResolveDidResponse, CommandError> {
    let did_key = crate::identity::DidKey::parse(&did)
        .map_err(|e| CommandError::validation("resolve_did").with_details(e.to_string()))?;
    let document = crate::identity::DidDocument::from_did_key(&did_key);
    Ok(ResolveDidResponse {
        did: did_key.did,
        document,
    })
}
