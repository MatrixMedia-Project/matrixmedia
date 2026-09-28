//! Dashboard-managed settings at runtime (spec §5–6): boot (import, re-encrypt,
//! overlay, safe mode), writes, live reload, the revision poll and "Apply & restart".
//! No HTTP here — `admin_settings` is the API on top.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::settings::crypto::KeyRing;
use mm_core::settings::overlay::{self, Problem, Source, Stored, StoredSetting};
use mm_core::settings::{ApplyClass, SettingDef, URL_CREDENTIALS, find, registry};
use mm_db::settings_db::{self, AuditRow, NewValue, SettingRow, StoredPayload};

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

#[derive(Debug)]
pub enum PatchError {
    /// No such setting.
    Unknown(String),
    /// Bootstrap or host-coupled; the message says where to change it instead.
    ReadOnly(String),
    /// A secret was sent but `MM_SETTINGS_ENCRYPTION_KEY` is not configured.
    NoKey(String),
    /// A URL that secrets are sent to changed without those secrets (URL_CREDENTIALS).
    NeedsCredentials(String),
    Invalid(Vec<Problem>),
    Conflict { current_rev: i64 },
    Db(sqlx::Error),
}

impl From<sqlx::Error> for PatchError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// This instance exits in `in_secs`; the restart policy brings it back.
    Restarting { in_secs: u64 },
    /// This instance already runs everything saved (other instances may still restart).
    NothingToRestart,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValueView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_set: Option<bool>,
    pub source: Source,
    pub env_shadowed: bool,
    pub pending: bool,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by: Option<String>,
}

/// The `GET /settings` body.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub schema: &'static [SettingDef],
    pub values: BTreeMap<&'static str, ValueView>,
    pub safe_mode: bool,
    pub safe_mode_reason: Option<String>,
    pub loaded_rev: i64,
    /// Newest stored revision — the client sends it back as `expected_rev`.
    pub current_rev: i64,
    pub pending_restart: Vec<&'static str>,
    pub encryption_key_configured: bool,
    /// Secrets still encrypted under the previous key (0 after a finished rotation).
    pub rows_on_previous_key: usize,
    pub secret_problems: Vec<Problem>,
    pub demo: bool,
}

fn read_only_message(def: &SettingDef) -> String {
    match def.class {
        ApplyClass::Bootstrap { reason } => format!(
            "{} is read-only here: {reason}. Change it in the server's environment and restart.",
            def.key
        ),
        ApplyClass::HostCoupled { service } => format!(
            "{} must match {service}'s own configuration; change both together outside the dashboard.",
            def.key
        ),
        ApplyClass::Live | ApplyClass::Restart => format!("{} is read-only", def.key),
    }
}

impl SettingsService {
    async fn stored(&self) -> Result<Vec<StoredSetting>, sqlx::Error> {
        Ok(settings_db::load_all(&self.pool).await?.iter().map(to_stored).collect())
    }

    /// The config the next restart would run: base + every stored value (just base while
    /// a stored value is rejected — the restart would come up in safe mode).
    pub async fn next_config(&self) -> Result<Config, sqlx::Error> {
        Ok(overlay::apply_overlay(&self.base, &self.stored().await?, self.keys.as_ref()).config)
    }

    fn encode(&self, def: &SettingDef, v: &Value) -> Result<StoredPayload, PatchError> {
        if !def.secret {
            return Ok(StoredPayload::Json(v.clone()));
        }
        let keys = self.keys.as_ref().ok_or_else(|| PatchError::NoKey(def.key.into()))?;
        Ok(StoredPayload::Encrypted(keys.encrypt(def.key, v.to_string().as_bytes())))
    }

    /// Does a stored secret row hold a value? Fails CLOSED: a row this instance cannot
    /// decrypt (no key, wrong key, corrupt) counts as set — it may decrypt after the next
    /// restart, and must not then be sent to a destination moved in the meantime.
    fn stored_secret_is_set(&self, def: &SettingDef, row: &SettingRow) -> bool {
        match (&row.payload, &self.keys) {
            (StoredPayload::Encrypted(blob), Some(k)) => match k.decrypt(def.key, blob) {
                Ok(plain) => serde_json::from_slice::<Value>(&plain).map_or(true, |v| !overlay::is_empty(&v)),
                Err(_) => true,
            },
            (StoredPayload::Encrypted(_), None) => true,
            (StoredPayload::Json(v), _) => !overlay::is_empty(v),
        }
    }

