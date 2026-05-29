//! Hysteresis state-machine step extracted from engine state.rs:258-296.
//! Takes `now` explicitly so replay CLI is deterministic.

use chrono::{DateTime, Utc};
use pegana_common_verify::PegState;
use serde::{Deserialize, Serialize};

/// Result of one hysteresis step. The caller writes these back into its
/// per-asset cache (in engine: `EngineState::AssetRt`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionDecision {
    pub new_last_state: PegState,
    pub new_candidate_state: Option<PegState>,
    pub new_candidate_since: Option<DateTime<Utc>>,
}

/// Apply hysteresis: a stricter `candidate` state must persist for
/// `confirm_up_secs` before becoming the new committed state; a relaxation
/// toward PEGGED must hold for `decay_down_secs`. Threshold flapping inside
/// either window is suppressed.
///
/// `now` is explicit (not `Utc::now()`) so the replay CLI can reproduce the
/// exact decision from the receipt's frozen inputs. This was a real
/// determinism bug in the pre-extraction engine — see ADR-0010 and the
/// "explicit now" task notes in `plans/03-phase-2-engine.md`.
pub fn transition_decide(
    current_last_state: PegState,
    candidate: PegState,
    prev_candidate_state: Option<PegState>,
    prev_candidate_since: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    confirm_up_secs: i64,
    decay_down_secs: i64,
) -> TransitionDecision {
    let strictness = state_strictness(candidate);
    let current_strictness = state_strictness(current_last_state);

    // Case 1: candidate matches the committed state. Clear any in-flight
    // candidate; nothing to confirm.
    if candidate == current_last_state {
        return TransitionDecision {
            new_last_state: current_last_state,
            new_candidate_state: None,
            new_candidate_since: None,
        };
    }

    // Case 2: a different candidate from the one we were tracking → reset
    // the timer. Either it's new entirely or the prior candidate matured
    // into something else.
    if prev_candidate_state != Some(candidate) {
        return TransitionDecision {
            new_last_state: current_last_state,
            new_candidate_state: Some(candidate),
            new_candidate_since: Some(now),
        };
    }

    // Case 3: same candidate as before. Compare elapsed time vs the
    // appropriate window — stricter requires confirm_up_secs, looser
    // (towards PEGGED) requires decay_down_secs.
    let since = prev_candidate_since.unwrap_or(now);
    let needed = if strictness > current_strictness {
        confirm_up_secs
    } else {
        decay_down_secs
    };
    if (now - since).num_seconds() >= needed {
        TransitionDecision {
            new_last_state: candidate,
            new_candidate_state: None,
            new_candidate_since: None,
        }
    } else {
        TransitionDecision {
            new_last_state: current_last_state,
            new_candidate_state: prev_candidate_state,
            new_candidate_since: prev_candidate_since,
        }
    }
}

fn state_strictness(s: PegState) -> u8 {
    match s {
        PegState::Pegged => 0,
        PegState::Drift => 1,
        PegState::Depeg => 2,
        PegState::Critical => 3,
        PegState::BlackSwan => 4,
        PegState::Unknown => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t0() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn no_op_when_candidate_equals_current() {
        let now = t0();
        let r = transition_decide(PegState::Drift, PegState::Drift, None, None, now, 30, 120);
        assert_eq!(r.new_last_state, PegState::Drift);
        assert_eq!(r.new_candidate_state, None);
    }

    #[test]
    fn new_candidate_does_not_flip_immediately() {
        let now = t0();
        let r = transition_decide(PegState::Pegged, PegState::Drift, None, None, now, 30, 120);
        assert_eq!(r.new_last_state, PegState::Pegged);
        assert_eq!(r.new_candidate_state, Some(PegState::Drift));
        assert_eq!(r.new_candidate_since, Some(now));
    }

    #[test]
    fn candidate_matures_after_confirm_up_secs() {
        let now = t0();
        let since = now - Duration::seconds(31);
        let r = transition_decide(
            PegState::Pegged,
            PegState::Drift,
            Some(PegState::Drift),
            Some(since),
            now,
            30,
            120,
        );
        assert_eq!(r.new_last_state, PegState::Drift);
    }

    #[test]
    fn downward_uses_decay_window() {
        let now = t0();
        let since = now - Duration::seconds(60); // < decay window 120s
        let r = transition_decide(
            PegState::Drift,
            PegState::Pegged,
            Some(PegState::Pegged),
            Some(since),
            now,
            30,
            120,
        );
        assert_eq!(r.new_last_state, PegState::Drift, "must wait full decay");

        let since2 = now - Duration::seconds(121); // > 120s
        let r2 = transition_decide(
            PegState::Drift,
            PegState::Pegged,
            Some(PegState::Pegged),
            Some(since2),
            now,
            30,
            120,
        );
        assert_eq!(r2.new_last_state, PegState::Pegged);
    }
}
