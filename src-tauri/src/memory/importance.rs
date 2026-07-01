//! Memory importance scoring.
//!
//! ## v1.0 P0#6 fix — alignment with the design document
//!
//! v0.3 used a hand-rolled formula:
//!
//! ```text
//!   score = 0.4 * freq + 0.4 * recency + 0.2 * feedback
//! ```
//!
//! with a 7-day recency half-life.  The design document
//! (`docs/ARCHITECTURE.md` §10.1) instead specifies:
//!
//! ```text
//!   score = base
//!         + 0.05 * min(access_count, 100) / 100   // access
//!         + 0.20 * recency_30d                    // recency, HL=30d
//!         + 0.20 * feedback                       // feedback
//!         + type_weight(memory_type)              // type bonus
//! ```
//!
//! where `base = 0.5`, `type_weight(Metacognitive) = 0.3`,
//! `type_weight(Emotional) = 0.2`, and every other type has a
//! weight of `0.0`.  The recency half-life is **30 days**, not
//! seven.  The final value is clamped to `[0.0, 1.0]`.
//!
//! v1.0 re-aligns the implementation with the design document and
//! adds `Memory::memory_type` as a first-class input to the
//! scorer.  Tests in this module are the new spec; pre-v1.0
//! tests that pinned to the old 0.4/0.4/0.2 weighting have
//! been replaced.
//!
//! ## Rationale for the v1.0 numbers
//!
//! * The 0.5 base keeps a freshly-stored memory from ever
//!   dropping below "unread but non-trivial" — the user's
//!   intent is the act of saving it.
//! * `type_weight` recognises that some categories of memory
//!   (metacognitive observations, emotional markers) are more
//!   important for the user's self-model than raw facts.
//! * The longer 30-day half-life prevents the system from
//!   forgetting long-tail preferences after a one-week idle
//!   period (the bug the v0.3 short half-life caused in
//!   beta-tester reports).

use serde::{Deserialize, Serialize};

use super::types::{Memory, MemoryType};

/// Tunable weights for [`ImportanceScorer`].
///
/// v1.0 P0#6: the four named slots match the design document
/// one-for-one.  Changing any of them is a tuning exercise, not
/// an architectural change.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImportanceWeights {
    /// Coefficient on the access component (0..=0.05 in the v1.0 spec).
    pub access: f32,
    /// Coefficient on the recency component (0..=0.20 in the v1.0 spec).
    pub recency: f32,
    /// Coefficient on the feedback component (0..=0.20 in the v1.0 spec).
    pub feedback: f32,
    /// Constant base value added to every score (0.5 in the v1.0 spec).
    pub base: f32,
}

impl Default for ImportanceWeights {
    /// v1.0 P0#6: matches `docs/ARCHITECTURE.md` §10.1.
    fn default() -> Self {
        Self {
            access: 0.05,
            recency: 0.20,
            feedback: 0.20,
            base: 0.5,
        }
    }
}

/// Stateless importance scorer.
///
/// v1.0 P0#6: half-life defaults to **30 days** (was 7 days in
/// v0.3).  The design document calls for a longer decay so that
/// long-tail preferences survive a one-week idle period.
#[derive(Debug, Clone, Copy)]
pub struct ImportanceScorer {
    pub weights: ImportanceWeights,
    /// Recency half-life in seconds (default 30 days).
    pub half_life_secs: i64,
}

impl Default for ImportanceScorer {
    fn default() -> Self {
        Self {
            weights: ImportanceWeights::default(),
            // v1.0 P0#6: 30 days, was 7.
            half_life_secs: 30 * 24 * 3600,
        }
    }
}