    /// Validate every change and the combined next config, persist optimistically, then
    /// apply Live changes on this instance now (others follow within one poll).
    pub async fn patch(
        &self,
        changes: &BTreeMap<String, Value>,
        expected_rev: i64,
        actor: &str,
    ) -> Result<i64, PatchError> {
        let mut accepted: Vec<(&'static SettingDef, &Value)> = vec![];
        let mut problems = vec![];
        for (key, value) in changes {
            let def = find(key).ok_or_else(|| PatchError::Unknown(key.clone()))?;
            if !def.editable() {
                return Err(PatchError::ReadOnly(read_only_message(def)));
            }
            match def.validate(value) {
                Ok(()) => accepted.push((def, value)),
                Err(reason) => problems.push(Problem { key: key.clone(), reason }),
            }
        }
        if !problems.is_empty() {
            return Err(PatchError::Invalid(problems));
        }
        let values = accepted
            .iter()
            .map(|(def, value)| Ok(NewValue { key: def.key.into(), payload: self.encode(def, value)? }))
            .collect::<Result<Vec<_>, PatchError>>()?;

        let rows = settings_db::load_all(&self.pool).await?;
        let stored: Vec<StoredSetting> = rows.iter().map(to_stored).collect();
        let mut next = overlay::apply_overlay(&self.base, &stored, self.keys.as_ref()).config;

        // A URL that secrets are sent to changes only together with those secrets, so a
        // URL change can never redirect a stored secret to another host (URL_CREDENTIALS).
        // A secret counts as set when the next config carries it OR the database holds it
        // (even one this instance cannot decrypt right now).
        for pair in URL_CREDENTIALS {
            if !changes.contains_key(pair.url) {
                continue;
            }
            let missing: Vec<&str> = pair
                .secrets
                .iter()
                .copied()
                .filter(|s| !changes.contains_key(*s))
                .filter(|s| {
                    find(s).is_some_and(|d| {
                        !overlay::is_empty(&(d.get)(&next))
                            || rows.iter().find(|r| r.key == *s).is_some_and(|r| self.stored_secret_is_set(d, r))
                    })
                })
                .collect();
            if !missing.is_empty() {
                return Err(PatchError::NeedsCredentials(format!(
                    "changing {} sends {} to the new host; re-enter {} in the same save",
                    pair.url,
                    missing.join(" and "),
                    if missing.len() == 1 { "it" } else { "them" }
                )));
            }
        }

        for (def, value) in &accepted {
            (def.set)(&mut next, (*value).clone())
                .map_err(|reason| PatchError::Invalid(vec![Problem { key: def.key.into(), reason }]))?;
        }
        overlay::validate_config(&next).map_err(|errors| {
            PatchError::Invalid(errors.into_iter().map(|reason| Problem { key: "*".into(), reason }).collect())
        })?;

        let rev = settings_db::write(&self.pool, &values, expected_rev, actor).await.map_err(|e| match e {
            settings_db::SettingsDbError::Conflict { current, .. } => PatchError::Conflict { current_rev: current },
            settings_db::SettingsDbError::Db(e) => PatchError::Db(e),
        })?;
        info!(
            actor,
            rev,
            keys = ?values.iter().map(|v| v.key.as_str()).collect::<Vec<_>>(),
            "settings: saved"
        );
        // The write is committed: report success either way. A failed reload leaves
        // `last_seen_rev` behind, so the next revision poll applies the change here too.
        if let Err(e) = self.reload_live_now().await {
            warn!(error = %e, "settings: saved, but reloading live values failed; the next poll retries");
        }
        Ok(rev)
    }

    /// Re-read Live values into the running config. A no-op in safe mode (the database
    /// is ignored until restart). Serialised so an older read can't overwrite a newer one.
    async fn reload_live_now(&self) -> Result<(), sqlx::Error> {
        let _guard = self.reload_lock.lock().await;
        let stored = self.stored().await?;
        self.last_seen_rev.store(stored.iter().map(|r| r.rev).max().unwrap_or(0), Ordering::SeqCst);
        if self.status().safe_mode {
            return Ok(());
        }
        match overlay::reload_live(&self.handle.load(), &stored, self.keys.as_ref()) {
            Ok(cfg) => self.handle.store(cfg),
            Err(problems) => {
                for p in &problems {
                    warn!(key = %p.key, reason = %p.reason, "settings: live reload rejected; keeping the running values");
                }
            }
        }
        Ok(())
    }

    /// Restart-class settings saved after this instance loaded its config.
    pub async fn pending(&self) -> Result<Vec<&'static str>, sqlx::Error> {
        Ok(overlay::pending_restart(&self.stored().await?, self.status().loaded_rev))
    }

    /// "Apply & restart" (spec §6.3): dry-run the full next config; if it is valid, record
    /// the request — every instance whose loaded revision is older restarts — and schedule
    /// this instance's own restart when it is one of them.
    pub async fn apply_restart(&self, actor: &str) -> Result<ApplyOutcome, PatchError> {
        let ov = overlay::apply_overlay(&self.base, &self.stored().await?, self.keys.as_ref());
        if !ov.problems.is_empty() {
            return Err(PatchError::Invalid(ov.problems));
        }
        let requested = settings_db::request_restart(&self.pool, actor).await?;
        let loaded = self.status().loaded_rev;
        info!(actor, requested, loaded, "settings: \"Apply & restart\" requested");
        if requested > loaded {
            self.schedule_restart(self.opts.restart_delay);
            Ok(ApplyOutcome::Restarting { in_secs: self.opts.restart_delay.as_secs_f64().ceil() as u64 })
        } else {
            Ok(ApplyOutcome::NothingToRestart)
        }
    }

