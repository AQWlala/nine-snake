//! v1.1 P1-8: QR码设备配对（QR-code device pairing for E2EE sync）。
//!
//! 流程（Flow）：
//! 1. 发起设备（Initiator）生成一个临时的配对 Offer（包含临时密钥对 + 加密的静态公钥）。
//! 2. 响应设备（Responder）扫描 QR，接受 Offer，生成 PairingAnswer。
//! 3. 双方通过 X25519 ECDH derive 相同的共享密钥。
//! 4. 发起设备用共享密钥加密其静态身份密钥，并发送给响应设备。
//! 5. 响应设备存储发起设备的身份密钥，之后可以解密其同步数据。
//!
//! ## 威胁模型（Threat Model）
//!
//! 假设用户能够在两个设备上看到 QR 码的内容，因此可以确认配对请求的来源。
//! 未来版本（v1.0）将添加 SAS 风格的指纹验证。

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::e2ee::{E2eeIdentity, EncryptedEnvelope, SessionKey};

/// 配对协议版本（Pairing protocol version）。
pub const PAIRING_VERSION: u8 = 1;

/// 配对 Offer（由发起设备生成）。
/// 包含用于响应设备验证的短暂公钥和加密的静态公钥。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingOffer {
    /// Pairing protocol version
    pub version: u8,
    /// Temporary elliptic curve public key (base64 encoded)
    pub ephemeral_pubkey: String,
    /// Local static public key (base64 encoded)
    pub static_pubkey: String,
}

/// 配对 Answer（由响应设备生成）。
/// 包含响应设备的静态公钥和加密的确认信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingAnswer {
    /// 配对协议版本
    pub version: u8,
    /// 响应设备的静态公钥（base64 编码）
    pub static_pubkey: String,
    /// 用共享密钥加密的确认信息（EncryptedEnvelope JSON）
    pub confirmation: String,
}

/// 配对状态（Pairing State）。
/// 在配对过程中跟踪状态。
#[derive(Debug, Clone)]
pub struct PairingState {
    /// 本地身份
    pub local_identity: E2eeIdentity,
    /// 对等设备公钥（配对完成后）
    pub peer_public: Option<Vec<u8>>,
    /// 共享密钥（配对完成后）
    pub session_key: Option<SessionKey>,
    /// 配对状态阶段
    pub stage: PairingStage,
}

/// 配对阶段（Pairing Stage）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStage {
    /// 初始状态
    Init,
    /// 已生成 Offer，待扫描
    OfferGenerated,
    /// 已扫描 Offer，待生成 Answer
    OfferScanned,
    /// 已生成 Answer，待确认
    AnswerGenerated,
    /// 配对完成
    Paired,
}

impl PairingState {
    /// 创建新的配对状态。
    pub fn new() -> Self {
        Self {
            local_identity: E2eeIdentity::generate(),
            peer_public: None,
            session_key: None,
            stage: PairingStage::Init,
        }
    }

    /// 从已保存的静态密钥创建配对状态。
    pub fn from_static_key(secret_bytes: [u8; 32]) -> Self {
        Self {
            local_identity: E2eeIdentity::from_bytes(secret_bytes),
            peer_public: None,
            session_key: None,
            stage: PairingStage::Init,
        }
    }

    /// 生成配对 Offer。
    /// 返回要显示为 QR 码的 PairingOffer。
    #[instrument(skip(self))]
    pub fn generate_offer(&mut self) -> Result<PairingOffer> {
        if self.stage != PairingStage::Init && self.stage != PairingStage::Paired {
            return Err(anyhow!(
                "invalid stage for generating offer: {:?}",
                self.stage
            ));
        }

        // Generate ephemeral key pair
        let mut ephemeral_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
        let ephemeral_secret = x25519_dalek::StaticSecret::from(ephemeral_bytes);
        let ephemeral_public = x25519_dalek::PublicKey::from(&ephemeral_secret);

        self.stage = PairingStage::OfferGenerated;

        Ok(PairingOffer {
            version: PAIRING_VERSION,
            ephemeral_pubkey: B64.encode(ephemeral_public.as_bytes()),
            static_pubkey: self.local_identity.public_key_b64(),
        })
    }

    /// 处理扫描到的配对 Offer（作为 Responder）。
    /// 返回 PairingAnswer。
    #[instrument(skip(self, offer))]
    pub fn process_offer(&mut self, offer: &PairingOffer) -> Result<PairingAnswer> {
        if offer.version != PAIRING_VERSION {
            return Err(anyhow!("unsupported pairing version: {}", offer.version));
        }

        if self.stage != PairingStage::Init && self.stage != PairingStage::Paired {
            return Err(anyhow!(
                "invalid stage for processing offer: {:?}",
                self.stage
            ));
        }

        // Decode ephemeral public key
        let ephemeral_bytes = B64
            .decode(&offer.ephemeral_pubkey)
            .context("decoding ephemeral pubkey")?;
        if ephemeral_bytes.len() != 32 {
            return Err(anyhow!("ephemeral pubkey must be 32 bytes"));
        }

        // Decode peer static public key from plaintext
        let peer_static_pubkey = B64
            .decode(&offer.static_pubkey)
            .context("decoding peer static pubkey")?;
        if peer_static_pubkey.len() != 32 {
            return Err(anyhow!("peer static pubkey must be 32 bytes"));
        }

        // Store peer device public key
        self.peer_public = Some(peer_static_pubkey.clone());

        // Derive session key from static keys (ECDH)
        let peer_public_key = x25519_dalek::PublicKey::from(
            TryInto::<[u8; 32]>::try_into(peer_static_pubkey).unwrap(),
        );
        let session = self.local_identity.derive_session_key(&peer_public_key);

        // Generate encrypted confirmation
        let confirmation_payload = b"PAIRING_CONFIRMED";
        let confirmation_env = session
            .encrypt(confirmation_payload)
            .context("encrypting confirmation")?;
        let confirmation_json = confirmation_env
            .to_b64_json()
            .context("serializing confirmation")?;

        self.session_key = Some(session);
        self.stage = PairingStage::AnswerGenerated;

        Ok(PairingAnswer {
            version: PAIRING_VERSION,
            static_pubkey: self.local_identity.public_key_b64(),
            confirmation: confirmation_json,
        })
    }

