//! Egress-aware relay-candidate ranking (spec R3): orders fan-out candidates by projected
//! total cost — hourly rental price plus the egress the candidate's preset charges for the
//! projected stream volume — while treating each preset's region allowlist as a hard
//! constraint rather than a ranking signal.
//!
//! Pure, integer milli-currency math throughout (`1_000` milli == $1.00): money that
//! reappears per TB of stream egress has no business being represented by anything that
//! rounds. `projected_tb_per_hour_milli` is likewise milli-TB (`1_000` milli == 1.0 TB), so
//! `egress_rate_milli_per_tb * tb_milli` is a milli·milli product that is scaled back down
//! by `/ 1_000` to land in milli-currency again.
//!
//! Capacity and NIC speed are deliberately absent from this module: they come from the
//! Task 4 path, not from `presets::fanout_size`. This module only reorders/filters the
//! candidates [`crate::placement`] has already deemed otherwise eligible for `Role::Fanout`;
//! test-boot and transcode placement never call it.

use crate::presets;

/// One candidate a cost-aware ranking could consider for a fan-out relay node.
///
/// `hourly_price_milli` is milli-currency per hour (e.g. the provider's list price for
/// `size`, from `mm_fleet_provider_status.prices`/`ProviderFacts::prices`, scaled to milli
/// and rounded by the caller). `None` means the price is simply not known yet — never
/// assumed to be zero, and never a reason to drop the candidate; see [`relay_rank`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCandidate {
    pub provider_id: String,
    pub kind: String,
    pub region: String,
    pub zone: String,
    pub size: String,
    pub hourly_price_milli: Option<i64>,
}

/// Total projected cost (milli-currency) for one candidate over the upcoming hour at
/// `projected_tb_per_hour_milli` egress, or `None` if the candidate's `(kind, region)` is not
/// in any preset's allowlist — a hard constraint, so such a candidate never ranks at all.
fn projected_cost_milli(c: &RelayCandidate, projected_tb_per_hour_milli: i64) -> Option<i64> {
    if !presets::allowed(&c.kind, &c.region) {
        return None;
    }
    // `allowed` and `egress_model` are kept in sync by the preset catalog (see
    // presets::tests::egress_model_does_not_mix_ovh_entities); `unwrap_or(0)` only guards
    // against that invariant, it is not a believed-reachable branch.
    let egress_rate_milli_per_tb = presets::egress_model(&c.kind, &c.region).unwrap_or(0);
    let egress_cost_milli = egress_rate_milli_per_tb
        .saturating_mul(projected_tb_per_hour_milli)
        / 1_000;
    let hourly_milli = c.hourly_price_milli.unwrap_or(0);
    Some(hourly_milli.saturating_add(egress_cost_milli))
}

