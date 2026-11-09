//! Streamer caption consent (V057).
//!
//! Track readiness lives in `mm_recording_captions.status` (`pending` →
//! `ready` + storage key, or `failed` + error); the recording API serves a
//! language only for `ready` rows with a non-null key and never hands out a
//! URL that 404s — the same discipline as `mp4_url` gating on `mp4_status`.
//! Track-row SQL lives in the caption job (`mm-api/src/caption_tracker.rs`)
//! and the `Database::get_ready_caption_tracks` row type; this module owns
//! only the consent side.
//!
//! Streamer consent mirrors V040's transcode opt-in shape (creator default +
//! per-broadcast `inherit|on|off` override) but has no `released` flag:
//! there is no rented GPU to release for captions, so no operator veto.
//! Captions are a consent feature, not a wallet feature. Precedence
//! (override over default) is NOT decided here — it lives in
//! [`CaptionsOptIn`], once.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// The per-broadcast caption setting (`mm_streams.captions_opt_in`).
///
/// Same three string forms as [`mm_core::fleet::transcode::TranscodeOverride`]
/// by design (streamer-facing `inherit|on|off` toggles look identical), but a
/// separate type: captions have no release veto, so sharing the transcode
/// type would smuggle fleet semantics into a consent column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CaptionOverride {
    /// Follow the broadcaster's default.
    #[default]
    Inherit,
    /// Caption this broadcast whatever the default says.
    On,
    /// Never caption this broadcast whatever the default says.
    Off,
}

impl CaptionOverride {
    pub const ALL: [CaptionOverride; 3] = [Self::Inherit, Self::On, Self::Off];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

impl fmt::Display for CaptionOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CaptionOverride {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|o| o.as_str() == s)
            .ok_or_else(|| format!("unknown caption override {s:?}"))
    }
}

/// Everything stored about one broadcast's caption choice.
///
/// `Default` is "never opted in" — the state of every broadcast before
/// anyone chooses anything, and the safe one: no STT work is scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptionsOptIn {
    /// `mm_creator_defaults.captions_opt_in_default` for the broadcast's host;
    /// `false` when the host has no defaults row.
    pub broadcaster_default: bool,
    /// `mm_streams.captions_opt_in`.
    pub broadcast_override: CaptionOverride,
}

impl CaptionsOptIn {
    /// Has the broadcaster chosen captions for this broadcast? The override
    /// wins; `Inherit` falls back to the default.
    pub fn opted_in(&self) -> bool {
        match self.broadcast_override {
            CaptionOverride::On => true,
            CaptionOverride::Off => false,
            CaptionOverride::Inherit => self.broadcaster_default,
        }
    }
}

/// One broadcast's caption facts, with what a caller needs to authorise a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastCaptions {
    pub host_user_id: String,
    /// `mm_streams.status`; only an `active` broadcast's override can change.
    pub active: bool,
    pub opt_in: CaptionsOptIn,
}

/// Why a per-broadcast caption override was not written. Each variant is a
/// different answer to the caller (404 / 403 / 410), which is the reason to
/// tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptionOverrideRefused {
    NotFound,
    NotHost,
    Ended,
}

/// The broadcaster's caption default; `false` when they have never saved defaults.
pub async fn broadcaster_default(pool: &PgPool, user_id: &str) -> Result<bool, sqlx::Error> {
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT captions_opt_in_default FROM mm_creator_defaults WHERE creator_user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(on.unwrap_or(false))
}

/// Set the broadcaster's caption default. Touches no other default: a
/// broadcaster with no defaults row gets one with the column defaults for
/// everything else.
pub async fn set_broadcaster_default(
    pool: &PgPool,
    user_id: &str,
    on: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mm_creator_defaults (creator_user_id, captions_opt_in_default)
         VALUES ($1, $2)
         ON CONFLICT (creator_user_id) DO UPDATE
            SET captions_opt_in_default = EXCLUDED.captions_opt_in_default,
                updated_at = now()",
    )
    .bind(user_id)
    .bind(on)
    .execute(pool)
    .await?;
    Ok(())
}

/// Everything stored about one broadcast's caption choice, or `None` when
/// the broadcast does not exist.
pub async fn for_broadcast(
    pool: &PgPool,
    stream_id: &str,
) -> Result<Option<BroadcastCaptions>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT s.host_user_id,
                s.status,
                s.captions_opt_in,
                COALESCE(d.captions_opt_in_default, false) AS broadcaster_default
           FROM mm_streams s
           LEFT JOIN mm_creator_defaults d ON d.creator_user_id = s.host_user_id
          WHERE s.id = $1",
    )
    .bind(stream_id)
    .fetch_optional(pool)
    .await?;

    row.map(|r| {
        Ok(BroadcastCaptions {
            host_user_id: r.try_get("host_user_id")?,
            active: r.try_get::<String, _>("status")? == "active",
            opt_in: CaptionsOptIn {
                broadcaster_default: r.try_get("broadcaster_default")?,
                broadcast_override: parse_override(r.try_get("captions_opt_in")?)?,
            },
        })
    })
    .transpose()
}