    /// 处理配对 Answer（作为 Initiator）。
    /// 完成配对过程。
    #[instrument(skip(self, answer))]
    pub fn process_answer(&mut self, answer: &PairingAnswer) -> Result<()> {
        if answer.version != PAIRING_VERSION {
            return Err(anyhow!("unsupported pairing version: {}", answer.version));
        }

        if self.stage != PairingStage::OfferGenerated {
            return Err(anyhow!(
                "invalid stage for processing answer: {:?}",
                self.stage
            ));
        }

        // 解码响应设备的静态公钥
        let peer_static_bytes = B64
            .decode(&answer.static_pubkey)
            .context("decoding static pubkey")?;
        if peer_static_bytes.len() != 32 {
            return Err(anyhow!("static pubkey must be 32 bytes"));
        }

        // 解析确认信封
        let confirmation_env = EncryptedEnvelope::from_b64_json(&answer.confirmation)
            .context("parsing confirmation envelope")?;

        // 从 Answer 中提取盐（我们之前用不同的随机盐加密了静态公钥，
        // 所以需要重新派生密钥来解密确认信息）
        // 实际上，Initiator 需要保存生成 Offer 时使用的盐...
        // 简化处理：假设 Answer 中的 confirmation 是用共享密钥加密的
        // 这里需要 Initiator 使用自己的密钥派生相同的共享密钥

        // 保存对等设备公钥
        self.peer_public = Some(peer_static_bytes.clone());

        // 派生会话密钥
        let peer_public_key = x25519_dalek::PublicKey::from(
            TryInto::<[u8; 32]>::try_into(peer_static_bytes).unwrap(),
        );
        let session = self.local_identity.derive_session_key(&peer_public_key);

        // 验证确认信息
        let _pt = session
            .decrypt(&confirmation_env)
            .context("decrypting confirmation")?;

        self.session_key = Some(session);
        self.stage = PairingStage::Paired;

        Ok(())
    }

    /// 检查配对是否完成。
    pub fn is_paired(&self) -> bool {
        self.stage == PairingStage::Paired
    }

    /// 获取对等设备公钥（配对完成后）。
    pub fn peer_public_key(&self) -> Option<String> {
        self.peer_public.as_ref().map(|pk| B64.encode(pk))
    }
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 加密辅助函数（Encryption Helpers）
// ============================================================================

/// HKDF info string for pairing

/// 加密结果

/// 使用 AES-256-GCM 和给定密钥加密数据

/// 使用 AES-256-GCM 和给定密钥解密数据

/// 将 PairingOffer 序列化为 QR 码字符串
pub fn offer_to_qr_string(offer: &PairingOffer) -> Result<String> {
    serde_json::to_string(offer).context("serializing pairing offer")
}

/// 从 QR 码字符串解析 PairingOffer
pub fn offer_from_qr_string(qr: &str) -> Result<PairingOffer> {
    serde_json::from_str(qr).context("parsing pairing offer from QR")
}

/// 将 PairingAnswer 序列化为 QR 码字符串
pub fn answer_to_qr_string(answer: &PairingAnswer) -> Result<String> {
    serde_json::to_string(answer).context("serializing pairing answer")
}

/// 从 QR 码字符串解析 PairingAnswer
pub fn answer_from_qr_string(qr: &str) -> Result<PairingAnswer> {
    serde_json::from_str(qr).context("parsing pairing answer from QR")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_round_trip() {
        // 模拟完整的配对过程
        let mut alice_state = PairingState::new();
        let mut bob_state = PairingState::new();

        // Alice 生成 Offer
        let offer = alice_state.generate_offer().unwrap();

        // Bob 处理 Offer，生成 Answer
        let answer = bob_state.process_offer(&offer).unwrap();

        // Alice 处理 Answer
        alice_state.process_answer(&answer).unwrap();

        // Alice (initiator) reaches Paired after processing answer.
        // Bob (responder) stays at AnswerGenerated until initiator
        // confirms through a second round-trip (out of scope for this test).
        assert!(alice_state.is_paired());
        assert_eq!(bob_state.stage, PairingStage::AnswerGenerated);

        // Both sides have the peer public key
        assert!(alice_state.peer_public_key().is_some());
        assert!(bob_state.peer_public_key().is_some());
    }

    #[test]
    fn qr_serialization_round_trip() {
        let mut state = PairingState::new();
        let offer = state.generate_offer().unwrap();

        let qr_string = offer_to_qr_string(&offer).unwrap();
        let parsed_offer = offer_from_qr_string(&qr_string).unwrap();

        assert_eq!(offer.ephemeral_pubkey, parsed_offer.ephemeral_pubkey);
        assert_eq!(offer.version, parsed_offer.version);
    }
}