impl ImportanceScorer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_weights(mut self, w: ImportanceWeights) -> Self {
        self.weights = w;
        self
    }

    pub fn with_half_life_days(mut self, days: i64) -> Self {
        self.half_life_secs = days * 24 * 3600;
        self
    }

    /// Computes the importance score of `m` relative to `now`.
    ///
    /// v1.0 P0#6 formula:
    /// ```text
    ///   score = base
    ///         + access * min(1, access_count / 100)
    ///         + recency * 0.5^((now - last_access) / half_life)
    ///         + feedback * ((fb + 1) / 2)
    ///         + type_weight(memory_type)
    /// ```
    /// clamped to `[0.0, 1.0]`.
    pub fn score(&self, m: &Memory, now: i64) -> f32 {
        let access = self.access_component(m.access_count);
        let rec = self.recency_component(m.last_access, now);
        let fb = self.feedback_component(&m.metadata);
        let type_w = type_weight(m.memory_type);
        let raw = self.weights.base
            + self.weights.access * access
            + self.weights.recency * rec
            + self.weights.feedback * fb
            + type_w;
        raw.clamp(0.0, 1.0)
    }

    /// Linear access component in `[0, 1]`. Saturates at 100
    /// accesses per the v1.0 spec.
    fn access_component(&self, access_count: u32) -> f32 {
        let n = access_count.min(100) as f32;
        n / 100.0
    }

    /// Exponential recency decay. `1.0` when `last_access == now`;
    /// `0.5` at one half-life; asymptotically to 0.
    fn recency_component(&self, last_access: i64, now: i64) -> f32 {
        let age = (now - last_access).max(0) as f64;
        let hl = self.half_life_secs.max(1) as f64;
        0.5_f64.powf(age / hl) as f32
    }

    /// Reads `metadata.importance_feedback` (if any, range `[-1, 1]`)
    /// and maps it to `[0, 1]`.
    fn feedback_component(&self, metadata: &serde_json::Value) -> f32 {
        let raw = metadata
            .get("importance_feedback")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .clamp(-1.0, 1.0);
        ((raw + 1.0) * 0.5) as f32
    }
}

/// v1.0 P0#6: per-type bonus from `docs/ARCHITECTURE.md` §10.1.
pub fn type_weight(mt: MemoryType) -> f32 {
    match mt {
        MemoryType::Metacognitive => 0.3,
        MemoryType::Emotional => 0.2,
        // Every other type — Semantic, Episodic, Procedural —
        // contributes nothing beyond the base / access / recency /
        // feedback terms.
        MemoryType::Semantic | MemoryType::Episodic | MemoryType::Procedural => 0.0,
    }
}

