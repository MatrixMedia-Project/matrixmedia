//! Static provider preset catalog (spec §4/§11): the "money-machine" hook that lets the
//! admin UI and the Task 8 ranker see, for a `kind`, which regions it may serve and what
//! egress costs there — without a DB round trip, because this data never changes at
//! runtime. `adapter_ready` is deliberately **not** stored here: it is whether this crate
//! has a create/destroy adapter wired up (`placement::adapter_built`), which is a property
//! of the code, not of the catalog, so the admin endpoint computes it at request time.
//!
//! Region strings are the existing `mm_fleet_provider_zones` enum values (`eu`, `us`,
//! `asia` — see `providers_db::REGIONS`). Several providers here have zones outside that
//! three-way split (e.g. Vultr's Tokyo, DigitalOcean's Singapore); this catalog maps them
//! coarsely: North America → `us`, everything else non-EU (APAC and otherwise) → `asia`.
//! That is a simplification, not a geography lesson — noted per zone below where it bites.

/// One zone a preset can place in: the provider's own zone id, the coarse region bucket it
/// falls into, and its position in failover order (lowest tried first).
pub type PresetZone = (&'static str, &'static str, u8);

/// One region's egress price for a preset, in integer USD-equivalent milli-currency per TB
/// (i.e. `1_000` milli == $1.00/TB). Integer, never float: money that reappears per TB of
/// stream egress has no business being represented by anything that rounds.
pub type EgressRate = (&'static str, i64);

/// A static description of one way to rent broadcast-fleet capacity from a provider.
/// `kind` must be a value `providers_db::KINDS` recognizes (or will, once its adapter
/// lands) — see `every_preset_kind_is_a_valid_db_kind` below for which kinds that holds
/// for today. Two presets may share a `kind`: OVH is modeled as two entities (`ovh-eu`,
/// `ovh-us`) because its EU and North-American clouds are different accounts with
/// different pricing, but both provision through the same `ovh` adapter kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    /// The `providers_db::KINDS` value this preset provisions through.
    pub kind: &'static str,
    /// Human label distinguishing this preset from any sibling sharing `kind` (e.g.
    /// "OVH (EU)" vs "OVH (US)"). Unique across the whole catalog.
    pub label: &'static str,
    /// The provider's default API endpoint.
    pub endpoint: &'static str,
    /// Zones this preset may place in, failover order.
    pub zones: &'static [PresetZone],
    /// The fanout-node size id this preset provisions by default.
    pub fanout_size: &'static str,
    /// Credential fields the operator must supply to use this preset.
    pub token_fields: &'static [&'static str],
    /// Free-text operator-facing caveats (coarse region mapping, EU-only, etc).
    pub notes: &'static str,
    /// Egress price per region, integer milli-currency per TB.
    pub egress_model: &'static [EgressRate],
}

const HETZNER: ProviderPreset = ProviderPreset {
    kind: "hetzner",
    label: "Hetzner",
    endpoint: "https://api.hetzner.cloud/v1",
    // Hetzner Cloud has no non-EU region for the broadcast fleet: Falkenstein, Nuremberg,
    // Helsinki are all 'eu'. No "us"/"asia" entry belongs here on purpose.
    zones: &[("fsn1", "eu", 0), ("nbg1", "eu", 1), ("hel1", "eu", 2)],
    fanout_size: "cpx21",
    token_fields: &["api_token"],
    notes: "EU only: Hetzner Cloud has no non-EU zone for this fleet.",
    egress_model: &[("eu", 1190)],
};

const OVH_EU: ProviderPreset = ProviderPreset {
    kind: "ovh",
    label: "OVH (EU)",
    endpoint: "https://eu.api.ovh.com/1.0",
    zones: &[("gra", "eu", 0), ("sbg", "eu", 1)],
    fanout_size: "b2-7",
    token_fields: &["application_key", "application_secret", "consumer_key"],
    notes: "OVH's EU cloud entity; separate billing/account from OVH (US).",
    egress_model: &[("eu", 0)],
};

const OVH_US: ProviderPreset = ProviderPreset {
    kind: "ovh",
    label: "OVH (US)",
    endpoint: "https://api.us.ovhcloud.com/1.0",
    // OVH's North American region (Beauharnois, Canada) is grouped under its US entity.
    zones: &[("bhs", "us", 0)],
    fanout_size: "b2-7",
    token_fields: &["application_key", "application_secret", "consumer_key"],
    notes: "OVH's North-American cloud entity; separate billing/account from OVH (EU).",
    egress_model: &[("us", 0)],
};

const VULTR: ProviderPreset = ProviderPreset {
    kind: "vultr",
    label: "Vultr",
    endpoint: "https://api.vultr.com/v2",
    // NA -> 'us', APAC/other -> 'asia' (coarse mapping, see module docs).
    zones: &[
        ("ewr", "us", 0),
        ("ams", "eu", 1),
        ("sgp", "asia", 2),
        ("nrt", "asia", 3),
    ],
    fanout_size: "vc2-2c-4gb",
    token_fields: &["api_key"],
    notes: "Non-EU/US zones (e.g. Tokyo) are bucketed into 'asia'; coarse mapping.",
    egress_model: &[("eu", 10000), ("us", 10000), ("asia", 10000)],
};