/// Ranks `candidates` by projected total cost (`hourly_price_milli` + egress for
/// `projected_tb_per_hour_milli`), cheapest first.
///
/// A candidate whose `(kind, region)` fails [`presets::allowed`] is **absent** from the
/// output, not sorted last — the allowlist is a hard constraint, not a ranking signal.
/// A candidate with `hourly_price_milli == None` (price not yet known) ranks after every
/// candidate with a known price, rather than being dropped or assumed free.
pub fn relay_rank(
    candidates: &[RelayCandidate],
    projected_tb_per_hour_milli: i64,
) -> Vec<RelayCandidate> {
    let mut ranked: Vec<(RelayCandidate, i64)> = candidates
        .iter()
        .filter_map(|c| {
            projected_cost_milli(c, projected_tb_per_hour_milli).map(|cost| (c.clone(), cost))
        })
        .collect();

    ranked.sort_by_key(|(c, cost)| (c.hourly_price_milli.is_none(), *cost));

    ranked.into_iter().map(|(c, _)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(kind: &str, region: &str, hourly_price_milli: Option<i64>) -> RelayCandidate {
        RelayCandidate {
            provider_id: format!("{kind}-{region}"),
            kind: kind.to_string(),
            region: region.to_string(),
            zone: "zone".to_string(),
            size: "size".to_string(),
            hourly_price_milli,
        }
    }

    #[test]
    fn hetzner_outside_eu_is_unplaceable() {
        // Hetzner has no non-EU zone in the catalog at all, so `us` is both disallowed and
        // unpriced; either way it must not appear in the ranked output.
        let candidates = vec![
            candidate("hetzner", "eu", Some(10_000)),
            candidate("hetzner", "us", Some(1)),
        ];
        let ranked = relay_rank(&candidates, 0);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].region, "eu");
    }

    #[test]
    fn ovh_eu_beats_hetzner_eu_at_high_egress() {
        // OVH EU egress is free (0 milli/TB); Hetzner EU egress is 1190 milli/TB. At a large
        // projected volume the egress term dominates even though OVH's hourly price is
        // modeled slightly higher than Hetzner's.
        let hetzner = candidate("hetzner", "eu", Some(5_000));
        let ovh = candidate("ovh", "eu", Some(6_000));
        let candidates = vec![hetzner.clone(), ovh.clone()];
        // 100 TB/hour projected (milli-TB): egress cost = rate_milli_per_tb * 100_000 / 1000
        // = rate_milli_per_tb * 100. Hetzner: 1190*100=119_000 + 5_000 = 124_000.
        // OVH: 0*100=0 + 6_000 = 6_000.
        let ranked = relay_rank(&candidates, 100_000);
        assert_eq!(ranked, vec![ovh, hetzner]);
    }

    #[test]
    fn hetzner_eu_beats_ovh_at_negligible_egress_if_cheaper_hourly() {
        // At negligible projected egress the hourly price dominates: Hetzner is cheaper per
        // hour than OVH here, so it wins despite OVH's lower egress rate.
        let hetzner = candidate("hetzner", "eu", Some(5_000));
        let ovh = candidate("ovh", "eu", Some(6_000));
        let candidates = vec![ovh.clone(), hetzner.clone()];
        let ranked = relay_rank(&candidates, 0);
        assert_eq!(ranked, vec![hetzner, ovh]);
    }

    #[test]
    fn included_tb_treated_as_zero_for_ephemeral() {
        // Vultr/Linode(Akamai)/DigitalOcean price at $10/$5/$10 per TB with no free
        // allowance modeled: the ranker must charge the full listed rate from the first
        // byte, never discounting for an "included" tier the preset catalog doesn't carry.
        let vultr = candidate("vultr", "us", Some(0));
        let akamai = candidate("akamai", "us", Some(0));
        let digitalocean = candidate("digitalocean", "us", Some(0));
        let candidates = vec![vultr.clone(), akamai.clone(), digitalocean.clone()];
        // 1 TB/hour projected => egress cost in milli-currency equals the listed rate.
        let ranked = relay_rank(&candidates, 1_000);
        // Akamai ($5/TB) cheapest, then Vultr and DigitalOcean tied at $10/TB (stable order
        // preserves input order on ties).
        assert_eq!(ranked, vec![akamai, vultr, digitalocean]);
    }

    #[test]
    fn disallowed_region_is_absent_not_last() {
        let allowed_candidate = candidate("hetzner", "eu", Some(1));
        let disallowed = candidate("ovh", "asia", Some(1));
        let candidates = vec![disallowed, allowed_candidate.clone()];
        let ranked = relay_rank(&candidates, 1_000_000);
        assert_eq!(ranked, vec![allowed_candidate]);
    }

    #[test]
    fn unknown_price_ranks_after_known_price_regardless_of_cost() {
        let unknown_cheap_egress = candidate("ovh", "eu", None);
        let known_expensive = candidate("hetzner", "eu", Some(999_999));
        let candidates = vec![unknown_cheap_egress.clone(), known_expensive.clone()];
        let ranked = relay_rank(&candidates, 0);
        assert_eq!(ranked, vec![known_expensive, unknown_cheap_egress]);
    }
}