/// Set one broadcast's caption override, as its host. Returns the resulting choice.
///
/// The host and `active` conditions are in the UPDATE itself, so a broadcast
/// that ends between the caller's read and this write is refused rather than
/// changed.
pub async fn set_broadcast_override(
    pool: &PgPool,
    stream_id: &str,
    host_user_id: &str,
    choice: CaptionOverride,
) -> Result<Result<CaptionsOptIn, CaptionOverrideRefused>, sqlx::Error> {
    let row = sqlx::query(
        "UPDATE mm_streams s
            SET captions_opt_in = $3
          WHERE s.id = $1 AND s.host_user_id = $2 AND s.status = 'active'
      RETURNING s.captions_opt_in,
                COALESCE((SELECT d.captions_opt_in_default
                            FROM mm_creator_defaults d
                           WHERE d.creator_user_id = s.host_user_id), false)
                    AS broadcaster_default",
    )
    .bind(stream_id)
    .bind(host_user_id)
    .bind(choice.as_str())
    .fetch_optional(pool)
    .await?;

    if let Some(r) = row {
        return Ok(Ok(CaptionsOptIn {
            broadcaster_default: r.try_get("broadcaster_default")?,
            broadcast_override: parse_override(r.try_get("captions_opt_in")?)?,
        }));
    }

    // Nothing written: say why.
    Ok(Err(match for_broadcast(pool, stream_id).await? {
        None => CaptionOverrideRefused::NotFound,
        Some(b) if b.host_user_id != host_user_id => CaptionOverrideRefused::NotHost,
        Some(_) => CaptionOverrideRefused::Ended,
    }))
}

/// The CHECK makes an unknown value unreachable; reaching it anyway is a schema
/// drift, reported as a decode error rather than read as "not opted in".
fn parse_override(s: String) -> Result<CaptionOverride, sqlx::Error> {
    s.parse().map_err(|e: String| sqlx::Error::Decode(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_not_opted_in() {
        let d = CaptionsOptIn::default();
        assert!(
            !d.opted_in(),
            "a broadcast nobody chose anything for gets no STT"
        );
    }

    #[test]
    fn the_override_wins_over_the_default() {
        use CaptionOverride::*;
        let cases = [
            (false, Inherit, false),
            (true, Inherit, true),
            (false, On, true),
            (true, On, true),
            (false, Off, false),
            (true, Off, false),
        ];
        for (default, ov, opted) in cases {
            let c = CaptionsOptIn {
                broadcaster_default: default,
                broadcast_override: ov,
            };
            assert_eq!(c.opted_in(), opted, "opted_in for {c:?}");
        }
    }

    #[test]
    fn string_forms_round_trip() {
        for o in CaptionOverride::ALL {
            assert_eq!(o.as_str().parse::<CaptionOverride>(), Ok(o));
            assert_eq!(
                serde_json::to_value(o).unwrap(),
                serde_json::Value::String(o.as_str().into()),
                "the API's JSON form must be the stored form"
            );
        }
        assert!("ON".parse::<CaptionOverride>().is_err());
        assert!("".parse::<CaptionOverride>().is_err());
    }

    mod ddl_agreement {
        use super::*;

        const V057: &str = include_str!("../migrations/V057__recording_captions.sql");

        /// The database tells you a variant drifted from the CHECK only at UPDATE
        /// time, in production. This tells you at `cargo test`.
        #[test]
        fn override_variants_match_the_check_constraint() {
            let flat = V057.split_whitespace().collect::<Vec<_>>().join(" ");
            let list = flat
                .split_once("CHECK (captions_opt_in IN (")
                .expect("V057 has no CHECK on captions_opt_in")
                .1
                .split_once("))")
                .expect("unterminated CHECK")
                .0;
            let mut in_ddl: Vec<String> = list
                .split(',')
                .map(|v| v.trim().trim_matches('\'').to_string())
                .collect();
            let mut in_rust: Vec<String> = CaptionOverride::ALL
                .iter()
                .map(|o| o.as_str().to_string())
                .collect();
            in_ddl.sort();
            in_rust.sort();
            assert_eq!(in_ddl, in_rust);

            assert!(
                flat.contains("captions_opt_in TEXT NOT NULL DEFAULT 'inherit'"),
                "the column default must be CaptionOverride::default()"
            );
            assert_eq!(CaptionOverride::default(), CaptionOverride::Inherit);
        }
    }
}