    fn schedule_restart(&self, after: Duration) {
        if self.restart_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let token = self.restart.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            info!("settings: restarting to apply saved settings");
            token.cancel();
        });
    }

    /// One revision poll (spec §5.7): reload Live values when the newest revision moved;
    /// restart after a random delay when "Apply & restart" asked for a newer config.
    pub async fn poll_once(&self) -> Result<(), sqlx::Error> {
        if settings_db::max_rev(&self.pool).await? > self.last_seen_rev.load(Ordering::SeqCst) {
            self.reload_live_now().await?;
        }
        let requested = settings_db::meta(&self.pool).await?.restart_requested_rev;
        let loaded = self.status().loaded_rev;
        if requested > loaded && !self.restart_scheduled.load(Ordering::SeqCst) {
            let max_ms = u64::try_from(self.opts.restart_jitter_max.as_millis()).unwrap_or(u64::MAX);
            let delay = Duration::from_millis(if max_ms == 0 { 0 } else { rand::rng().random_range(0..=max_ms) });
            info!(
                requested,
                loaded,
                delay_ms = delay.as_millis() as u64,
                "settings: restart requested from the dashboard"
            );
            self.schedule_restart(delay);
        }
        Ok(())
    }

    /// Settings history, newest first. Secret rows never carry values.
    pub async fn audit(&self, key: Option<&str>, limit: i64) -> Result<Vec<AuditRow>, sqlx::Error> {
        settings_db::audit(&self.pool, key, limit).await
    }

    /// Shown as "set" or "not set" only. A stored row decides when it decrypts (it is what
    /// the next restart runs); otherwise — no row, or one this instance cannot decrypt, whose
    /// failure is reported in `secret_problems` — the running value decides.
    fn secret_is_set(&self, def: &SettingDef, row: Option<&SettingRow>, running: &Config) -> bool {
        if let (Some(SettingRow { payload: StoredPayload::Encrypted(blob), .. }), Some(k)) = (row, &self.keys)
            && let Ok(plain) = k.decrypt(def.key, blob)
        {
            return !overlay::is_empty(&serde_json::from_slice(&plain).unwrap_or(Value::Null));
        }
        !overlay::is_empty(&(def.get)(running))
    }

    /// Everything the dashboard renders. Secrets never carry `value`; `demo` hides every
    /// value (and anything that could echo one).
    pub async fn view(&self, demo: bool) -> Result<SettingsView, sqlx::Error> {
        let rows = settings_db::load_all(&self.pool).await?;
        let stored: Vec<StoredSetting> = rows.iter().map(to_stored).collect();
        let by_key: HashMap<&str, &SettingRow> = rows.iter().map(|r| (r.key.as_str(), r)).collect();
        let status = self.status();
        let running = self.handle.load();
        let defaults = Config::default();
        let pending = overlay::pending_restart(&stored, status.loaded_rev);
        let probe = self.opts.env_probe;

        let mut values = BTreeMap::new();
        for def in registry() {
            let row = by_key.get(def.key).copied();
            let env_set = def.env.is_some_and(|v| probe(v) || probe(&format!("{v}_FROM_FILE")));
            let source = if status.from_db.contains(def.key) {
                Source::Database
            } else if env_set {
                Source::Env
            } else if (def.get)(&self.base) != (def.get)(&defaults) {
                Source::File
            } else {
                Source::Default
            };
            let (value, is_set) = if demo {
                (Some(Value::String("hidden".into())), None)
            } else if def.secret {
                (None, Some(self.secret_is_set(def, row, &running)))
            } else {
                let v = match row {
                    Some(SettingRow { payload: StoredPayload::Json(v), .. }) => v.clone(),
                    _ => (def.get)(&running),
                };
                (Some(v), None)
            };
            values.insert(
                def.key,
                ValueView {
                    value,
                    is_set,
                    source,
                    env_shadowed: def.env.is_some_and(|v| status.shadowed_env.contains(&v)),
                    pending: pending.contains(&def.key),
                    updated_at: row.filter(|_| !demo).map(|r| r.updated_at),
                    updated_by: row.filter(|_| !demo).map(|r| r.updated_by.clone()),
                },
            );
        }
        let rows_on_previous_key = self.keys.as_ref().map_or(0, |k| {
            rows.iter()
                .filter(|r| matches!(&r.payload, StoredPayload::Encrypted(b) if k.is_on_previous(b)))
                .count()
        });
        Ok(SettingsView {
            schema: registry(),
            values,
            safe_mode: status.safe_mode,
            // The reason can quote a rejected (non-secret) value, e.g. an origin.
            safe_mode_reason: if demo {
                status.safe_mode_reason.as_ref().map(|_| "hidden".to_string())
            } else {
                status.safe_mode_reason.clone()
            },
            loaded_rev: status.loaded_rev,
            current_rev: rows.iter().map(|r| r.rev).max().unwrap_or(0),
            pending_restart: pending,
            encryption_key_configured: self.keys.is_some(),
            rows_on_previous_key,
            secret_problems: if demo { vec![] } else { status.secret_problems.clone() },
            demo,
        })
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
