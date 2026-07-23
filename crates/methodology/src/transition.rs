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

    let escalating = strictness > current_strictness;

    // The confirm/decay window measures continuous time spent on the SAME SIDE
    // of the committed state (escalation vs relaxation), NOT time at one exact
    // band. Reset the timer ONLY when there is no prior candidate on the same
    // side; a same-side hop keeps the window running and commits to the CURRENT
    // band once it matures.
    //
    // Case 2 used to reset on ANY candidate change. That silently broke on a
    // discount oscillating between two ALERTING bands (e.g. DRIFT↔DEPEG, both
    // stricter than a PEGGED commit): each tick's candidate differed from the
    // last, so the timer reset every tick and NEITHER band ever confirmed — the
    // engine held/published PEGGED indefinitely while the asset was continuously
    // unhealthy (audit 2026-07-18). The deadband fix (ADR-0021/0023) only smooths
    // oscillation at the single PEGGED↔first-band boundary; between-band flapping
    // higher up was never covered. Same-side continuity closes it.
    let same_side = match prev_candidate_state {
        Some(prev) => (state_strictness(prev) > current_strictness) == escalating,
        None => false,
    };

    // Case 2: no prior candidate on this side (first sighting, or the side just
    // flipped) → start a fresh window. Never commits immediately (guards
    // no_spontaneous_escalation_on_timer_reset).
    if !same_side {
        return TransitionDecision {
            new_last_state: current_last_state,
            new_candidate_state: Some(candidate),
            new_candidate_since: Some(now),
        };
    }

    // Case 3: same side as the tracked candidate → the window keeps running.
    // Escalation needs confirm_up_secs, relaxation (towards PEGGED) decay_down_secs.
    // Commit to the CURRENT candidate once matured; else hold, but track the
    // current band so maturity commits to where the signal is NOW.
    let since = prev_candidate_since.unwrap_or(now);
    let needed = if escalating {
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
            new_candidate_state: Some(candidate),
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
    use proptest::prelude::*;

    // ── Proptest helpers ──────────────────────────────────────────────────────

    /// All PegState variants for strategy composition.
    fn arb_peg_state() -> impl Strategy<Value = PegState> {
        prop_oneof![
            Just(PegState::Pegged),
            Just(PegState::Drift),
            Just(PegState::Depeg),
            Just(PegState::Critical),
            Just(PegState::BlackSwan),
            Just(PegState::Unknown),
        ]
    }

    /// Produce a DateTime<Utc> anchored to a fixed epoch plus an offset in
    /// `[0, range_secs)`.  Using a fixed epoch makes test output reproducible
    /// across time zones and DST changes.
    fn base_epoch() -> DateTime<Utc> {
        // 2024-01-01T00:00:00Z — arbitrary but stable.
        DateTime::from_timestamp(1_704_067_200, 0).unwrap()
    }

    proptest! {
        /// DETERMINISM: identical inputs must produce identical outputs.
        #[test]
        fn determinism(
            current in arb_peg_state(),
            candidate in arb_peg_state(),
            has_prev_candidate in any::<bool>(),
            prev_state_idx in 0usize..6,
            since_offset in 0i64..=3600,
            now_offset  in 0i64..=3600,
            confirm_up  in 1i64..=600,
            decay_down  in 1i64..=600,
        ) {
            let epoch = base_epoch();
            let now   = epoch + Duration::seconds(now_offset);

            let (prev_candidate_state, prev_candidate_since) = if has_prev_candidate {
                let states = [
                    PegState::Pegged, PegState::Drift, PegState::Depeg,
                    PegState::Critical, PegState::BlackSwan, PegState::Unknown,
                ];
                // candidate_since must be <= now (monotonic-time invariant)
                let since = epoch + Duration::seconds(since_offset.min(now_offset));
                (Some(states[prev_state_idx % 6]), Some(since))
            } else {
                (None, None)
            };

            let r1 = transition_decide(
                current, candidate, prev_candidate_state, prev_candidate_since,
                now, confirm_up, decay_down,
            );
            let r2 = transition_decide(
                current, candidate, prev_candidate_state, prev_candidate_since,
                now, confirm_up, decay_down,
            );
            prop_assert_eq!(r1, r2, "same inputs must produce the same output");
        }

        /// NO-SPONTANEOUS-ESCALATION: a brand-new candidate (timer just
        /// started) must never immediately commit into a stricter state.
        /// This only applies when a different candidate is presented (no prior
        /// matching candidate), so the FSM is in the "timer reset" path.
        #[test]
        fn no_spontaneous_escalation_on_timer_reset(
            current in arb_peg_state(),
            candidate in arb_peg_state(),
            now_offset in 0i64..=3600,
            confirm_up in 1i64..=600,
            decay_down in 1i64..=600,
        ) {
            // Present a candidate with no prior matching tracked state —
            // the FSM must NOT commit a stricter state immediately.
            let now = base_epoch() + Duration::seconds(now_offset);

            let r = transition_decide(
                current,
                candidate,
                // no prior candidate at all — forces Case 2 (timer reset)
                None,
                None,
                now,
                confirm_up,
                decay_down,
            );

            // If the candidate matches current, it's a no-op (Case 1) and
            // new_last_state = current regardless, which is fine.
            if candidate == current {
                prop_assert_eq!(r.new_last_state, current);
            } else {
                // Case 2: timer just started.  The committed state must NOT
                // escalate immediately beyond current.
                let escalated = state_strictness(r.new_last_state)
                    > state_strictness(current);
                prop_assert!(
                    !escalated,
                    "spontaneous escalation: current={:?} candidate={:?} → new_last_state={:?}",
                    current, candidate, r.new_last_state
                );
                // The candidate must be recorded.
                prop_assert_eq!(r.new_candidate_state, Some(candidate));
            }
        }

        /// NO-ESCALATION-BEFORE-CONFIRM-WINDOW: when the same candidate has
        /// been tracked but the elapsed time is strictly less than confirm_up,
        /// the committed state must not move to a stricter level.
        #[test]
        fn no_escalation_before_window(
            current in arb_peg_state(),
            candidate in arb_peg_state(),
            confirm_up in 2i64..=600,
            decay_down in 1i64..=600,
            // elapsed in [0, confirm_up - 1]
            elapsed in 0i64..=598,
        ) {
            // Ensure candidate is strictly more severe than current so we are
            // on the escalation path; skip when they are equal or candidate is
            // looser (those are covered by different invariants).
            prop_assume!(state_strictness(candidate) > state_strictness(current));
            // Clamp elapsed to [0, confirm_up - 1].
            let elapsed = elapsed % confirm_up; // always < confirm_up

            let epoch = base_epoch();
            let since = epoch;
            let now   = epoch + Duration::seconds(elapsed);

            let r = transition_decide(
                current,
                candidate,
                Some(candidate), // same candidate being tracked
                Some(since),
                now,
                confirm_up,
                decay_down,
            );

            prop_assert_eq!(
                r.new_last_state, current,
                "must NOT commit {:?} before confirm_up ({} secs); elapsed={}",
                candidate, confirm_up, elapsed
            );
        }

        /// ESCALATION-COMPLETES-AFTER-WINDOW: when elapsed >= confirm_up the
        /// strict candidate must be committed.
        #[test]
        fn escalation_completes_after_window(
            current in arb_peg_state(),
            candidate in arb_peg_state(),
            confirm_up in 1i64..=600,
            decay_down in 1i64..=600,
            // elapsed in [confirm_up, confirm_up + 3600]
            extra in 0i64..=3600,
        ) {
            prop_assume!(state_strictness(candidate) > state_strictness(current));

            let epoch = base_epoch();
            let since = epoch;
            let elapsed = confirm_up + extra; // always >= confirm_up
            let now = epoch + Duration::seconds(elapsed);

            let r = transition_decide(
                current,
                candidate,
                Some(candidate),
                Some(since),
                now,
                confirm_up,
                decay_down,
            );

            prop_assert_eq!(
                r.new_last_state, candidate,
                "must commit {:?} once elapsed ({}) >= confirm_up ({})",
                candidate, elapsed, confirm_up
            );
        }

        /// MONOTONIC-TIME INVARIANT: candidate_since must be <= now.
        /// The FSM should never receive a future since; this property ensures
        /// the code handles the degenerate case gracefully (no panic, just
        /// keeps the current state).
        #[test]
        fn candidate_since_not_in_future_for_maturation(
            current in arb_peg_state(),
            candidate in arb_peg_state(),
            confirm_up in 1i64..=600,
            decay_down in 1i64..=600,
            // since > now: elapsed will be negative → must not commit
            extra in 1i64..=3600,
        ) {
            prop_assume!(state_strictness(candidate) > state_strictness(current));

            let epoch = base_epoch();
            let now   = epoch;
            // since is in the FUTURE relative to now
            let since = epoch + Duration::seconds(extra);

            let r = transition_decide(
                current,
                candidate,
                Some(candidate),
                Some(since),
                now,
                confirm_up,
                decay_down,
            );

            // elapsed is negative → num_seconds() < needed → must NOT commit.
            prop_assert_eq!(
                r.new_last_state, current,
                "future candidate_since must not trigger maturation"
            );
        }
    }

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

    /// REGRESSION (audit 2026-07-18): a discount oscillating between two
    /// ALERTING bands (DRIFT↔DEPEG, both stricter than a PEGGED commit) must
    /// eventually commit an alerting state, not hold PEGGED forever. Before the
    /// same-side-continuity fix, each tick's candidate differed from the last so
    /// the confirm timer reset every tick and NEITHER band matured — the engine
    /// held PEGGED indefinitely while the asset was continuously unhealthy.
    #[test]
    fn oscillation_between_alerting_bands_still_commits() {
        let epoch = base_epoch();
        let mut committed = PegState::Pegged;
        let mut cand_state: Option<PegState> = None;
        let mut cand_since: Option<DateTime<Utc>> = None;
        // 20 ticks, 5s apart, alternating DRIFT/DEPEG — 100s of continuous
        // alerting-band flapping (confirm_up = 30s).
        for i in 0..20i64 {
            let candidate = if i % 2 == 0 {
                PegState::Drift
            } else {
                PegState::Depeg
            };
            let now = epoch + Duration::seconds(i * 5);
            let r = transition_decide(committed, candidate, cand_state, cand_since, now, 30, 120);
            committed = r.new_last_state;
            cand_state = r.new_candidate_state;
            cand_since = r.new_candidate_since;
        }
        assert_ne!(
            committed,
            PegState::Pegged,
            "oscillation between alerting bands must not hold PEGGED (the bug)"
        );
        assert!(
            state_strictness(committed) >= state_strictness(PegState::Drift),
            "committed {committed:?} must be at least DRIFT"
        );
    }
}
