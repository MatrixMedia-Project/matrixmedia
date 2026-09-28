//! Dashboard-managed settings at runtime (spec §5–6): boot (import, re-encrypt,
//! overlay, safe mode), writes, live reload, the revision poll and "Apply & restart".
//! No HTTP here — `admin_settings` is the API on top.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::settings::crypto::KeyRing;
use mm_core::settings::overlay::{self, Problem, Stored, StoredSetting};
use mm_db::settings_db::{self, NewValue, SettingRow, StoredPayload};

/// Knobs that differ between production and tests.
#[derive(Debug, Clone)]
pub struct BootOptions {
    /// `MM_SETTINGS_SAFE_MODE=1`: ignore the database overlay entirely (break-glass).
    pub break_glass: bool,
    /// Is this env var set? Injected so tests never touch the process environment.
    pub env_probe: fn(&str) -> bool,
    /// Revision poll period (spec §5.7: 5 s).
    pub poll_interval: Duration,
    /// Delay between "Apply & restart" and exit on the instance that handled it (2 s).
    pub restart_delay: Duration,
    /// Other instances restart after a random delay in `0..=restart_jitter_max` (10 s).
    pub restart_jitter_max: Duration,
}

/// `MM_SETTINGS_SAFE_MODE` fails CLOSED: this is a break-glass switch that disables the
/// entire dashboard-settings system, so an unrecognized or garbled value must never be
/// silently read as "off". Only unset, empty/whitespace, or a recognized "off" spelling
/// (`0`/`false`/`no`/`off`, case-insensitive, trimmed) yields `false`; every other value —
/// including a typo like "of" or "flase" — yields `true`.
pub fn safe_mode_flag(v: Option<&str>) -> bool {
    match v.map(str::trim) {
        None | Some("") => false,
        Some(s) => !matches!(s.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
    }
}

impl BootOptions {
    pub fn production() -> Self {
        Self {
            break_glass: safe_mode_flag(std::env::var("MM_SETTINGS_SAFE_MODE").ok().as_deref()),
            env_probe: |v| std::env::var_os(v).is_some(),
            poll_interval: Duration::from_secs(5),
            restart_delay: Duration::from_secs(2),
            restart_jitter_max: Duration::from_secs(10),
        }
    }

    /// Fast timings and no environment access.
    pub fn for_tests() -> Self {
        Self {
            break_glass: false,
            env_probe: |_| false,
            poll_interval: Duration::from_millis(50),
            restart_delay: Duration::from_millis(20),
            restart_jitter_max: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsStatus {
    pub safe_mode: bool,
    pub safe_mode_reason: Option<String>,
    /// Newest revision this process considered at boot (spec §5.6).
    pub loaded_rev: i64,
    /// Keys whose running value came from the database.
    pub from_db: BTreeSet<String>,
    pub secret_problems: Vec<Problem>,
    pub shadowed_env: Vec<&'static str>,
}

pub struct SettingsService {
    pool: PgPool,
    /// File + env, captured at boot: what safe mode runs, and the base for "next config".
    base: Config,
    keys: Option<KeyRing>,
    handle: ConfigHandle,
    status: ArcSwap<SettingsStatus>,
    opts: BootOptions,
    /// Cancelling this runs the normal graceful shutdown; the restart policy restarts us.
    restart: CancellationToken,
    last_seen_rev: AtomicI64,
    restart_scheduled: AtomicBool,
    /// Serialises live reloads so an older database read can never overwrite a newer one.
    reload_lock: tokio::sync::Mutex<()>,
}

fn to_payload(v: Stored) -> StoredPayload {
    match v {
        Stored::Json(j) => StoredPayload::Json(j),
        Stored::Encrypted(b) => StoredPayload::Encrypted(b),
    }
}

fn to_stored(r: &SettingRow) -> StoredSetting {
    StoredSetting {
        key: r.key.clone(),
        value: match &r.payload {
            StoredPayload::Json(v) => Stored::Json(v.clone()),
            StoredPayload::Encrypted(b) => Stored::Encrypted(b.clone()),
        },
        rev: r.rev,
    }
}

async fn reencrypt_previous(pool: &PgPool, keys: &KeyRing) -> Result<(), sqlx::Error> {
    let mut updates = vec![];
    for row in settings_db::load_all(pool).await? {
        let StoredPayload::Encrypted(blob) = &row.payload else { continue };
        if !keys.is_on_previous(blob) {
            continue;
        }
        match keys.decrypt(&row.key, blob) {
            Ok(plain) => updates.push((row.key.clone(), row.rev, keys.encrypt(&row.key, &plain))),
            Err(e) => warn!(key = %row.key, error = %e, "settings: cannot re-encrypt a secret"),
        }
    }
    if !updates.is_empty() {
        // Conditional on the revision read above: a secret saved meanwhile is left alone
        // (it was written under whatever key its writer had) and is picked up next start.
        let skipped = settings_db::reencrypt(pool, &updates).await?;
        info!(count = updates.len() - skipped.len(), "settings: re-encrypted secrets under the new key");
        if !skipped.is_empty() {
            warn!(keys = ?skipped, "settings: secrets changed during re-encryption; they are re-encrypted on the next start");
        }
    }
    Ok(())
}

impl SettingsService {
    /// Boot sequence: import values that have no row yet, re-encrypt secrets left on the
    /// previous key, then overlay the database onto `base` (file + env) — or run `base`
    /// unchanged in break-glass or automatic safe mode.
    pub async fn boot(
        pool: PgPool,
        base: Config,
        keys: Option<KeyRing>,
        opts: BootOptions,
        restart: CancellationToken,
    ) -> Result<Arc<Self>, sqlx::Error> {
        // Read the database as it stands BEFORE this boot's import, so the R20 guard below
        // judges "was this destination chosen in the dashboard?" against what an operator
        // (or an earlier boot) actually put there — never against rows this same import is
        // about to create.
        let existing: Vec<StoredSetting> = settings_db::load_all(&pool).await?.iter().map(to_stored).collect();

        let plan = overlay::import_plan(&base, keys.as_ref());
        for p in &plan.skipped {
            warn!(key = %p.key, reason = %p.reason, "settings: not imported (invalid value); it stays file/env-sourced");
        }
        // R15/R20: never import a secret whose paired URL_CREDENTIALS destination was
        // already chosen in the database — otherwise a secret that only appears in
        // file/env on a later boot would be imported that day and count as `from_db`,
        // letting apply_overlay's own pairing check wave the DB-chosen destination through
        // paired with a secret that never came from the dashboard.
        let (kept, dropped) = overlay::guard_import(plan.values, &existing, &base);
        for key in &dropped {
            warn!(key = %key, "settings: not imported — its paired destination was already chosen in the dashboard");
        }
        let values: Vec<NewValue> =
            kept.into_iter().map(|(key, v)| NewValue { key, payload: to_payload(v) }).collect();
        let outcome = settings_db::import(&pool, &values).await?;
        if outcome.first {
            info!(inserted = outcome.inserted, "settings: first-boot import of file/env values");
        } else if outcome.inserted > 0 {
            info!(inserted = outcome.inserted, "settings: imported values new to the database");
        }
        if let Some(k) = &keys {
            reencrypt_previous(&pool, k).await?;
        }

        let stored: Vec<StoredSetting> = settings_db::load_all(&pool).await?.iter().map(to_stored).collect();
        let (config, status) = if opts.break_glass {
            warn!("settings: MM_SETTINGS_SAFE_MODE is set — dashboard settings ignored; running from file + env");
            let status = SettingsStatus {
                safe_mode: true,
                safe_mode_reason: Some("MM_SETTINGS_SAFE_MODE is set".into()),
                loaded_rev: stored.iter().map(|r| r.rev).max().unwrap_or(0),
                ..Default::default()
            };
            (base.clone(), status)
        } else {
            let ov = overlay::apply_overlay(&base, &stored, keys.as_ref());
            for p in &ov.secret_problems {
                warn!(key = %p.key, reason = %p.reason, "settings: secret kept from file/env");
            }
            for k in &ov.ignored {
                warn!(key = %k, "settings: ignoring a stored value this version does not manage");
            }
            let shadowed = overlay::shadowed_env(&ov.from_db, opts.env_probe);
            if !shadowed.is_empty() {
                warn!(vars = ?shadowed, "settings: these env vars are ignored — the dashboard value wins");
            }
            let reason = ov.safe_mode_reason();
            if let Some(r) = &reason {
                error!(reason = %r, "settings: AUTOMATIC SAFE MODE — stored settings rejected; running from file + env");
            }
            let status = SettingsStatus {
                safe_mode: reason.is_some(),
                safe_mode_reason: reason,
                loaded_rev: ov.loaded_rev,
                from_db: ov.from_db,
                secret_problems: ov.secret_problems,
                shadowed_env: shadowed,
            };
            (ov.config, status)
        };
        let loaded_rev = status.loaded_rev;
        Ok(Arc::new(Self {
            pool,
            base,
            keys,
            handle: ConfigHandle::new(config),
            status: ArcSwap::from_pointee(status),
            opts,
            restart,
            last_seen_rev: AtomicI64::new(loaded_rev),
            restart_scheduled: AtomicBool::new(false),
            reload_lock: tokio::sync::Mutex::new(()),
        }))
    }

    /// The live config handle handlers read from.
    pub fn handle(&self) -> &ConfigHandle {
        &self.handle
    }

    pub fn status(&self) -> Arc<SettingsStatus> {
        self.status.load_full()
    }

    pub fn poll_interval(&self) -> Duration {
        self.opts.poll_interval
    }

    pub fn encryption_configured(&self) -> bool {
        self.keys.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_mode_flag_fails_closed_on_anything_but_a_recognized_off_spelling() {
        let cases: &[(Option<&str>, bool)] = &[
            (None, false),
            (Some(""), false),
            (Some("   "), false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("FALSE"), false),
            (Some("no"), false),
            (Some("NO"), false),
            (Some("off"), false),
            (Some("OFF"), false),
            (Some(" off "), false),
            (Some("1"), true),
            (Some("true"), true),
            (Some("TRUE"), true),
            (Some("yes"), true),
            (Some("on"), true),
            (Some("garbage"), true),
        ];
        for (input, expected) in cases {
            assert_eq!(safe_mode_flag(*input), *expected, "input {input:?}");
        }
    }
}
