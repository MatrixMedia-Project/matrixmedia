//! Dashboard-managed settings at runtime (spec §5–6): boot (import, re-encrypt,
//! overlay, safe mode), writes, live reload, the revision poll and "Apply & restart".
//! No HTTP here — `admin_settings` is the API on top.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use mm_core::config::{BuildPolicy, Config};
use mm_core::config_handle::ConfigHandle;
use mm_core::settings::crypto::{self, CryptoError, KeyRing};
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
    /// Release build and `MM_ALLOW_MOCK`, read once at startup: rules that depend on the
    /// build apply to every config this service checks (boot, save, apply, live reload).
    pub policy: BuildPolicy,
    /// Why the encryption key could not be loaded, when it is set but unusable (the key
    /// ring passed to `boot` is then `None`). Names the variable, never its value.
    pub key_error: Option<CryptoError>,
}

/// `MM_SETTINGS_SAFE_MODE` fails CLOSED: this is a break-glass switch that disables the
/// entire dashboard-settings system, so an unrecognized or garbled value must never be
/// silently read as "off". Only unset, empty/whitespace, or a recognized "off" spelling
/// (`0`/`false`/`no`/`off`, case-insensitive, trimmed) yields `false`; every other value —
/// including a typo like "of" or "flase", and a value that is not valid UTF-8 — yields
/// `true`.
pub fn safe_mode_flag(v: Option<&std::ffi::OsStr>) -> bool {
    let Some(v) = v else { return false };
    let Some(s) = v.to_str() else { return true };
    match s.trim() {
        "" => false,
        s => !matches!(s.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
    }
}

impl BootOptions {
    pub fn production() -> Self {
        Self {
            break_glass: safe_mode_flag(std::env::var_os("MM_SETTINGS_SAFE_MODE").as_deref()),
            env_probe: |v| std::env::var_os(v).is_some(),
            poll_interval: Duration::from_secs(5),
            restart_delay: Duration::from_secs(2),
            restart_jitter_max: Duration::from_secs(10),
            policy: BuildPolicy {
                release_build: !cfg!(debug_assertions),
                allow_mock: std::env::var("MM_ALLOW_MOCK").is_ok_and(|v| v == "true"),
            },
            key_error: None,
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
            policy: BuildPolicy { release_build: false, allow_mock: false },
            key_error: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsStatus {
    pub safe_mode: bool,
    /// Safe mode because MM_SETTINGS_SAFE_MODE is set (not because a stored value was
    /// rejected): a restart does not leave it, so "Apply & restart" is refused.
    pub break_glass: bool,
    pub safe_mode_reason: Option<String>,
    /// Newest revision this process considered at boot (spec §5.6).
    pub loaded_rev: i64,
    /// Keys whose running value came from the database.
    pub from_db: BTreeSet<String>,
    pub secret_problems: Vec<Problem>,
    pub shadowed_env: Vec<&'static str>,
    /// The last live reload was rejected (key names and reasons, never values); cleared
    /// by the next successful one. Until then saved Live values are not running here.
    pub live_reload_error: Option<String>,
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
    /// When the scheduled restart fires (set once, together with `restart_scheduled`).
    restart_at: std::sync::Mutex<Option<Instant>>,
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

async fn load_stored(pool: &PgPool) -> Result<Vec<StoredSetting>, sqlx::Error> {
    Ok(settings_db::load_all(pool).await?.iter().map(to_stored).collect())
}

/// How often boot re-reads and re-judges when another writer commits between its read
/// and its reset of an unpaired destination.
const RESET_ATTEMPTS: usize = 3;

/// Boot overlay, after resetting every stored destination the overlay ignored because a
/// paired secret set in file/env was never stored (`Overlay::unpaired_destinations`) to
/// its file/env value. Left alone, such a row is ignored on every boot, but takes effect
/// unnoticed once those secrets are saved in the dashboard — sending them to a host nobody
/// confirmed. The reset is audited as `system`; it commits only while the newest revision
/// is still the one judged (settings write lock), otherwise the rows are read and judged
/// again. A destination whose file/env value is itself invalid is left as it is (writing
/// it would put the next boot in safe mode); it stays ignored, reported in secret_problems.
async fn overlay_resetting_unpaired(
    pool: &PgPool,
    base: &Config,
    keys: Option<&KeyRing>,
    policy: BuildPolicy,
) -> Result<overlay::Overlay, sqlx::Error> {
    let mut stored = load_stored(pool).await?;
    for _ in 0..RESET_ATTEMPTS {
        let ov = overlay::apply_overlay(base, &stored, keys, policy);
        let resets: Vec<NewValue> = ov
            .unpaired_destinations
            .iter()
            .filter_map(|key| find(key))
            .map(|def| (def, (def.get)(base)))
            .filter(|(def, v)| def.validate(v).is_ok())
            .map(|(def, v)| NewValue { key: def.key.into(), payload: StoredPayload::Json(v) })
            .collect();
        if resets.is_empty() {
            return Ok(ov);
        }
        let snapshot = stored.iter().map(|r| r.rev).max().unwrap_or(0);
        match settings_db::write(pool, &resets, snapshot, "system").await {
            Ok(_) => warn!(
                keys = ?resets.iter().map(|v| v.key.as_str()).collect::<Vec<_>>(),
                "settings: reset a destination saved in the dashboard to its file/env value — its secrets were \
                 never saved there, and it would otherwise take effect once they are"
            ),
            Err(settings_db::SettingsDbError::Conflict { .. }) => {}
            Err(settings_db::SettingsDbError::Db(e)) => return Err(e),
        }
        stored = load_stored(pool).await?;
    }
    let ov = overlay::apply_overlay(base, &stored, keys, policy);
    if !ov.unpaired_destinations.is_empty() {
        warn!(
            keys = ?ov.unpaired_destinations,
            "settings: settings kept changing; an ignored destination is reset on the next start"
        );
    }
    Ok(ov)
}

/// The variable an unusable-key error names (the key ring is rejected as a whole).
fn key_var(e: &CryptoError) -> &'static str {
    match e {
        CryptoError::BadKey(var) => var,
        _ => crypto::KEY_ENV,
    }
}

/// With a key that is set but unusable, "not set" would send the operator looking in the
/// wrong place: say it could not be loaded, and list the key itself first.
fn report_key_error(key_error: Option<&CryptoError>, mut problems: Vec<Problem>) -> Vec<Problem> {
    let Some(e) = key_error else { return problems };
    let var = key_var(e);
    let not_set = overlay::no_key_reason();
    for p in problems.iter_mut().filter(|p| p.reason == not_set) {
        p.reason = format!("{var} is set but could not be loaded, so this secret cannot be decrypted");
    }
    problems.insert(
        0,
        Problem {
            key: var.into(),
            reason: format!(
                "could not be loaded ({e}); secrets keep their file/env values and cannot be saved in the \
                 dashboard until it is fixed"
            ),
        },
    );
    problems
}

/// Why "Apply & restart" is refused while MM_SETTINGS_SAFE_MODE is set.
pub const BREAK_GLASS_APPLY_REFUSED: &str =
    "MM_SETTINGS_SAFE_MODE is set: a restart would still ignore dashboard settings; remove it and recreate mm-core";

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
        let plan = overlay::import_plan(&base, keys.as_ref());
        for p in &plan.skipped {
            warn!(key = %p.key, reason = %p.reason, "settings: not imported (invalid value); it stays file/env-sourced");
        }
        // Never import a secret whose paired URL_CREDENTIALS destination was
        // already chosen in the database — otherwise a secret that only appears in
        // file/env on a later boot would be imported that day and count as `from_db`,
        // letting apply_overlay's own pairing check wave the DB-chosen destination through
        // paired with a secret that never came from the dashboard.
        // The guard judges the rows read under the import's own advisory lock, so
        // a destination moved by a save that committed while this import waited is seen
        // (a read taken before the lock would miss it). Those rows predate this import, so
        // it never judges rows it is about to create.
        let mut dropped: Vec<String> = vec![];
        let outcome = settings_db::import_filtered(&pool, |rows| {
            let existing: Vec<StoredSetting> = rows.iter().map(to_stored).collect();
            let (kept, d) = overlay::guard_import(plan.values, &existing, &base);
            dropped = d;
            kept.into_iter().map(|(key, v)| NewValue { key, payload: to_payload(v) }).collect()
        })
        .await?;
        for key in &dropped {
            warn!(key = %key, "settings: not imported — its paired destination was already chosen in the dashboard");
        }
        if outcome.first {
            info!(inserted = outcome.inserted, "settings: first-boot import of file/env values");
        } else if outcome.inserted > 0 {
            info!(inserted = outcome.inserted, "settings: imported values new to the database");
        }
        if let Some(k) = &keys {
            reencrypt_previous(&pool, k).await?;
        }

        let (config, status) = if opts.break_glass {
            let stored = load_stored(&pool).await?;
            warn!("settings: MM_SETTINGS_SAFE_MODE is set — dashboard settings ignored; running from file + env");
            let status = SettingsStatus {
                safe_mode: true,
                break_glass: true,
                safe_mode_reason: Some("MM_SETTINGS_SAFE_MODE is set".into()),
                loaded_rev: stored.iter().map(|r| r.rev).max().unwrap_or(0),
                secret_problems: report_key_error(opts.key_error.as_ref(), vec![]),
                ..Default::default()
            };
            (base.clone(), status)
        } else {
            let mut ov = overlay_resetting_unpaired(&pool, &base, keys.as_ref(), opts.policy).await?;
            ov.secret_problems = report_key_error(opts.key_error.as_ref(), ov.secret_problems);
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
                break_glass: false,
                safe_mode_reason: reason,
                loaded_rev: ov.loaded_rev,
                from_db: ov.from_db,
                secret_problems: ov.secret_problems,
                shadowed_env: shadowed,
                live_reload_error: None,
            };
            (ov.config, status)
        };
        // Advice about the config this instance runs (e.g. a Redis URL without
        // credentials) is logged here and when a live reload brings new advice, never on
        // the reads and saves that validate a config.
        for w in config.warnings() {
            warn!("{w}");
        }
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
            restart_at: std::sync::Mutex::new(None),
            reload_lock: tokio::sync::Mutex::new(()),
        }))
    }

    /// The live config handle handlers read from.
    pub fn handle(&self) -> &ConfigHandle {
        &self.handle
    }

    /// The settings store's pool, for checks that read other tables (the fleet's providers).
    pub fn pool(&self) -> &PgPool {
        &self.pool
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

    /// The build policy every check here applies (read once at startup), for startup's
    /// own last check of the effective config.
    pub fn build_policy(&self) -> BuildPolicy {
        self.opts.policy
    }
}