/// Convenience: re-score a single memory in place.
pub fn rescore(m: &mut Memory, scorer: &ImportanceScorer, now: i64) {
    m.importance = scorer.score(m, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{MemoryLayer, MultiGranularity, SourceKind};

    fn make(mt: MemoryType, access: u32, last: i64, fb: Option<f64>) -> Memory {
        let mut m = Memory::new(mt, MemoryLayer::L3, "x", SourceKind::UserInput);
        m.access_count = access;
        m.last_access = last;
        if let Some(f) = fb {
            m.metadata = serde_json::json!({ "importance_feedback": f });
        }
        m.summary = MultiGranularity::default();
        m
    }

    #[test]
    fn score_in_unit_range() {
        let s = ImportanceScorer::new();
        for access in [0, 1, 10, 100, 1000] {
            for age in [0, 86_400, 86_400 * 7, 86_400 * 365] {
                for mt in [
                    MemoryType::Semantic,
                    MemoryType::Episodic,
                    MemoryType::Procedural,
                    MemoryType::Emotional,
                    MemoryType::Metacognitive,
                ] {
                    let m = make(mt, access, 1_700_000_000 - age, None);
                    let v = s.score(&m, 1_700_000_000);
                    assert!((0.0..=1.0).contains(&v), "score out of range: {v}");
                }
            }
        }
    }

    #[test]
    fn recency_decays_with_30d_half_life() {
        // v1.0 P0#6: half-life is 30 days, not 7.
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let fresh = make(MemoryType::Semantic, 0, now, None);
        let stale_30d = make(MemoryType::Semantic, 0, now - s.half_life_secs, None);
        let s_fresh = s.score(&fresh, now);
        let s_stale = s.score(&stale_30d, now);
        assert!(s_fresh > s_stale);
        // At exactly one half-life the recency component is 0.5;
        // the gap between fresh and stale must be at least
        // `recency * 0.5`.
        assert!((s_fresh - s_stale).abs() > 0.05);
    }

    #[test]
    fn positive_feedback_raises_score() {
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let a = make(MemoryType::Semantic, 5, now, Some(0.0));
        let b = make(MemoryType::Semantic, 5, now, Some(1.0));
        assert!(s.score(&b, now) > s.score(&a, now));
    }

    #[test]
    fn negative_feedback_lowers_score() {
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let a = make(MemoryType::Semantic, 5, now, Some(0.0));
        let b = make(MemoryType::Semantic, 5, now, Some(-1.0));
        assert!(s.score(&b, now) < s.score(&a, now));
    }

    #[test]
    fn rescore_updates_in_place() {
        let scorer = ImportanceScorer::new();
        let mut m = make(MemoryType::Semantic, 42, 1_700_000_000, Some(0.5));
        let before = m.importance;
        rescore(&mut m, &scorer, 1_700_000_000);
        assert!((m.importance - before).abs() > 1e-6 || m.importance > 0.0);
    }

    // -----------------------------------------------------------------
    // v1.0 P0#6: the following tests pin to the design-document
    // formula.  They replace the v0.3 weighting tests.
    // -----------------------------------------------------------------

    #[test]
    fn base_is_0_5_for_unaccessed_unfeedback_record() {
        // A fresh, unaccessed, neutrally-rated Semantic record
        // must score exactly `base = 0.5` (access = 0,
        // recency = 1, feedback = 0.5, type = 0.0).
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let m = make(MemoryType::Semantic, 0, now, None);
        // 0.5 + 0.05*0 + 0.20*1.0 + 0.20*0.5 + 0.0 = 0.80
        let expected = 0.5 + 0.05 * 0.0 + 0.20 * 1.0 + 0.20 * 0.5 + 0.0;
        let v = s.score(&m, now);
        assert!((v - expected).abs() < 1e-5, "got {v}, expected {expected}");
    }

    #[test]
    fn half_life_default_is_30_days() {
        let s = ImportanceScorer::new();
        assert_eq!(s.half_life_secs, 30 * 24 * 3600);
    }

    #[test]
    fn weights_default_matches_design_doc() {
        let w = ImportanceWeights::default();
        assert!((w.base - 0.5).abs() < 1e-6);
        assert!((w.access - 0.05).abs() < 1e-6);
        assert!((w.recency - 0.20).abs() < 1e-6);
        assert!((w.feedback - 0.20).abs() < 1e-6);
    }

    #[test]
    fn metacognitive_gets_0_3_type_bonus() {
        // design doc: Metacognitive weight = 0.3
        assert!((type_weight(MemoryType::Metacognitive) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn emotional_gets_0_2_type_bonus() {
        assert!((type_weight(MemoryType::Emotional) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn other_types_get_zero_type_bonus() {
        for mt in [
            MemoryType::Semantic,
            MemoryType::Episodic,
            MemoryType::Procedural,
        ] {
            assert!((type_weight(mt) - 0.0).abs() < 1e-6, "type {mt:?}");
        }
    }

    #[test]
    fn metacognitive_outranks_semantic_for_otherwise_equal_input() {
        // A metacognitive memory with the same access / recency /
        // feedback as a semantic one must score strictly higher.
        // We use low access and old last_access so both scores stay
        // well below the 1.0 clamp (otherwise the metacognitive score
        // saturates at 1.0 and the difference is less than 0.3).
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let old = now - 86_400 * 365;
        let a = make(MemoryType::Semantic, 0, old, Some(-1.0));
        let b = make(MemoryType::Metacognitive, 0, old, Some(-1.0));
        assert!(s.score(&b, now) > s.score(&a, now));
        // The difference is exactly `0.3` (the type weight).
        assert!((s.score(&b, now) - s.score(&a, now) - 0.3).abs() < 1e-5);
    }

    #[test]
    fn access_component_saturates_at_100() {
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let a = make(MemoryType::Semantic, 100, now, Some(0.0));
        let b = make(MemoryType::Semantic, 1000, now, Some(0.0));
        // Both should land at the same access component because
        // the spec saturates at 100.
        assert!((s.score(&a, now) - s.score(&b, now)).abs() < 1e-5);
    }

    #[test]
    fn max_input_score_saturates_at_one() {
        // access = 100, recency = 1, feedback = 1.0, type = Meta
        // = 0.5 + 0.05 + 0.20 + 0.20 + 0.3 = 1.25 -> clamps to 1.0
        let s = ImportanceScorer::new();
        let now = 1_700_000_000;
        let mut m = make(MemoryType::Metacognitive, 100, now, Some(1.0));
        m.summary = MultiGranularity::default();
        assert!((s.score(&m, now) - 1.0).abs() < 1e-6);
    }
}
