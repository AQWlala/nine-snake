//! Core data model for the nine-snake v7.0 memory system.
//!
//! These types are intentionally `Clone` + `Serialize`/`Deserialize` so
//! they can flow freely through the Tauri command boundary, the swarm
//! orchestrator and the SQLite/LanceDB persistence layers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Cognitive classification of a single memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Factual, declarative knowledge ("the sky is blue").
    Semantic,
    /// Time-stamped events and experiences ("user asked about X at 14:32").
    Episodic,
    /// Reusable procedures and skills ("how to reset a router").
    Procedural,
    /// Emotional impressions and value judgements ("user seemed frustrated").
    Emotional,
    /// Self-reflective observations about cognition itself.
    Metacognitive,
}

impl MemoryType {
    /// String form used by the database and HTTP APIs.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Semantic => "semantic",
            MemoryType::Episodic => "episodic",
            MemoryType::Procedural => "procedural",
            MemoryType::Emotional => "emotional",
            MemoryType::Metacognitive => "metacognitive",
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "semantic" => Ok(MemoryType::Semantic),
            "episodic" => Ok(MemoryType::Episodic),
            "procedural" => Ok(MemoryType::Procedural),
            "emotional" => Ok(MemoryType::Emotional),
            "metacognitive" => Ok(MemoryType::Metacognitive),
            other => Err(format!("unknown memory type: {other}")),
        }
    }
}

/// Memory layer hierarchy (L0-L5 active, L6-L7 reserved for v1.5+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MemoryLayer {
    /// Temporary cache (single conversation turn).
    L0,
    /// Rolling message history within a session.
    L1,
    /// Cross-session experience.
    L2,
    /// Concrete facts.
    L3,
    /// Distilled knowledge.
    L4,
    /// Lessons learned from mistakes.
    L5,
    /// Re-usable principles.
    L6,
    /// Singularity — the core, never compressed.
    L7,
}

impl MemoryLayer {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryLayer::L0 => "L0",
            MemoryLayer::L1 => "L1",
            MemoryLayer::L2 => "L2",
            MemoryLayer::L3 => "L3",
            MemoryLayer::L4 => "L4",
            MemoryLayer::L5 => "L5",
            MemoryLayer::L6 => "L6",
            MemoryLayer::L7 => "L7",
        }
    }

    /// Layers that the black-hole engine is *never* allowed to touch.
    pub fn is_immutable(&self) -> bool {
        matches!(self, MemoryLayer::L7)
    }
}

impl fmt::Display for MemoryLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryLayer {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "L0" => Ok(MemoryLayer::L0),
            "L1" => Ok(MemoryLayer::L1),
            "L2" => Ok(MemoryLayer::L2),
            "L3" => Ok(MemoryLayer::L3),
            "L4" => Ok(MemoryLayer::L4),
            "L5" => Ok(MemoryLayer::L5),
            "L6" => Ok(MemoryLayer::L6),
            "L7" => Ok(MemoryLayer::L7),
            other => Err(format!("unknown memory layer: {other}")),
        }
    }
}

/// Origin of a memory — useful for debugging and trust scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    UserInput,
    AgentOutput,
    Reflection,
    System,
    External,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceKind::UserInput => "user_input",
            SourceKind::AgentOutput => "agent_output",
            SourceKind::Reflection => "reflection",
            SourceKind::System => "system",
            SourceKind::External => "external",
        }
    }
}

impl FromStr for SourceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user_input" => Ok(SourceKind::UserInput),
            "agent_output" => Ok(SourceKind::AgentOutput),
            "reflection" => Ok(SourceKind::Reflection),
            "system" => Ok(SourceKind::System),
            "external" => Ok(SourceKind::External),
            other => Err(format!("unknown source kind: {other}")),
        }
    }
}

/// Kind of edge in the memory knowledge graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Causes,
    Supports,
    Contradicts,
    References,
    DerivedFrom,
}

impl RelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationKind::Causes => "causes",
            RelationKind::Supports => "supports",
            RelationKind::Contradicts => "contradicts",
            RelationKind::References => "references",
            RelationKind::DerivedFrom => "derived_from",
        }
    }
}

impl FromStr for RelationKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "causes" => Ok(RelationKind::Causes),
            "supports" => Ok(RelationKind::Supports),
            "contradicts" => Ok(RelationKind::Contradicts),
            "references" => Ok(RelationKind::References),
            "derived_from" => Ok(RelationKind::DerivedFrom),
            other => Err(format!("unknown relation kind: {other}")),
        }
    }
}

/// Four pre-computed summaries at increasing granularity.
///
/// They are produced on `store` and refreshed by the black-hole
/// compression engine. The order of the tuple corresponds to the
/// `constants::SUMMARY_BUCKETS` array (`[50, 150, 500, 2000]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiGranularity {
    pub s50: String,
    pub s150: String,
    pub s500: String,
    pub s2000: String,
}

