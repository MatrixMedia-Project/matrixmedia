//! Player-reported viewability signals, judged against the MRC video rule.
//!
//! An ad is *served* when it starts rendering; it is *viewable* only when the
//! player reports the ad had a real opportunity to be seen. The MRC video
//! rule (see `MM_ADS_PLAN.md` §1):
//!
//! - at least **50% of the ad's pixels in view**, continuously for
//! - at least **2 seconds**.
//!
//! The player (OM-SDK-shaped geometry tracking) reports coverage and the
//! unbroken visible span; the server applies the threshold. The client never
//! self-declares "viewable": [`meets_mrc_viewable`] is the single place the
//! rule lives. Audibility is recorded for attention reporting but is NOT part
//! of the MRC video threshold.
//!
//! Absent a qualifying signal an impression stays served-but-undetermined —
//! never counted as viewable. This keeps server-side reporting MRC-honest:
//! we may confirm "served", we may confirm "viewable" on player evidence, we
//! never invent visibility.

use serde::{Deserialize, Serialize};

/// MRC video threshold: percent of ad pixels in view.
pub const MRC_COVERAGE_PCT: f32 = 50.0;
/// MRC video threshold: continuous visible seconds.
pub const MRC_CONTINUOUS_SECS: f32 = 2.0;

/// One player heartbeat while an ad renders (see `MM_ADS_PLAN.md` §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewabilitySignal {
    /// Fraction of ad pixels in view, 0–100 (OMID geometry).
    pub coverage_pct: f32,
    /// Whether the ad was audible at this heartbeat.
    pub audible: bool,
    /// Unbroken seconds with coverage at/above threshold, tracked by the
    /// player; resets to 0 whenever coverage drops below threshold.
    pub continuous_visible_secs: f32,
}

impl ViewabilitySignal {
    /// Clamp out-of-range player input to the physical domain before judging.
    /// Clamping (not rejecting) keeps one misbehaving player from breaking the
    /// pipeline while bounding what it can claim.
    pub fn clamped(self) -> Self {
        Self {
            coverage_pct: self.coverage_pct.clamp(0.0, 100.0),
            audible: self.audible,
            continuous_visible_secs: self.continuous_visible_secs.max(0.0),
        }
    }
}

/// The MRC video viewability rule. Pure: unit-test the boundary here, not
/// against a database.
pub fn meets_mrc_viewable(signal: ViewabilitySignal) -> bool {
    let s = signal.clamped();
    s.coverage_pct >= MRC_COVERAGE_PCT && s.continuous_visible_secs >= MRC_CONTINUOUS_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(coverage_pct: f32, continuous_visible_secs: f32) -> ViewabilitySignal {
        ViewabilitySignal { coverage_pct, audible: true, continuous_visible_secs }
    }

    #[test]
    fn at_exactly_threshold_is_viewable() {
        assert!(meets_mrc_viewable(signal(50.0, 2.0)));
    }

    #[test]
    fn just_below_coverage_is_not_viewable() {
        assert!(!meets_mrc_viewable(signal(49.9, 10.0)));
    }

    #[test]
    fn just_below_duration_is_not_viewable() {
        assert!(!meets_mrc_viewable(signal(100.0, 1.99)));
    }

    #[test]
    fn zero_coverage_is_never_viewable() {
        assert!(!meets_mrc_viewable(signal(0.0, 30.0)));
    }

    #[test]
    fn inaudible_can_still_be_viewable() {
        // Audibility is reported for attention analysis; the MRC video rule
        // does not require sound.
        let s = ViewabilitySignal { coverage_pct: 80.0, audible: false, continuous_visible_secs: 5.0 };
        assert!(meets_mrc_viewable(s));
    }

    #[test]
    fn out_of_range_input_is_clamped_not_trusted() {
        // 150% coverage clamps to 100% (still viewable with duration), and a
        // negative span clamps to 0 (never viewable).
        assert!(meets_mrc_viewable(signal(150.0, 3.0)));
        assert!(!meets_mrc_viewable(signal(80.0, -5.0)));
        assert!(!meets_mrc_viewable(signal(-10.0, 5.0)));
    }
}
