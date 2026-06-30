use anyhow::{Context, Result};
use base64::Engine;

const X25519_MULTICODEC: &[u8] = &[0xec, 0x01];

#[derive(Debug, Clone)]
pub struct DidKey {
    pub did: String,
    pub public_key_bytes: [u8; 32],
}

impl DidKey {
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let mut prefixed = Vec::with_capacity(X25519_MULTICODEC.len() + 32);
        prefixed.extend_from_slice(X25519_MULTICODEC);
        prefixed.extend_from_slice(public_key);

        let encoded = multibase_base58btc(&prefixed);
        let did = format!("did:key:z{encoded}");

        DidKey {
            did,
            public_key_bytes: *public_key,
        }
    }

    pub fn parse(did: &str) -> Result<Self> {
        let rest = did
            .strip_prefix("did:key:z")
            .context("DID must start with 'did:key:z'")?;

        let decoded = decode_base58btc(rest).context("failed to decode multibase base58btc")?;

        if decoded.len() < 2 + 32 {
            anyhow::bail!("decoded DID key too short");
        }

        if decoded[0] != X25519_MULTICODEC[0] || decoded[1] != X25519_MULTICODEC[1] {
            anyhow::bail!("unsupported key type (expected X25519 multicodec 0xec01)");
        }

        let mut pk = [0u8; 32];
        pk.copy_from_slice(&decoded[2..34]);

        Ok(DidKey {
            did: did.to_string(),
            public_key_bytes: pk,
        })
    }

    pub fn public_key_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.public_key_bytes)
    }
}

fn multibase_base58btc(data: &[u8]) -> String {
    bs58::encode(data).into_string()
}

fn decode_base58btc(s: &str) -> Result<Vec<u8>> {
    let bytes = bs58::decode(s).into_vec().context("base58 decode error")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let pk = [42u8; 32];
        let did_key = DidKey::from_public_key(&pk);
        assert!(did_key.did.starts_with("did:key:z"));

        let parsed = DidKey::parse(&did_key.did).unwrap();
        assert_eq!(parsed.public_key_bytes, pk);
        assert_eq!(parsed.did, did_key.did);
    }

    #[test]
    fn parse_rejects_invalid_prefix() {
        assert!(DidKey::parse("did:web:example.com").is_err());
        assert!(DidKey::parse("did:key:abc").is_err());
    }
}