impl MultiGranularity {
    /// Builds a `MultiGranularity` from raw pre-computed strings.
    pub fn new(
        s50: impl Into<String>,
        s150: impl Into<String>,
        s500: impl Into<String>,
        s2000: impl Into<String>,
    ) -> Self {
        Self {
            s50: s50.into(),
            s150: s150.into(),
            s500: s500.into(),
            s2000: s2000.into(),
        }
    }

    /// Returns the summary at the requested bucket index (0..=3).
    pub fn at_bucket(&self, idx: usize) -> &str {
        match idx {
            0 => &self.s50,
            1 => &self.s150,
            2 => &self.s500,
            _ => &self.s2000,
        }
    }
}

/// The canonical memory record stored across all subsystems.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Stable UUIDv4.
    pub id: String,
    /// Cognitive type.
    pub memory_type: MemoryType,
    /// Layer (L0..L7).
    pub layer: MemoryLayer,
    /// Raw content (un-truncated).
    pub content: String,
    /// Pre-computed multi-granularity summaries.
    pub summary: MultiGranularity,
    /// 512-dim BGE embedding (BGE-small-zh-v1.5). Empty when not yet embedded.
    pub embedding: Vec<f32>,
    /// Importance score in `[0.0, 1.0]`.
    pub importance: f32,
    /// Number of times this memory has been retrieved.
    pub access_count: u32,
    /// Unix timestamp (seconds) of the most recent access.
    pub last_access: i64,
    /// Unix timestamp (seconds) of creation.
    pub created_at: i64,
    /// Origin of this memory.
    pub source: SourceKind,
    /// Free-form extension bag (lang ids, file paths, etc.).
    pub metadata: serde_json::Value,
    /// If this record is the result of a black-hole compression, this
    /// points to the parent record.
    pub compressed_from: Option<String>,
    pub compression_gen: u32,
    pub pinned: bool,
    pub archived: bool,
}

impl Memory {
    /// Convenience constructor for a freshly-spawned memory.
    pub fn new(
        memory_type: MemoryType,
        layer: MemoryLayer,
        content: impl Into<String>,
        source: SourceKind,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            memory_type,
            layer,
            content: content.into(),
            summary: MultiGranularity::default(),
            embedding: Vec::new(),
            importance: 0.5,
            access_count: 0,
            last_access: now,
            created_at: now,
            source,
            metadata: serde_json::json!({}),
            compressed_from: None,
            compression_gen: 0,
            pinned: layer == MemoryLayer::L7,
            archived: false,
        }
    }

    /// Records an access (bumps counter + last_access). Returns the
    /// updated importance (see [`crate::memory::importance`]).
    pub fn touch(&mut self, now: i64) {
        self.access_count = self.access_count.saturating_add(1);
        self.last_access = now;
    }

    /// v1.0.1 P0#12: returns `true` when the `content` field
    /// looks like a secret.  Used by the sponge / blackhole
    /// write paths to blank out the `s2000` summary and set
    /// the `masked` column so the secret never reaches the
    /// long-form summary or the JSON dumps.
    ///
    /// Heuristics (deliberately conservative — false positives
    /// are fine, false negatives are not):
    ///
    /// 1. The lower-cased content contains any of: `api_key`,
    ///    `apikey`, `password`, `passwd`, `secret`, `token`,
    ///    `bearer `, `aws_access`, `aws_secret`,
    ///    `private_key`, `client_secret`.  These are
    ///    case-insensitive substring matches; the trigger
    ///    tokens come from the OAuth 2.0 and AWS secret
    ///    naming conventions.
    /// 2. The content contains a contiguous run of base64-ish
    ///    characters (A–Z, a–z, 0–9, +, /, =) of length ≥ 40
    ///    — long enough to be a real secret, short enough
    ///    not to flag prose.  We require the run to be
    ///    followed or preceded by a non-base64 character
    ///    (whitespace, punctuation) to avoid matching plain
    ///    English.
    /// 3. The content contains a JWT-shaped triple of
    ///    `header.payload.signature` separated by dots, where
    ///    each segment is base64-url.
    pub fn is_sensitive(&self) -> bool {
        sensitive_text_predicate(&self.content)
    }
}

