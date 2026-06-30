//! `nine_snake::memory` — v7.0 layered memory system.
//!
//! The memory subsystem is the heart of nine-snake. It provides:
//!
//! * a strongly typed [`Memory`] value object with five cognitive
//!   [`MemoryType`]s and eight [`MemoryLayer`]s (L0..L7),
//! * a SQLite-backed structured store ([`sqlite_store`]),
//! * a LanceDB-backed dense vector store ([`lance_store`]),
//! * an embedding client ([`embedder`]) targeting BGE-small-zh-v1.5 via
//!   the local Ollama HTTP endpoint,
//! * an [`importance`] scorer that combines access frequency, recency and
//!   explicit user feedback,
//! * a [`blackhole`] compression engine that *never* deletes memories
//!   (it only densifies them) and respects the L7 singularity layer,
//! * a [`sponge`] absorption engine that de-duplicates, normalises and
//!   links incoming memories before they reach the hot path, and
//! * a [`reflect`] meta-cognitive engine (L5) that periodically
//!   summarises recent high-importance memories into reflections.
//!
//! The [`types`] module is the canonical source of truth for the data
//! model; every other module re-exports the relevant types from there.

pub mod acl;
pub mod blackhole;
pub mod embedder;
pub mod entity_extractor;
pub mod export;
pub mod forgetting;
pub mod graph_search;
pub mod importance;
pub mod lance_store;
pub mod layers;
pub mod migration;
pub mod reflect;
pub mod sponge;
pub mod sqlite_store;
pub mod types;

pub use acl::{AclEffect, AclPermission, AclRule, MemoryAcl};
pub use blackhole::BlackholeEngine;
pub use embedder::Embedder;
pub use entity_extractor::{EntityExtractor, ExtractedRelation};
pub use export::{DataExporter, ExportManifest, ImportResult};
pub use forgetting::{ForgettingCandidate, ForgettingConfig, ForgettingEngine};
pub use graph_search::{GraphSearchConfig, GraphSearchEngine, GraphSearchResult};
pub use importance::ImportanceScorer;
pub use lance_store::LanceStore;
pub use layers::LayerPolicy;
pub use migration::{Migration, MigrationState, MigrationStatus};
pub use reflect::{ReflectConfig, Reflection, ReflectionEngine};
pub use sponge::SpongeEngine;
pub use sqlite_store::SqliteStore;
pub use types::{Memory, MemoryLayer, MemoryType, MultiGranularity, RelationKind, SourceKind};

/// Constants shared across the memory subsystem.
pub mod constants {
    /// Default cosine-similarity threshold above which the sponge considers
    /// two memories "the same" and merges them.
    pub const SPONGE_MERGE_THRESHOLD: f32 = 0.85;

    /// Hard lower bound on memory importance. Below this value (and after
    /// the configured inactivity threshold) the black-hole may compress.
    pub const BLACKHOLE_IMPORTANCE_FLOOR: f32 = 0.10;

    /// Multi-granularity summary length buckets (characters).
    pub const SUMMARY_BUCKETS: [usize; 4] = [50, 150, 500, 2000];
}
