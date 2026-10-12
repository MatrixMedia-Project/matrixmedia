//! Who pays for a fleet node, and the broadcaster's funding choice (GPU funding
//! design §3.1–§3.4). The resolver (`resolve`) lives here too, from Task 8 on.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Who pays for ONE NODE for its whole life. Written on the desired row when the
/// planner orders it, copied to the node row when the runner creates it, and read by
/// the meter when it writes that node's usage. Never changed after insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Funding {
    /// The broadcaster's prepaid wallet.
    Wallet,
    /// The platform, in exchange for ad inventory on the broadcast.
    PlatformAds,
    /// The platform, by an operator's decision (test boots, FR-308c nodes).
    Operator,
}

impl Funding {
    pub const ALL: [Funding; 3] = [Self::Wallet, Self::PlatformAds, Self::Operator];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::PlatformAds => "platform_ads",
            Self::Operator => "operator",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == raw)
    }
}

impl fmt::Display for Funding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Funding {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown funding {s:?}"))
    }
}

/// Who pays for ONE USAGE ROW. A superset of [`Funding`]: Open-tier origin egress
/// has no node payer, it is simply the platform's cost (`PlatformFree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payer {
    Wallet,
    PlatformAds,
    PlatformFree,
    Operator,
}

impl Payer {
    pub const ALL: [Payer; 4] = [
        Self::Wallet,
        Self::PlatformAds,
        Self::PlatformFree,
        Self::Operator,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::PlatformAds => "platform_ads",
            Self::PlatformFree => "platform_free",
            Self::Operator => "operator",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == raw)
    }

    /// Rows nobody is charged for are written already rated, so they never sit in
    /// the rating queue or inflate the "unsent bill" backlog (design §4.3, review B9).
    pub fn is_rated_at_write(self) -> bool {
        !matches!(self, Self::Wallet)
    }
}

impl From<Funding> for Payer {
    fn from(f: Funding) -> Self {
        match f {
            Funding::Wallet => Payer::Wallet,
            Funding::PlatformAds => Payer::PlatformAds,
            Funding::Operator => Payer::Operator,
        }
    }
}

impl fmt::Display for Payer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The broadcaster's stored choice of who should pay for their GPU (design §3.3).
/// Stored in P1; in P0 every broadcast resolves to `Wallet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuFunding {
    #[default]
    Wallet,
    Ads,
}

/// Broadcaster tier (design §3.2). Stored in P1; in P0 derived (Task 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Open,
    Verified,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_and_payer_round_trip_through_their_strings() {
        for f in Funding::ALL {
            assert_eq!(Funding::parse(f.as_str()), Some(f));
            assert_eq!(
                Payer::from(f).as_str(),
                f.as_str(),
                "a node's funding is a usage payer of the same name"
            );
        }
        for p in Payer::ALL {
            assert_eq!(Payer::parse(p.as_str()), Some(p));
        }
        assert_eq!(
            Funding::parse("platform_free"),
            None,
            "a usage payer with no node behind it"
        );
        assert_eq!(Payer::parse("gift"), None);
    }

    #[test]
    fn only_wallet_rows_wait_for_the_rater() {
        assert!(!Payer::Wallet.is_rated_at_write());
        for p in [Payer::PlatformAds, Payer::PlatformFree, Payer::Operator] {
            assert!(p.is_rated_at_write(), "{p} is nobody's bill");
        }
    }

    /// The string forms are what V043's CHECK constraints accept; `ddl_agreement`
    /// (Task 2) reads the migration file to prove it. This pins the Rust side.
    #[test]
    fn the_string_forms_are_the_schema_words() {
        assert_eq!(
            Funding::ALL.map(Funding::as_str),
            ["wallet", "platform_ads", "operator"]
        );
        assert_eq!(
            Payer::ALL.map(Payer::as_str),
            ["wallet", "platform_ads", "platform_free", "operator"]
        );
    }
}