/// v1.0.1 P0#12: free-function form of [`Memory::is_sensitive`]
/// so callers can run the predicate on arbitrary text (e.g.
/// user-supplied `code` fields in skills).
pub fn sensitive_text_predicate(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const TRIGGERS: &[&str] = &[
        "api_key",
        "apikey",
        "password",
        "passwd",
        "secret",
        "token",
        "bearer ",
        "aws_access",
        "aws_secret",
        "private_key",
        "client_secret",
    ];
    if TRIGGERS.iter().any(|t| lower.contains(t)) {
        return true;
    }
    // Long base64 run: at least 40 contiguous characters
    // from the base64 alphabet, bounded by non-base64.
    let mut run: usize = 0;
    let mut best_run: usize = 0;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric()
            || ch == '+'
            || ch == '/'
            || ch == '='
            || ch == '-'
            || ch == '_'
        {
            run = run.saturating_add(1);
            if run > best_run {
                best_run = run;
            }
        } else {
            run = 0;
        }
    }
    if best_run >= 40 {
        return true;
    }
    // JWT shape: three base64-url segments separated by dots,
    // each at least 4 chars.
    let mut dot_runs = 0usize;
    let mut seg_len = 0usize;
    let mut ok = true;
    for ch in text.chars() {
        if ch == '.' {
            if seg_len >= 4 {
                dot_runs += 1;
            } else {
                ok = false;
                break;
            }
            seg_len = 0;
        } else if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            seg_len = seg_len.saturating_add(1);
        } else {
            ok = false;
            break;
        }
    }
    if ok && dot_runs == 2 && seg_len >= 4 {
        return true;
    }
    false
}

/// A relation between two memories (graph edge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub id: String,
    pub src_id: String,
    pub dst_id: String,
    pub kind: RelationKind,
    pub weight: f32,
    pub created_at: i64,
    pub evidence: Option<String>,
}

impl MemoryRelation {
    pub fn new(src_id: impl Into<String>, dst_id: impl Into<String>, kind: RelationKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            src_id: src_id.into(),
            dst_id: dst_id.into(),
            kind,
            weight: 1.0,
            created_at: chrono::Utc::now().timestamp(),
            evidence: None,
        }
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = Some(evidence.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_round_trip() {
        for l in [
            MemoryLayer::L0,
            MemoryLayer::L1,
            MemoryLayer::L2,
            MemoryLayer::L3,
            MemoryLayer::L4,
            MemoryLayer::L5,
            MemoryLayer::L6,
            MemoryLayer::L7,
        ] {
            let parsed: MemoryLayer = l.as_str().parse().unwrap();
            assert_eq!(parsed, l);
        }
    }

    #[test]
    fn only_l7_is_immutable() {
        assert!(MemoryLayer::L7.is_immutable());
        for l in [
            MemoryLayer::L0,
            MemoryLayer::L1,
            MemoryLayer::L2,
            MemoryLayer::L3,
            MemoryLayer::L4,
            MemoryLayer::L5,
            MemoryLayer::L6,
        ] {
            assert!(!l.is_immutable());
        }
    }

    #[test]
    fn memory_new_initialises_pinned_for_l7() {
        let m = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L7,
            "core",
            SourceKind::System,
        );
        assert!(m.pinned);
        let m2 = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            "fact",
            SourceKind::UserInput,
        );
        assert!(!m2.pinned);
    }

    #[test]
    fn touch_increments_counter() {
        let mut m = Memory::new(
            MemoryType::Episodic,
            MemoryLayer::L1,
            "hi",
            SourceKind::UserInput,
        );
        let before = m.access_count;
        m.touch(123);
        assert_eq!(m.access_count, before + 1);
        assert_eq!(m.last_access, 123);
    }

    /// v1.0.1 P0#12: the sensitive-content predicate is the
    /// load-bearing primitive for the masking path.  These
    /// cases pin the trigger list (false positives OK, false
    /// negatives not OK).
    #[test]
    fn is_sensitive_detects_api_key_pattern() {
        let cases = [
            "sk-abc123def456ghi789jkl012mno345pqr678stu901vwx",
            "MY API_KEY = hunter2-hunter2-hunter2-hunter2-hunter2-hunter2",
            "password: correcthorsebatterystaple",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxw",
            "-----BEGIN PRIVATE KEY-----",
            // JWT-shaped: 3 base64-url segments, each >= 4 chars.
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
            // Long base64 run: 40+ contiguous chars.
            "blob: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ];
        for c in cases {
            assert!(
                sensitive_text_predicate(c),
                "expected `{c}` to be flagged sensitive"
            );
        }
    }

    #[test]
    fn is_sensitive_ignores_normal_prose() {
        let cases = [
            "the quick brown fox jumps over the lazy dog",
            "今天午饭吃了红烧排骨",
            "function add(a, b) { return a + b; }",
            // Short base64-ish run (under 40 chars).
            "abc123",
        ];
        for c in cases {
            assert!(
                !sensitive_text_predicate(c),
                "expected `{c}` to NOT be flagged sensitive"
            );
        }
    }

    #[test]
    fn memory_is_sensitive_delegates_to_predicate() {
        let m = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            "MY_API_KEY=sk-abc",
            SourceKind::UserInput,
        );
        assert!(m.is_sensitive());
        let m2 = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            "the cat sat on the mat",
            SourceKind::UserInput,
        );
        assert!(!m2.is_sensitive());
    }
}
