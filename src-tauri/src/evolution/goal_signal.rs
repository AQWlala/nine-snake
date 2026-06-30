//! Goal signal derivation — the "objective function" that drives
//! self-evolution.
//!
//! Current shape:
//!   * win_rate(outcomes) = count(confidence >= threshold) / count(*)
//!   * rolling_win_rate(source_id, n=50) — last 50 outcomes for an
//!     agent / skill, used by the background worker.
//!
//! These numbers feed prompts/mutations and the archive policy.  The
//! full fitness function (cost, time, satisfaction) is described in
//! `docs/ARCHITECTURE.md` and will be expanded in v1.4.  v1.3 ships
//! the simplest useful version: confidence-based win rate.

use crate::evolution::outcome::{Outcome, OutcomeSource};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalSignal {
    /// 0.0 - 1.0
    pub win_rate: f32,
    /// sample size
    pub n: usize,
}

pub fn win_rate(outcomes: &[Outcome], confidence_threshold: f32) -> GoalSignal {
    let n = outcomes.len();
    if n == 0 {
        return GoalSignal {
            win_rate: 0.0,
            n: 0,
        };
    }
    let wins = outcomes
        .iter()
        .filter(|o| o.confidence >= confidence_threshold)
        .count();
    GoalSignal {
        win_rate: wins as f32 / n as f32,
        n,
    }
}

pub fn confidence_mean(outcomes: &[Outcome]) -> f32 {
    if outcomes.is_empty() {
        0.0
    } else {
        outcomes.iter().map(|o| o.confidence).sum::<f32>() / outcomes.len() as f32
    }
}

pub fn has_regressed(baseline: &[Outcome], recent: &[Outcome], threshold_pct: f32) -> bool {
    let baseline_mean = confidence_mean(baseline);
    let recent_mean = confidence_mean(recent);
    if baseline_mean == 0.0 {
        return false;
    }
    let drop_pct = ((baseline_mean - recent_mean) / baseline_mean) * 100.0;
    drop_pct >= threshold_pct
}

/// Convenience: filter outcomes to a single source + source_id pair
/// and return them newest-first.
pub fn filter_by<'a>(
    outcomes: &'a [Outcome],
    source: OutcomeSource,
    source_id: &str,
) -> Vec<&'a Outcome> {
    let mut out: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.source == source && o.source_id == source_id)
        .collect();
    out.sort_by_key(|o| std::cmp::Reverse(o.created_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::outcome::{OutcomeSource, OutcomeStatus};

    fn make_outcome(
        id: &str,
        source: OutcomeSource,
        source_id: &str,
        conf: f32,
        t: i64,
    ) -> Outcome {
        Outcome {
            id: id.into(),
            source_id: source_id.into(),
            source,
            status: OutcomeStatus::Success,
            confidence: conf,
            error: "".into(),
            duration_ms: 0,
            created_at: t,
        }
    }

    #[test]
    fn win_rate_all_wins() {
        let outs: Vec<Outcome> = (0..4)
            .map(|i| make_outcome(&format!("o{i}"), OutcomeSource::Skill, "s", 0.9, i))
            .collect();
        assert_eq!(win_rate(&outs, 0.7).win_rate, 1.0);
        assert_eq!(win_rate(&outs, 0.7).n, 4);
    }

    #[test]
    fn win_rate_half() {
        let mut outs: Vec<Outcome> = Vec::new();
        for i in 0..2 {
            outs.push(make_outcome(
                &format!("o{i}"),
                OutcomeSource::Skill,
                "s",
                0.9,
                i,
            ));
        }
        for i in 2..4 {
            outs.push(make_outcome(
                &format!("o{i}"),
                OutcomeSource::Skill,
                "s",
                0.4,
                i,
            ));
        }
        let g = win_rate(&outs, 0.7);
        assert!((g.win_rate - 0.5).abs() < 0.001);
    }

    #[test]
    fn win_rate_empty() {
        let g = win_rate(&[], 0.7);
        assert_eq!(g.win_rate, 0.0);
        assert_eq!(g.n, 0);
    }

    #[test]
    fn confidence_mean_typical() {
        let outs: Vec<Outcome> = (0..3)
            .map(|i| make_outcome(&format!("o{i}"), OutcomeSource::Swarm, "coder", 0.6, i))
            .collect();
        assert!((confidence_mean(&outs) - 0.6).abs() < 0.001);
    }

    #[test]
    fn has_regressed_signals_drop() {
        let baseline: Vec<Outcome> = (0..3)
            .map(|i| make_outcome(&format!("b{i}"), OutcomeSource::Swarm, "coder", 1.0, i))
            .collect();
        let recent: Vec<Outcome> = (0..3)
            .map(|i| make_outcome(&format!("r{i}"), OutcomeSource::Swarm, "coder", 0.7, i))
            .collect();
        assert!(has_regressed(&baseline, &recent, 25.0)); // 30% drop
        assert!(!has_regressed(&baseline, &recent, 50.0)); // 30% drop not enough
    }

    #[test]
    fn filter_by_returns_only_matching_sorted() {
        let outs: Vec<Outcome> = vec![
            make_outcome("o1", OutcomeSource::Skill, "a", 0.9, 1),
            make_outcome("o2", OutcomeSource::Skill, "b", 0.9, 2),
            make_outcome("o3", OutcomeSource::Skill, "a", 0.9, 5),
        ];
        let f = filter_by(&outs, OutcomeSource::Skill, "a");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].id, "o3"); // newest first
        assert_eq!(f[1].id, "o1");
    }
}