const AKAMAI: ProviderPreset = ProviderPreset {
    kind: "akamai",
    label: "Akamai (Linode)",
    endpoint: "https://api.linode.com/v4",
    zones: &[
        ("us-iad", "us", 0),
        ("eu-west", "eu", 1),
        ("ap-south", "asia", 2),
    ],
    fanout_size: "g6-standard-4",
    token_fields: &["api_token"],
    notes: "Non-EU/US zones (e.g. ap-south) are bucketed into 'asia'; coarse mapping.",
    egress_model: &[("eu", 5000), ("us", 5000), ("asia", 5000)],
};

const DIGITALOCEAN: ProviderPreset = ProviderPreset {
    kind: "digitalocean",
    label: "DigitalOcean",
    endpoint: "https://api.digitalocean.com/v2",
    zones: &[
        ("nyc1", "us", 0),
        ("ams3", "eu", 1),
        ("sgp1", "asia", 2),
    ],
    fanout_size: "s-4vcpu-8gb",
    token_fields: &["api_token"],
    notes: "Non-EU/US zones (e.g. sgp1) are bucketed into 'asia'; coarse mapping.",
    egress_model: &[("eu", 10000), ("us", 10000), ("asia", 10000)],
};

const CATALOG: &[ProviderPreset] = &[HETZNER, OVH_EU, OVH_US, VULTR, AKAMAI, DIGITALOCEAN];

/// The full preset catalog, in no particular ranking order (Task 8 ranks).
pub fn catalog() -> &'static [ProviderPreset] {
    CATALOG
}

/// Whether any preset for `kind` may place in `region`.
pub fn allowed(kind: &str, region: &str) -> bool {
    CATALOG
        .iter()
        .filter(|p| p.kind == kind)
        .any(|p| p.zones.iter().any(|(_, r, _)| *r == region))
}

/// The egress rate (milli-currency per TB) for `kind` in `region`, if any preset for that
/// kind prices it. When two presets share a `kind` (OVH), only the preset whose `zones`
/// cover `region` is consulted, so EU and US pricing never mix.
pub fn egress_model(kind: &str, region: &str) -> Option<i64> {
    CATALOG
        .iter()
        .filter(|p| p.kind == kind)
        .filter(|p| p.zones.iter().any(|(_, r, _)| *r == region))
        .find_map(|p| {
            p.egress_model
                .iter()
                .find(|(r, _)| *r == region)
                .map(|(_, milli)| *milli)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement;
    use crate::providers_db::KINDS;
    use std::collections::HashSet;

    #[test]
    fn catalog_is_nonempty_and_labels_unique() {
        let cat = catalog();
        assert!(!cat.is_empty());
        let mut labels = HashSet::new();
        for p in cat {
            assert!(
                labels.insert(p.label),
                "duplicate preset label: {}",
                p.label
            );
        }
    }

    #[test]
    fn hetzner_preset_has_no_non_eu_zone() {
        let hetzner = catalog().iter().find(|p| p.label == "Hetzner").unwrap();
        assert!(hetzner.zones.iter().all(|(_, region, _)| *region == "eu"));
    }

    /// Every preset names a `kind` that either already has a create/destroy adapter
    /// (`placement::adapter_built`) or is a documented not-yet-rentable kind. Vultr and
    /// DigitalOcean are the latter today: their kinds are not in `providers_db::KINDS` and
    /// have no adapter, so the admin endpoint must mark them `adapter_ready: false` rather
    /// than claim they can be rented.
    #[test]
    fn every_preset_kind_is_a_valid_db_kind() {
        const NOT_YET_RENTABLE: &[&str] = &["vultr", "digitalocean"];
        for p in catalog() {
            let known = KINDS.contains(&p.kind);
            let not_yet = NOT_YET_RENTABLE.contains(&p.kind);
            assert!(
                known || not_yet,
                "preset kind '{}' is neither a known db kind nor documented as not-yet-rentable",
                p.kind
            );
            if not_yet {
                assert!(
                    !known,
                    "'{}' is listed as not-yet-rentable but is already a known db kind \
                     — update NOT_YET_RENTABLE",
                    p.kind
                );
                assert!(
                    !placement::adapter_built(p.kind),
                    "'{}' has an adapter built but is listed as not-yet-rentable",
                    p.kind
                );
            }
        }
    }

    #[test]
    fn allowed_reflects_catalog_zones() {
        assert!(allowed("hetzner", "eu"));
        assert!(!allowed("hetzner", "us"));
        assert!(allowed("ovh", "eu"));
        assert!(allowed("ovh", "us"));
        assert!(!allowed("ovh", "asia"));
        assert!(!allowed("nonexistent-kind", "eu"));
    }

    #[test]
    fn egress_model_does_not_mix_ovh_entities() {
        assert_eq!(egress_model("ovh", "eu"), Some(0));
        assert_eq!(egress_model("ovh", "us"), Some(0));
        assert_eq!(egress_model("ovh", "asia"), None);
        assert_eq!(egress_model("hetzner", "eu"), Some(1190));
        assert_eq!(egress_model("hetzner", "us"), None);
        assert_eq!(egress_model("akamai", "asia"), Some(5000));
        assert_eq!(egress_model("vultr", "us"), Some(10000));
        assert_eq!(egress_model("digitalocean", "asia"), Some(10000));
    }
}
