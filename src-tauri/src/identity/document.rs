use serde::{Deserialize, Serialize};

use super::DidKey;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DidDocument {
    #[serde(rename = "@context")]
    pub context: Vec<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verification_method: Vec<VerificationMethod>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authentication: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "keyAgreement")]
    pub key_agreement: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationMethod {
    pub id: String,
    #[serde(rename = "type")]
    pub vm_type: String,
    pub controller: String,
    #[serde(rename = "publicKeyBase58")]
    pub public_key_base58: String,
}

impl DidDocument {
    pub fn from_did_key(did_key: &DidKey) -> Self {
        let vm_id = format!("{}#key-1", did_key.did);
        let pk_base58 = bs58::encode(did_key.public_key_bytes).into_string();

        DidDocument {
            context: vec![
                "https://www.w3.org/ns/did/v1".to_string(),
                "https://w3id.org/security/suites/x25519-2020/v1".to_string(),
            ],
            id: did_key.did.clone(),
            verification_method: vec![VerificationMethod {
                id: vm_id.clone(),
                vm_type: "X25519KeyAgreementKey2020".to_string(),
                controller: did_key.did.clone(),
                public_key_base58: pk_base58,
            }],
            authentication: vec![],
            key_agreement: vec![vm_id],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_has_key_agreement() {
        let pk = [7u8; 32];
        let did_key = DidKey::from_public_key(&pk);
        let doc = DidDocument::from_did_key(&did_key);
        assert_eq!(doc.id, did_key.did);
        assert_eq!(doc.key_agreement.len(), 1);
        assert!(doc.key_agreement[0].ends_with("#key-1"));
        assert_eq!(doc.verification_method.len(), 1);
        assert_eq!(
            doc.verification_method[0].vm_type,
            "X25519KeyAgreementKey2020"
        );
    }

    #[test]
    fn document_serializes_valid_json() {
        let pk = [7u8; 32];
        let did_key = DidKey::from_public_key(&pk);
        let doc = DidDocument::from_did_key(&did_key);
        let json = serde_json::to_string(&doc).unwrap();
        assert!(json.contains("\"@context\""));
        assert!(json.contains("\"keyAgreement\""));
    }
}