#[derive(Debug)]
pub enum PatchError {
    /// No such setting.
    Unknown(String),
    /// Bootstrap or host-coupled; the message says where to change it instead.
    ReadOnly(String),
    /// A secret was sent but no encryption key is loaded (not configured, or set but
    /// unusable). Carries the message for the operator.
    NoKey(String),
    /// A URL that secrets are sent to changed without those secrets (URL_CREDENTIALS).
    NeedsCredentials(String),
    Invalid(Vec<Problem>),
    Conflict { current_rev: i64 },
    /// "Apply & restart" under MM_SETTINGS_SAFE_MODE ([`BREAK_GLASS_APPLY_REFUSED`]).
    BreakGlass,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for PatchError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// See [`SettingsService::next_run`].
pub struct NextRun {
    /// The config the next restart would run.
    pub config: Config,
    /// Destinations paired with secrets that are not settled (stored or next ≠ running).
    pub unsettled: Vec<&'static str>,
}

/// A committed save: its revision, and the stored rows as the save left them (the rows it
/// validated against, with the ones it wrote replaced).
pub struct Saved {
    pub rev: i64,
    rows: Vec<SettingRow>,
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
    /// Set when the file/env value fails this setting's validation; `value` is then
    /// withheld (it may carry credentials, e.g. URL userinfo) and this says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The `GET /settings` body.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub schema: &'static [SettingDef],
    pub values: BTreeMap<&'static str, ValueView>,
    pub safe_mode: bool,
    /// Safe mode forced by MM_SETTINGS_SAFE_MODE: "Apply & restart" is refused (a restart
    /// would still ignore the saved settings). False in automatic safe mode.
    pub break_glass: bool,
    pub safe_mode_reason: Option<String>,
    pub loaded_rev: i64,
    /// Newest stored revision — the client sends it back as `expected_rev`.
    pub current_rev: i64,
    pub pending_restart: Vec<&'static str>,
    pub encryption_key_configured: bool,
    /// Secrets still encrypted under the previous key (0 after a finished rotation).
    pub rows_on_previous_key: usize,
    pub secret_problems: Vec<Problem>,
    /// The last live reload on this instance was rejected (key names and reasons).
    pub live_reload_error: Option<String>,
    pub demo: bool,
}

/// `ValueView::problem` for a file/env value that fails validation. Never quotes the value.
const INVALID_OUTSIDE_VALUE: &str =
    "the value from the server's file/env configuration is not valid for this setting, so it is not shown; \
     correct it there";

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
        Ok(self.next_run().await?.config)
    }

    /// [`Self::next_config`], and the destinations a secret must not be sent to without
    /// naming them (`overlay::unsettled_destinations`), from one read of the store: what a
    /// connection test runs against, under the same rule a save applies.
    pub async fn next_run(&self) -> Result<NextRun, sqlx::Error> {
        let stored = self.stored().await?;
        let config = overlay::apply_overlay(&self.base, &stored, self.keys.as_ref(), self.opts.policy).config;
        let unsettled = overlay::unsettled_destinations(&stored, &config, &self.handle.load());
        Ok(NextRun { config, unsettled })
    }

    fn no_key(&self, key: &str) -> PatchError {
        PatchError::NoKey(match &self.opts.key_error {
            None => format!(
                "{key} is a secret, and {} is not configured on this server, so it cannot be saved here",
                crypto::KEY_ENV
            ),
            Some(e) => format!(
                "{key} is a secret, and {} is set but could not be loaded on this server (see the settings \
                 status), so it cannot be saved here",
                key_var(e)
            ),
        })
    }

    fn encode(&self, def: &SettingDef, v: &Value) -> Result<StoredPayload, PatchError> {
        if !def.secret {
            return Ok(StoredPayload::Json(v.clone()));
        }
        let keys = self.keys.as_ref().ok_or_else(|| self.no_key(def.key))?;
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

    /// [`Self::save`], returning the new revision only.
    pub async fn patch(
        &self,
        changes: &BTreeMap<String, Value>,
        expected_rev: i64,
        actor: &str,
    ) -> Result<i64, PatchError> {
        self.save(changes, expected_rev, actor).await.map(|saved| saved.rev)
    }

    /// Validate every change and the combined next config, persist optimistically, then
    /// apply Live changes on this instance now (others follow within one poll).
    pub async fn save(
        &self,
        changes: &BTreeMap<String, Value>,
        expected_rev: i64,
        actor: &str,
    ) -> Result<Saved, PatchError> {
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
        // Everything below is validated against this snapshot, so `expected_rev`
        // must name exactly it. `settings_db::write` then re-checks it under the lock:
        // every writer takes a new revision under that lock, so any commit in between
        // (a save, another instance's import) moves max(rev) and makes this a Conflict.
        let snapshot_rev = rows.iter().map(|r| r.rev).max().unwrap_or(0);
        if snapshot_rev != expected_rev {
            return Err(PatchError::Conflict { current_rev: snapshot_rev });
        }
        let stored: Vec<StoredSetting> = rows.iter().map(to_stored).collect();
        let mut next = overlay::apply_overlay(&self.base, &stored, self.keys.as_ref(), self.opts.policy).config;

        // A URL that secrets are sent to changes only together with those secrets, so a
        // URL change can never redirect a stored secret to another host (URL_CREDENTIALS).
        // A secret counts as set when the next config carries it OR the database holds it
        // (even one this instance cannot decrypt right now).
        for pair in URL_CREDENTIALS {
            let Some(dest) = changes.get(pair.url) else { continue };
            // Re-sending the destination the next restart would use anyway moves
            // nothing, so it needs no re-entered secrets.
            if find(pair.url).is_some_and(|d| (d.get)(&next) == *dest) {
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

        // Secrets go to the destination the next restart runs. While a stored destination
        // differs from the running one (a move waiting for a restart, or a value ignored at
        // boot), saving its secrets alone would send them there with nobody confirming the
        // host; the save must name the destination too. A connection test applies the same
        // rule (`next_run`).
        let unsettled = overlay::unsettled_destinations(&stored, &next, &self.handle.load());
        if let Some((url, sent)) = overlay::unconfirmed_destination(|k| changes.contains_key(k), &unsettled) {
            return Err(PatchError::NeedsCredentials(format!(
                "{url} is not settled: its saved value, or the one a restart would run, differs from the \
                 running one, so saving {secrets} alone could send {them} to a host nobody confirmed; include \
                 {url} in the same save",
                secrets = sent.join(" and "),
                them = if sent.len() == 1 { "it" } else { "them" },
            )));
        }

        for (def, value) in &accepted {
            (def.set)(&mut next, (*value).clone())
                .map_err(|reason| PatchError::Invalid(vec![Problem { key: def.key.into(), reason }]))?;
        }
        overlay::validate_config(&next, self.opts.policy).map_err(|errors| {
            PatchError::Invalid(errors.into_iter().map(|reason| Problem { key: "*".into(), reason }).collect())
        })?;

        // A Live change is applied onto the RUNNING config, which can differ from the
        // next one (Restart values still pending). Dry-run that reload before saving: saved,
        // a change the running config rejects would fail every later live reload on every
        // instance until a restart. Safe mode applies nothing live, so it has nothing to check.
        if !self.status().safe_mode && accepted.iter().any(|(def, _)| def.class == ApplyClass::Live) {
            let mut after: Vec<StoredSetting> =
                stored.iter().filter(|r| !changes.contains_key(&r.key)).cloned().collect();
            after.extend(values.iter().map(|v| StoredSetting {
                key: v.key.clone(),
                value: match &v.payload {
                    StoredPayload::Json(j) => Stored::Json(j.clone()),
                    StoredPayload::Encrypted(b) => Stored::Encrypted(b.clone()),
                },
                rev: snapshot_rev + 1,
            }));
            if let Err(problems) = overlay::reload_live(&self.handle.load(), &after, self.keys.as_ref(), self.opts.policy) {
                return Err(PatchError::Invalid(
                    problems
                        .into_iter()
                        .map(|p| Problem {
                            reason: format!(
                                "{} — conflicts with the running configuration until \"Apply & restart\" \
                                 applies the pending changes",
                                p.reason
                            ),
                            key: p.key,
                        })
                        .collect(),
                ));
            }
        }

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
        let updated_at = Utc::now();
        let mut rows = rows;
        for v in values {
            let row = SettingRow { key: v.key, payload: v.payload, rev, updated_at, updated_by: actor.to_string() };
            match rows.iter_mut().find(|r| r.key == row.key) {
                Some(r) => *r = row,
                None => rows.push(row),
            }
        }
        Ok(Saved { rev, rows })
    }

    /// The view that answers a committed save. It never fails: a save reported as failed
    /// would be saved again. When the store cannot be read back (two tries), the view is
    /// built from the rows the save validated against and wrote, which is what a reload
    /// shows unless another save landed in between (the next poll or save then catches up).
    pub async fn view_after_save(&self, saved: Saved) -> SettingsView {
        for attempt in 1..=2 {
            match self.view(false).await {
                Ok(view) => return view,
                Err(e) => warn!(error = %e, attempt, "settings: saved, but reading the settings back failed"),
            }
        }
        self.view_of(saved.rows, false)
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
        let error = match overlay::reload_live(&self.handle.load(), &stored, self.keys.as_ref(), self.opts.policy) {
            Ok(cfg) => {
                let before = self.handle.load().warnings();
                for w in cfg.warnings().into_iter().filter(|w| !before.contains(w)) {
                    warn!("{w}");
                }
                if cfg.server.cors_origins.is_empty() && !self.handle.load().server.cors_origins.is_empty() {
                    warn!(
                        "settings: server.cors_origins is now empty, so only the localhost development origins \
                         may call the API from a browser; set explicit origins for production"
                    );
                }
                self.handle.store(cfg);
                None
            }
            Err(problems) => {
                for p in &problems {
                    warn!(key = %p.key, reason = %p.reason, "settings: live reload rejected; keeping the running values");
                }
                Some(problems.iter().map(|p| format!("{}: {}", p.key, p.reason)).collect::<Vec<_>>().join("; "))
            }
        };
        if self.status().live_reload_error != error {
            // Serialised by `reload_lock`; boot is the only other writer of `status`.
            self.status.rcu(|s| SettingsStatus { live_reload_error: error.clone(), ..(**s).clone() });
        }
        Ok(())
    }

    /// Saved settings this instance is not running yet and a restart would apply.
    pub async fn pending(&self) -> Result<Vec<&'static str>, sqlx::Error> {
        Ok(self.pending_in(&self.stored().await?))
    }

    /// Settings saved after this instance loaded its config that wait for a restart: the
    /// Restart-class ones, or every one in automatic safe mode (which applies nothing live;
    /// under break-glass a restart applies nothing, and "Apply & restart" is refused). Plus
    /// every destination paired with secrets whose next value differs from the running
    /// one, however old its row: where secrets go must never change without showing here.
    fn pending_in(&self, stored: &[StoredSetting]) -> Vec<&'static str> {
        let status = self.status();
        let every_class = status.safe_mode && !status.break_glass;
        let mut pending = overlay::pending_restart(stored, status.loaded_rev, every_class);
        let next = overlay::apply_overlay(&self.base, stored, self.keys.as_ref(), self.opts.policy).config;
        for key in overlay::moved_destinations(&next, &self.handle.load()) {
            if !pending.contains(&key) {
                pending.push(key);
            }
        }
        pending
    }

    /// "Apply & restart" (spec §6.3): dry-run the full next config; if it is valid, record
    /// the request — every instance whose loaded revision is older restarts — and schedule
    /// this instance's own restart when it is one of them.
    pub async fn apply_restart(&self, actor: &str) -> Result<ApplyOutcome, PatchError> {
        if self.opts.break_glass {
            info!(actor, "settings: \"Apply & restart\" refused — MM_SETTINGS_SAFE_MODE is set");
            return Err(PatchError::BreakGlass);
        }
        let ov = overlay::apply_overlay(&self.base, &self.stored().await?, self.keys.as_ref(), self.opts.policy);
        if !ov.problems.is_empty() {
            return Err(PatchError::Invalid(ov.problems));
        }
        let requested = settings_db::request_restart(&self.pool, actor).await?;
        let loaded = self.status().loaded_rev;
        info!(actor, requested, loaded, "settings: \"Apply & restart\" requested");
        if requested > loaded {
            // A restart already scheduled (e.g. by the poll) keeps its own deadline.
            let left = self.schedule_restart(self.opts.restart_delay).saturating_duration_since(Instant::now());
            Ok(ApplyOutcome::Restarting { in_secs: left.as_secs_f64().ceil() as u64 })
        } else {
            Ok(ApplyOutcome::NothingToRestart)
        }
    }

    /// Schedule this instance's restart `after` from now, at most once; returns when the
    /// restart fires (the earlier deadline when one was already scheduled).
    fn schedule_restart(&self, after: Duration) -> Instant {
        let mut at = self.restart_at.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(deadline) = *at {
            return deadline;
        }
        let deadline = Instant::now() + after;
        *at = Some(deadline);
        self.restart_scheduled.store(true, Ordering::SeqCst);
        let token = self.restart.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            info!("settings: restarting to apply saved settings");
            token.cancel();
        });
        deadline
    }

    /// One revision poll (spec §5.7): reload Live values when the newest revision moved;
    /// restart after a random delay when "Apply & restart" asked for a newer config.
    pub async fn poll_once(&self) -> Result<(), sqlx::Error> {
        if settings_db::max_rev(&self.pool).await? > self.last_seen_rev.load(Ordering::SeqCst) {
            self.reload_live_now().await?;
        }
        // A restart does not leave break-glass: it would only take the API down.
        if self.opts.break_glass {
            return Ok(());
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
        Ok(self.view_of(settings_db::load_all(&self.pool).await?, demo))
    }

    /// [`Self::view`] of the given stored rows.
    fn view_of(&self, rows: Vec<SettingRow>, demo: bool) -> SettingsView {
        let stored: Vec<StoredSetting> = rows.iter().map(to_stored).collect();
        let by_key: HashMap<&str, &SettingRow> = rows.iter().map(|r| (r.key.as_str(), r)).collect();
        let status = self.status();
        let running = self.handle.load();
        let defaults = Config::default();
        let pending = self.pending_in(&stored);
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
            let (value, is_set, problem) = if demo {
                (Some(Value::String("hidden".into())), None, None)
            } else if def.secret {
                (None, Some(self.secret_is_set(def, row, &running)), None)
            } else {
                match row {
                    Some(SettingRow { payload: StoredPayload::Json(v), .. }) => (Some(v.clone()), None, None),
                    _ => {
                        // A file/env value never passed the dashboard's validation
                        // (the import skips it) and may carry credentials, e.g. URL
                        // userinfo — withhold it rather than echo it.
                        let v = (def.get)(&running);
                        if def.validate(&v).is_ok() {
                            (Some(v), None, None)
                        } else {
                            (None, None, Some(INVALID_OUTSIDE_VALUE.to_string()))
                        }
                    }
                }
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
                    problem,
                },
            );
        }
        let rows_on_previous_key = self.keys.as_ref().map_or(0, |k| {
            rows.iter()
                .filter(|r| matches!(&r.payload, StoredPayload::Encrypted(b) if k.is_on_previous(b)))
                .count()
        });
        SettingsView {
            schema: registry(),
            values,
            safe_mode: status.safe_mode,
            break_glass: status.break_glass,
            // The demo role sees that safe mode is on, not why.
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
            // Hidden from the demo role, like the safe-mode reason.
            live_reload_error: if demo {
                status.live_reload_error.as_ref().map(|_| "hidden".to_string())
            } else {
                status.live_reload_error.clone()
            },
            demo,
        }
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
            assert_eq!(safe_mode_flag(input.map(std::ffi::OsStr::new)), *expected, "input {input:?}");
        }
    }

    /// A value that is not valid UTF-8 is set, just not readable: it is not a recognized
    /// "off" spelling, so break-glass stays on.
    #[cfg(unix)]
    #[test]
    fn safe_mode_flag_is_on_for_a_value_that_is_not_valid_utf8() {
        use std::os::unix::ffi::OsStrExt;
        assert!(safe_mode_flag(Some(std::ffi::OsStr::from_bytes(b"of\xff"))));
        assert!(safe_mode_flag(Some(std::ffi::OsStr::from_bytes(b"\xff"))));
    }
}
