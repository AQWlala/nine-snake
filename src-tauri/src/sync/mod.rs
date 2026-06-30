//! v0.5: cross-device sync — E2EE + local transport.
//!
//! v0.5 implements the *local* part of the sync story: a pair of
//! devices establish a session key via QR-code pairing (or manual
//! key import), and exchange encrypted envelopes through a shared
//! inbox directory.  Cloud relay, conflict resolution across
//! concurrent edits, and large-file chunking are v1.0 items.
//!
//! v1.1 P1-8: 添加了 `pairing` 模块，提供 QR 码配对功能。
//! v1.1: 添加了 `key_vault` 模块，提供私钥安全存储抽象。

pub mod crdt;
pub mod device_manager;
pub mod e2ee;
pub mod key_vault;
pub mod pairing;
pub mod transport;

pub use crdt::{CrdtEngine, CrdtMergeResult, CrdtVersion, FieldChange};
pub use device_manager::{DeviceManager, DeviceRevokeResult, PairedDevice};
pub use e2ee::{
    encrypt_for_peer, E2eeIdentity, E2eePublicIdentity, EncryptedEnvelope, Pair, SessionKey,
    ENVELOPE_VERSION,
};
pub use key_vault::KeyVault;
pub use pairing::{
    answer_from_qr_string, answer_to_qr_string, offer_from_qr_string, offer_to_qr_string,
    PairingAnswer, PairingOffer, PairingStage, PairingState, PAIRING_VERSION,
};
pub use transport::{recv_all_unsealed, send_sealed, InboxMessage, LocalTransport};
