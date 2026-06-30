//! `nine_snake::skills` — v0.3 procedural-memory subsystem.
//!
//! The `skills` table was reserved in v0.1 (see
//! `migrations/001_initial.sql`) but never written to. v0.3 promotes
//! it to a first-class subsystem with its own store, execution engine
//! and gRPC/Tauri surface.
//!
//! ## Layout
//!
//! * [`types`] — wire-shape DTOs (Skill, SkillResult, request/response
//!   envelopes).
//! * [`store`] — thin SQLite wrapper around the existing `skills` table.
//! * [`engine`] — orchestrates creation, execution, rating and search.
//! * [`extractor`] — v1.2 skill closed-loop learning: auto-distils reusable
//!   skills from successful swarm task executions.
//! * [`sandbox`] — v1.3 capability-based sandbox model for skill execution
//!   isolation (P1-3).
//! * [`marketplace`] — v1.3 skill marketplace with search, one-click install,
//!   update checking and publishing infrastructure (P2-7).

pub mod audit;
pub mod engine;
pub mod extractor;
pub mod hub_client;
pub mod importer;
pub mod marketplace;
pub mod sandbox;
pub mod seeder;
pub mod store;
pub mod types;

pub use audit::{redact_if_sensitive, truncate_summary, SkillAuditEntry, SkillAuditLogger};
pub use engine::SkillEngine;
pub use extractor::{ExtractionReport, SkillExtractor};
pub use importer::{ImportResult, SkillImporter, SkillSource};
pub use marketplace::{
    MarketplaceQuery, MarketplaceResponse, MarketplaceStats, PublishManifest, SearchHit,
    SkillEntry, SkillMarketplace, SortBy, UpdateInfo,
};
pub use sandbox::{
    Capability, CapabilitySet, RiskLevel, SandboxConfig, SandboxPolicy, SandboxResult,
};
pub use seeder::seed_demo_skills;
pub use store::SkillStore;
pub use types::{
    CreateSkillRequest, ListSkillsRequest, RateSkillRequest, Skill, SkillResult,
    SkillSearchRequest, UseSkillRequest,
};
