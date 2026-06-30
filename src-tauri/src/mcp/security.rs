use std::collections::HashSet;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

const SAFE_ENV_VARS: &[&str] = &["PATH", "HOME", "USER", "LANG"];

pub fn filter_safe_env_vars(
    env: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let safe: HashSet<&str> = SAFE_ENV_VARS.iter().copied().collect();
    env.iter()
        .filter(|(k, _)| safe.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn sanitize_credentials(message: &str) -> String {
    let mut result = message.to_string();
    let patterns = [
        (r"sk-[a-zA-Z0-9]{20,}", "[REDACTED_API_KEY]"),
        (r"Bearer\s+[a-zA-Z0-9\-._~+/]+=*", "Bearer [REDACTED]"),
        (r"token[=:]\s*[a-zA-Z0-9\-._~+/]{20,}", "token=[REDACTED]"),
    ];
    for (pattern, replacement) in patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            result = re.replace_all(&result, replacement).to_string();
        }
    }
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceChallenge {
    pub code_verifier: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
}

impl PkceChallenge {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("getrandom failed");
        let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        let digest = sha256_digest(code_verifier.as_bytes());
        let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest);
        Self {
            code_verifier,
            code_challenge,
            code_challenge_method: "S256".to_string(),
        }
    }
}

fn sha256_digest(data: &[u8]) -> [u8; 32] {
    use std::fmt::Write;
    let mut hasher = sha2_assumed::Sha256::new();
    sha2_assumed::Digest::update(&mut hasher, data);
    sha2_assumed::Digest::finalize(hasher).into()
}

mod sha2_assumed {
    pub use sha2::{Digest, Sha256};
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthDiscovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_unsafe_env_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("SECRET_KEY".to_string(), "super-secret".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());
        let filtered = filter_safe_env_vars(&env);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains_key("PATH"));
        assert!(filtered.contains_key("HOME"));
        assert!(!filtered.contains_key("SECRET_KEY"));
    }

    #[test]
    fn sanitizes_api_keys() {
        let msg = "Error with key sk-abc123def456ghi789jkl012mno345pqr678";
        let sanitized = sanitize_credentials(msg);
        assert!(!sanitized.contains("sk-abc123"));
        assert!(sanitized.contains("[REDACTED_API_KEY]"));
    }

    #[test]
    fn pkce_challenge_generates() {
        let pkce = PkceChallenge::generate();
        assert_eq!(pkce.code_challenge_method, "S256");
        assert!(!pkce.code_verifier.is_empty());
        assert!(!pkce.code_challenge.is_empty());
        assert_ne!(pkce.code_verifier, pkce.code_challenge);
    }
}
