//! Behavioural policies for the 8 memory layers (L0..L7).
//!
//! Each layer has its own retention, summarisation and sharing
//! characteristics. The [`LayerPolicy`] struct encapsulates the rules and
//! is consulted by the sponge, the black-hole engine, and the LLM
//! gateway when constructing context windows.

use serde::{Deserialize, Serialize};

use super::types::MemoryLayer;

const PROMOTE_L3_ACCESS_THRESHOLD: u32 = 10;
const PROMOTE_L3_IMPORTANCE_THRESHOLD: f32 = 0.7;
const PROMOTE_L4_ACCESS_THRESHOLD: u32 = 20;
const PROMOTE_L4_IMPORTANCE_THRESHOLD: f32 = 0.8;

/// Static policy describing how a memory layer should be treated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerPolicy {
    /// Layer this policy applies to.
    pub layer: MemoryLayer,
    /// Human-readable description.
    pub description: &'static str,
    /// Default TTL in days; 0 means "no auto-eviction".
    pub ttl_days: u32,
    /// Whether the layer participates in embedding-based retrieval.
    pub searchable: bool,
    /// Whether the layer is shared with peer agents in a swarm.
    pub shared: bool,
    /// Whether the black-hole engine may compress this layer.
    pub compressible: bool,
    /// Multi-granularity bucket the sponge should use as the default.
    pub default_summary_bucket: usize,
}

const fn policy(
    layer: MemoryLayer,
    description: &'static str,
    ttl_days: u32,
    searchable: bool,
    shared: bool,
    compressible: bool,
    default_summary_bucket: usize,
) -> LayerPolicy {
    LayerPolicy {
        layer,
        description,
        ttl_days,
        searchable,
        shared,
        compressible,
        default_summary_bucket,
    }
}

/// Returns the policy for a given layer.
pub fn policy_for(layer: MemoryLayer) -> LayerPolicy {
    match layer {
        MemoryLayer::L0 => policy(
            MemoryLayer::L0,
            "Ephemeral cache. Single conversation turn.",
            0,
            false,
            false,
            true,
            0,
        ),
        MemoryLayer::L1 => policy(
            MemoryLayer::L1,
            "Rolling message history within a session.",
            1,
            true,
            true,
            true,
            0,
        ),
        MemoryLayer::L2 => policy(
            MemoryLayer::L2,
            "Cross-session experience.",
            7,
            true,
            true,
            true,
            1,
        ),
        MemoryLayer::L3 => policy(MemoryLayer::L3, "Concrete facts.", 30, true, true, true, 1),
        MemoryLayer::L4 => policy(
            MemoryLayer::L4,
            "Distilled knowledge.",
            90,
            true,
            true,
            true,
            2,
        ),
        MemoryLayer::L5 => policy(
            MemoryLayer::L5,
            "Lessons learned from mistakes.",
            365,
            true,
            true,
            true,
            2,
        ),
        MemoryLayer::L6 => policy(
            MemoryLayer::L6,
            "Re-usable principles.",
            0,
            true,
            true,
            true,
            3,
        ),
        MemoryLayer::L7 => policy(
            MemoryLayer::L7,
            "Singularity — core values, never compressed.",
            0,
            true,
            true,
            false,
            3,
        ),
    }
}

/// Returns true if the layer is allowed to participate in the standard
/// LLM context window.
pub fn in_context_window(layer: MemoryLayer) -> bool {
    matches!(
        layer,
        MemoryLayer::L1 | MemoryLayer::L2 | MemoryLayer::L3 | MemoryLayer::L4 | MemoryLayer::L6
    )
}

pub fn check_auto_promote(
    current_layer: MemoryLayer,
    access_count: u32,
    importance: f32,
    pinned: bool,
) -> Option<MemoryLayer> {
    if pinned {
        return None;
    }
    match current_layer {
        MemoryLayer::L3 => {
            if access_count > PROMOTE_L3_ACCESS_THRESHOLD
                && importance > PROMOTE_L3_IMPORTANCE_THRESHOLD
            {
                return Some(MemoryLayer::L4);
            }
        }
        MemoryLayer::L4 => {
            if access_count > PROMOTE_L4_ACCESS_THRESHOLD
                && importance > PROMOTE_L4_IMPORTANCE_THRESHOLD
            {
                return Some(MemoryLayer::L6);
            }
        }
        MemoryLayer::L7 => return None,
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l7_is_incompressible() {
        assert!(!policy_for(MemoryLayer::L7).compressible);
    }

    #[test]
    fn l0_is_not_searchable() {
        assert!(!policy_for(MemoryLayer::L0).searchable);
    }

    #[test]
    fn context_window_policy_matches_spec() {
        assert!(in_context_window(MemoryLayer::L1));
        assert!(!in_context_window(MemoryLayer::L0));
        assert!(!in_context_window(MemoryLayer::L5));
        assert!(!in_context_window(MemoryLayer::L7));
    }
}
