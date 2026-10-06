//! The transcode opt-in (FR-314a/c; ops-page design §14, owner decisions D12/D13).
//!
//! The GPU ladder is ~88% of a small broadcast's bill and it spends the
//! broadcaster's wallet, so whether a broadcast gets one is the **broadcaster's
//! stored choice**: a default on their creator profile plus a per-broadcast
//! override. An operator who releases a transcoder sets a sticky flag on the
//! broadcast, and only the broadcaster opting that broadcast in again clears it.
//!
//! "Paying broadcasters only" is deliberately **not** part of this type. It is a
//! separate condition the planner ANDs with [`TranscodeOptIn::wants_transcoder`]
//! (FR-314a): this type says what the broadcaster wants, not what they can pay for.
//!
//! The string forms of [`TranscodeOverride`] MUST match the CHECK in
//! `V040__transcode_opt_in.sql`; `ddl_agreement` below reads the file.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The per-broadcast setting (`mm_streams.transcode_opt_in`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TranscodeOverride {
    /// Follow the broadcaster's default.
    #[default]
    Inherit,
    /// Transcode this broadcast whatever the default says. Also the only way to
    /// clear an operator release (FR-314c).
    On,
    /// Never transcode this broadcast whatever the default says.
    Off,
}

impl TranscodeOverride {
    pub const ALL: [TranscodeOverride; 3] = [Self::Inherit, Self::On, Self::Off];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

impl fmt::Display for TranscodeOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TranscodeOverride {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|o| o.as_str() == s)
            .ok_or_else(|| format!("unknown transcode override {s:?}"))
    }
}

/// Everything stored about one broadcast's transcode choice.
///
/// `Default` is "never opted in, never released" — the state of every broadcast
/// before anyone chooses anything, and the safe one: it provisions nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscodeOptIn {
    /// `mm_creator_defaults.transcode_opt_in_default` for the broadcast's host;
    /// `false` when the host has no defaults row.
    pub broadcaster_default: bool,
    /// `mm_streams.transcode_opt_in`.
    pub broadcast_override: TranscodeOverride,
    /// `mm_streams.transcode_released` — an operator released this broadcast's
    /// transcoder (FR-314c).
    pub released: bool,
}

impl TranscodeOptIn {
    /// Has the broadcaster chosen transcoding for this broadcast? The override
    /// wins; `Inherit` falls back to the default. Says nothing about a release.
    pub fn opted_in(&self) -> bool {
        match self.broadcast_override {
            TranscodeOverride::On => true,
            TranscodeOverride::Off => false,
            TranscodeOverride::Inherit => self.broadcaster_default,
        }
    }

    /// Should this broadcast have a transcoder, as far as the broadcaster's choice
    /// goes? An operator release vetoes it until the broadcaster opts in again.
    ///
    /// Necessary, not sufficient: the planner still requires a paying broadcaster
    /// and its live-programme and balance gates before it provisions one.
    pub fn wants_transcoder(&self) -> bool {
        self.opted_in() && !self.released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt_in(default: bool, ov: TranscodeOverride, released: bool) -> TranscodeOptIn {
        TranscodeOptIn {
            broadcaster_default: default,
            broadcast_override: ov,
            released,
        }
    }

    #[test]
    fn the_default_is_not_opted_in() {
        let d = TranscodeOptIn::default();
        assert!(!d.opted_in());
        assert!(
            !d.wants_transcoder(),
            "a broadcast nobody has chosen anything for must not be given a GPU"
        );
    }

    /// The full table: 2 defaults × 3 overrides × 2 release states.
    #[test]
    fn the_override_wins_and_a_release_vetoes() {
        use TranscodeOverride::*;
        let cases = [
            // default, override, released => opted_in, wants
            (false, Inherit, false, false, false),
            (true, Inherit, false, true, true),
            (false, On, false, true, true),
            (true, On, false, true, true),
            (false, Off, false, false, false),
            (true, Off, false, false, false),
            (false, Inherit, true, false, false),
            (true, Inherit, true, true, false),
            (false, On, true, true, false),
            (true, On, true, true, false),
            (false, Off, true, false, false),
            (true, Off, true, false, false),
        ];
        for (default, ov, released, opted, wants) in cases {
            let c = opt_in(default, ov, released);
            assert_eq!(c.opted_in(), opted, "opted_in for {c:?}");
            assert_eq!(c.wants_transcoder(), wants, "wants_transcoder for {c:?}");
        }
    }

    #[test]
    fn string_forms_round_trip() {
        for o in TranscodeOverride::ALL {
            assert_eq!(o.as_str().parse::<TranscodeOverride>(), Ok(o));
            assert_eq!(
                serde_json::to_value(o).unwrap(),
                serde_json::Value::String(o.as_str().into()),
                "the API's JSON form must be the stored form"
            );
        }
        assert!("ON".parse::<TranscodeOverride>().is_err());
        assert!("".parse::<TranscodeOverride>().is_err());
    }

    mod ddl_agreement {
        use super::*;

        const V040: &str = include_str!("../../../mm-db/migrations/V040__transcode_opt_in.sql");

        /// The database tells you a variant drifted from the CHECK only at UPDATE
        /// time, in production. This tells you at `cargo test`.
        #[test]
        fn override_variants_match_the_check_constraint() {
            let flat = V040.split_whitespace().collect::<Vec<_>>().join(" ");
            let list = flat
                .split_once("CHECK (transcode_opt_in IN (")
                .expect("V040 has no CHECK on transcode_opt_in")
                .1
                .split_once("))")
                .expect("unterminated CHECK")
                .0;
            let mut in_ddl: Vec<String> = list
                .split(',')
                .map(|v| v.trim().trim_matches('\'').to_string())
                .collect();
            let mut in_rust: Vec<String> = TranscodeOverride::ALL
                .iter()
                .map(|o| o.as_str().to_string())
                .collect();
            in_ddl.sort();
            in_rust.sort();
            assert_eq!(in_ddl, in_rust);

            assert!(
                flat.contains("transcode_opt_in TEXT NOT NULL DEFAULT 'inherit'"),
                "the column default must be TranscodeOverride::default()"
            );
            assert_eq!(TranscodeOverride::default(), TranscodeOverride::Inherit);
        }
    }
}
