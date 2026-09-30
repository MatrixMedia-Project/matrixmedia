//! Pure logic that turns stored settings into a running `Config` (spec §5.2–5.6): the
//! boot overlay with automatic safe mode, the first-boot import plan, live reload,
//! pending-restart and shadowed-env detection. No I/O.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use super::crypto::{self, KeyRing};
use super::{ApplyClass, SettingDef, URL_CREDENTIALS, find, registry};
use crate::config::{BuildPolicy, Config};

/// A stored value, as the overlay sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    Json(Value),
    Encrypted(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct StoredSetting {
    pub key: String,
    pub value: Stored,
    pub rev: i64,
}

/// Where a setting's shown value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    File,
    Env,
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Overlay {
    /// What to run: base + database, or exactly `base` in safe mode.
    pub config: Config,
    /// Newest revision among ALL rows, applied or not. The restart watcher compares
    /// against it, so a server in safe mode can't restart in a loop.
    pub loaded_rev: i64,
    /// Keys whose running value came from the database.
    pub from_db: BTreeSet<String>,
    /// Non-empty ⇒ automatic safe mode.
    pub problems: Vec<Problem>,
    /// Secrets that could not be decrypted; they keep their file/env value.
    pub secret_problems: Vec<Problem>,
    /// Stored keys this version doesn't manage (removed, or not editable).
    pub ignored: Vec<String>,
    /// Stored destinations (URL_CREDENTIALS) reverted to their file/env value because a
    /// paired secret set in file/env has never been stored in the database. Such a row is
    /// ignored on every boot, but would take effect unnoticed once those secrets are saved
    /// in the dashboard, so the caller resets it to the file/env value.
    pub unpaired_destinations: Vec<&'static str>,
}

impl Overlay {
    pub fn safe_mode_reason(&self) -> Option<String> {
        if self.problems.is_empty() {
            return None;
        }
        Some(self.problems.iter().map(|p| format!("{}: {}", p.key, p.reason)).collect::<Vec<_>>().join("; "))
    }
}

pub struct ImportPlan {
    pub values: Vec<(String, Stored)>,
    /// Values left out because they fail validation (they stay file/env-sourced).
    pub skipped: Vec<Problem>,
}

/// Rules no single setting can express (cross-field checks), including those that
/// depend on the build (`policy`), e.g. a release build refusing a mock Stripe key.
pub fn validate_config(c: &Config, policy: BuildPolicy) -> Result<(), Vec<String>> {
    let mut errors = vec![];
    if let Err(e) = c.validate() {
        errors.push(e);
    }
    if let Err(e) = c.monetization.validate_for(policy) {
        errors.push(e);
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

pub fn is_empty(v: &Value) -> bool {
    v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(|a| a.is_empty())
}

/// Why a stored secret cannot be decrypted when no key ring was passed. The caller that
/// knows the key is set but unusable replaces this reason with one saying so.
pub fn no_key_reason() -> String {
    format!("{} is not set, so this secret cannot be decrypted", crypto::KEY_ENV)
}

fn decode(def: &SettingDef, row: &StoredSetting, keys: Option<&KeyRing>) -> Result<Value, String> {
    match (&row.value, def.secret) {
        (Stored::Json(v), false) => Ok(v.clone()),
        (Stored::Encrypted(blob), true) => {
            let keys = keys.ok_or_else(no_key_reason)?;
            let plain = keys.decrypt(def.key, blob).map_err(|e| e.to_string())?;
            serde_json::from_slice(&plain).map_err(|_| "decrypted value is not valid JSON".to_string())
        }
        (Stored::Json(_), true) => Err("secret stored without encryption".into()),
        (Stored::Encrypted(_), false) => Err("non-secret value stored encrypted".into()),
    }
}

fn apply_row(def: &SettingDef, row: &StoredSetting, keys: Option<&KeyRing>, cfg: &mut Config) -> Result<(), String> {
    let v = decode(def, row, keys)?;
    def.validate(&v)?;
    (def.set)(cfg, v)
}

/// Boot overlay: `base` (file + env) plus every editable stored value, checked with the
/// same rules (`validate_config` under `policy`) the server applies to what it runs.
pub fn apply_overlay(base: &Config, rows: &[StoredSetting], keys: Option<&KeyRing>, policy: BuildPolicy) -> Overlay {
    let mut candidate = base.clone();
    let mut out = Overlay {
        config: base.clone(),
        loaded_rev: rows.iter().map(|r| r.rev).max().unwrap_or(0),
        from_db: BTreeSet::new(),
        problems: vec![],
        secret_problems: vec![],
        ignored: vec![],
        unpaired_destinations: vec![],
    };
    for row in rows {
        let Some(def) = find(&row.key).filter(|d| d.editable()) else {
            out.ignored.push(row.key.clone());
            continue;
        };
        match apply_row(def, row, keys, &mut candidate) {
            Ok(()) => {
                out.from_db.insert(row.key.clone());
            }
            Err(reason) => {
                let p = Problem { key: row.key.clone(), reason };
                if def.secret { out.secret_problems.push(p) } else { out.problems.push(p) }
            }
        }
    }
    // A database value for a destination paired with secrets (URL_CREDENTIALS) applies
    // only when every non-empty paired secret also came from the database; otherwise a
    // DB-chosen host could receive a secret that fell back to file/env (e.g. after a
    // decryption failure). The destination then keeps its file/env value.
    for pair in URL_CREDENTIALS {
        let Some(def) = find(pair.url) else { continue };
        if !out.from_db.contains(pair.url) || (def.get)(&candidate) == (def.get)(base) {
            continue;
        }
        let foreign: Vec<&str> = pair
            .secrets
            .iter()
            .copied()
            .filter(|s| !out.from_db.contains(*s))
            .filter(|s| find(s).is_some_and(|d| !is_empty(&(d.get)(&candidate))))
            .collect();
        if !foreign.is_empty() {
            let _ = (def.set)(&mut candidate, (def.get)(base));
            out.from_db.remove(pair.url);
            if !foreign.iter().any(|s| rows.iter().any(|r| r.key == *s)) {
                out.unpaired_destinations.push(pair.url);
            }
            out.secret_problems.push(Problem {
                key: pair.url.into(),
                reason: format!(
                    "the stored value is ignored because {} did not come from the dashboard",
                    foreign.join(" and ")
                ),
            });
        }
    }

    if out.problems.is_empty()
        && let Err(errors) = validate_config(&candidate, policy)
    {
        out.problems.extend(errors.into_iter().map(|reason| Problem { key: "*".into(), reason }));
    }
    if out.problems.is_empty() {
        out.config = candidate;
    } else {
        out.from_db.clear();
    }
    out
}

/// First-boot import (spec §5.3): the effective value of every editable setting.
/// Secrets are encrypted, or left out when no key is configured; empty secrets are left
/// out (nothing to protect); invalid values are left out and reported, so an odd `.env`
/// value stays env-sourced instead of forcing safe mode.
pub fn import_plan(base: &Config, keys: Option<&KeyRing>) -> ImportPlan {
    let mut plan = ImportPlan { values: vec![], skipped: vec![] };
    for def in registry().iter().filter(|d| d.editable()) {
        let v = (def.get)(base);
        if let Err(reason) = def.validate(&v) {
            plan.skipped.push(Problem { key: def.key.into(), reason });
            continue;
        }
        if def.secret {
            let Some(keys) = keys else { continue };
            if is_empty(&v) {
                continue;
            }
            let blob = keys.encrypt(def.key, v.to_string().as_bytes());
            plan.values.push((def.key.into(), Stored::Encrypted(blob)));
        } else {
            plan.values.push((def.key.into(), Stored::Json(v)));
        }
    }
    plan
}

/// Guard applied to the import plan before anything is written to the database:
/// a secret paired with a destination (`URL_CREDENTIALS`) must never be imported once that
/// destination already has a stored value different from `base` — i.e. it was chosen in the
/// dashboard. Without this, a secret that only appears in file/env on a LATER boot (after the
/// destination was already moved in the database) would be imported that day, count as
/// `from_db`, and let `apply_overlay`'s own pairing check wave the DB-chosen destination
/// through paired with a secret that never came from the dashboard — exactly the invariant
/// that check exists to protect. This runs earlier, before the secret is ever written.
///
/// A destination is never itself a secret (see the `url_credentials_pair_registered_editable_
/// settings` test in `settings::mod`), so its stored row is expected to decode as JSON; an
/// encrypted row for one is nonsensical and treated as "chosen" (fail closed) rather than
/// trusted.
///
/// Returns the values to keep and the keys dropped, for the caller to log by name only —
/// never log the value of a secret.
pub fn guard_import(
    values: Vec<(String, Stored)>,
    stored: &[StoredSetting],
    base: &Config,
) -> (Vec<(String, Stored)>, Vec<String>) {
    let chosen_in_db = |url: &str| -> bool {
        let Some(def) = find(url) else { return false };
        let Some(row) = stored.iter().find(|r| r.key == url) else { return false };
        match &row.value {
            Stored::Json(v) => *v != (def.get)(base),
            Stored::Encrypted(_) => true,
        }
    };
    let dropped_secrets: BTreeSet<&str> = URL_CREDENTIALS
        .iter()
        .filter(|pair| chosen_in_db(pair.url))
        .flat_map(|pair| pair.secrets.iter().copied())
        .collect();
    let mut dropped = vec![];
    let kept = values
        .into_iter()
        .filter(|(key, _)| {
            if dropped_secrets.contains(key.as_str()) {
                dropped.push(key.clone());
                false
            } else {
                true
            }
        })
        .collect();
    (kept, dropped)
}

/// Re-apply the stored values of Live settings onto the running config; Restart-class
/// values stay as loaded. A secret that fails to decrypt keeps its running value. Any
/// other bad row, or an invalid result (`validate_config` under `policy`), rejects the
/// whole reload.
pub fn reload_live(
    current: &Config,
    rows: &[StoredSetting],
    keys: Option<&KeyRing>,
    policy: BuildPolicy,
) -> Result<Config, Vec<Problem>> {
    let mut candidate = current.clone();
    let mut problems = vec![];
    for row in rows {
        let Some(def) = find(&row.key).filter(|d| d.class == ApplyClass::Live) else { continue };
        if let Err(reason) = apply_row(def, row, keys, &mut candidate)
            && !def.secret
        {
            problems.push(Problem { key: row.key.clone(), reason });
        }
    }
    if problems.is_empty()
        && let Err(errors) = validate_config(&candidate, policy)
    {
        problems.extend(errors.into_iter().map(|reason| Problem { key: "*".into(), reason }));
    }
    if problems.is_empty() { Ok(candidate) } else { Err(problems) }
}

/// Settings saved after this instance loaded its config that wait for a restart (spec
/// §5.6): the Restart-class ones, and the Live ones too when `every_class` is set (the
/// caller's automatic safe mode, which applies nothing live).
pub fn pending_restart(rows: &[StoredSetting], loaded_rev: i64, every_class: bool) -> Vec<&'static str> {
    rows.iter()
        .filter(|r| r.rev > loaded_rev)
        .filter_map(|r| find(&r.key))
        .filter(|d| d.class == ApplyClass::Restart || (every_class && d.class == ApplyClass::Live))
        .map(|d| d.key)
        .collect()
}

/// Destinations paired with secrets (URL_CREDENTIALS) whose value in `next` (what a restart
/// would run) differs from `running`. An older destination row can start to take effect
/// because of newer rows (e.g. its secrets saved later), so a revision check alone would
/// miss it; where secrets are sent must never change without showing up as pending.
pub fn moved_destinations<'a>(next: &'a Config, running: &'a Config) -> impl Iterator<Item = &'static str> + 'a {
    URL_CREDENTIALS
        .iter()
        .filter_map(|pair| find(pair.url))
        .filter(move |d| (d.get)(next) != (d.get)(running))
        .map(|d| d.key)
}

/// Destinations paired with secrets (URL_CREDENTIALS) that are not settled: the stored value
/// differs from the `running` one (a move waiting for a restart, or a value ignored at boot),
/// or `next` — what a restart would run — does. A secret sent without naming such a
/// destination could end up at a host nobody confirmed. An encrypted row for a destination
/// (never written by the service) counts as unsettled.
pub fn unsettled_destinations(stored: &[StoredSetting], next: &Config, running: &Config) -> Vec<&'static str> {
    URL_CREDENTIALS
        .iter()
        .filter_map(|pair| find(pair.url))
        .filter(|def| {
            let now = (def.get)(running);
            let stored_elsewhere = stored.iter().find(|r| r.key == def.key).is_some_and(|r| match &r.value {
                Stored::Json(v) => *v != now,
                Stored::Encrypted(_) => true,
            });
            stored_elsewhere || (def.get)(next) != now
        })
        .map(|def| def.key)
        .collect()
}

/// The rule a save and a connection test share: the first unsettled destination
/// ([`unsettled_destinations`]) whose secrets are among `sent` while the destination itself
/// is not, with those secrets. `None` when every secret sent goes to a settled or named host.
pub fn unconfirmed_destination(
    sent: impl Fn(&str) -> bool,
    unsettled: &[&str],
) -> Option<(&'static str, Vec<&'static str>)> {
    URL_CREDENTIALS.iter().filter(|pair| unsettled.contains(&pair.url) && !sent(pair.url)).find_map(|pair| {
        let secrets: Vec<&'static str> = pair.secrets.iter().copied().filter(|s| sent(s)).collect();
        (!secrets.is_empty()).then_some((pair.url, secrets))
    })
}

/// Env vars that are set but ignored because the database owns the setting (spec §5.4).
pub fn shadowed_env(from_db: &BTreeSet<String>, is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    registry()
        .iter()
        .filter(|d| from_db.contains(d.key))
        .filter_map(|d| d.env)
        .filter(|v| is_set(v) || is_set(&format!("{v}_FROM_FILE")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const K2: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
    /// A debug build: no build-dependent rule applies.
    const DEV: BuildPolicy = BuildPolicy { release_build: false, allow_mock: false };
    const RELEASE: BuildPolicy = BuildPolicy { release_build: true, allow_mock: false };

    fn ring(k: &str) -> KeyRing {
        KeyRing::from_values(Some(k), None).unwrap().unwrap()
    }
    fn row(key: &str, v: Value, rev: i64) -> StoredSetting {
        StoredSetting { key: key.into(), value: Stored::Json(v), rev }
    }
    fn secret_row(r: &KeyRing, key: &str, v: Value, rev: i64) -> StoredSetting {
        StoredSetting { key: key.into(), value: Stored::Encrypted(r.encrypt(key, v.to_string().as_bytes())), rev }
    }
    fn same(a: &Config, b: &Config) -> bool {
        registry().iter().all(|d| (d.get)(a) == (d.get)(b))
    }

    #[test]
    fn no_rows_runs_the_base_config() {
        let base = Config::default();
        let ov = apply_overlay(&base, &[], None, DEV);
        assert!(same(&ov.config, &base));
        assert_eq!((ov.loaded_rev, ov.safe_mode_reason()), (0, None));
    }

    #[test]
    fn live_and_restart_rows_win_over_the_base() {
        let ov = apply_overlay(
            &Config::default(),
            &[row("server.cors_origins", json!(["https://a.example"]), 3), row("server.drain_seconds", json!(45), 5)],
            None,
            DEV,
        );
        assert_eq!(ov.config.server.cors_origins, vec!["https://a.example"]);
        assert_eq!(ov.config.server.drain_seconds, 45);
        assert_eq!(ov.loaded_rev, 5);
        assert!(ov.from_db.contains("server.cors_origins") && ov.from_db.contains("server.drain_seconds"));
    }

    #[test]
    fn bootstrap_host_coupled_and_unknown_rows_are_ignored_not_fatal() {
        let mut base = Config::default();
        base.jwt_signing_key = "k".repeat(40);
        let ov = apply_overlay(
            &base,
            &[row("jwt_signing_key", json!("evil"), 1), row("matrix.as_token", json!("x"), 2), row("gone.setting", json!(1), 3)],
            None,
            DEV,
        );
        assert_eq!(ov.config.jwt_signing_key, base.jwt_signing_key);
        assert_eq!(ov.config.matrix.as_token, "");
        assert_eq!(ov.ignored, vec!["jwt_signing_key", "matrix.as_token", "gone.setting"]);
        assert_eq!(ov.safe_mode_reason(), None);
        assert_eq!(ov.loaded_rev, 3);
    }

    #[test]
    fn a_malformed_row_means_safe_mode_on_the_base() {
        let base = Config::default();
        let ov = apply_overlay(
            &base,
            // The bad row is the newest AND not last, so loaded_rev can't come out right
            // by accident from either "only applied rows" or "just the last row".
            &[row("server.cors_origins", json!("not-a-list"), 8), row("recording.retention_days", json!(5), 7)],
            None,
            DEV,
        );
        assert!(same(&ov.config, &base), "safe mode runs file + env only");
        assert_eq!(ov.problems[0].key, "server.cors_origins");
        assert!(ov.from_db.is_empty());
        assert_eq!(ov.loaded_rev, 8, "loaded_rev still covers every row (restart-loop guard)");
        assert!(ov.safe_mode_reason().unwrap().contains("server.cors_origins"));
    }

    #[test]
    fn an_out_of_range_row_means_safe_mode() {
        let ov = apply_overlay(&Config::default(), &[row("turn.ttl_secs", json!(5), 1)], None, DEV);
        assert_eq!(ov.problems[0].key, "turn.ttl_secs");
    }

    #[test]
    fn an_invalid_combination_means_safe_mode() {
        let mut base = Config::default();
        base.monetization.enabled = true;
        base.monetization.postgres_url = "postgres://x".into();
        base.monetization.stripe_secret_key = "sk_test_x".into();
        base.monetization.webhook_signing_secret = "whsec_x".into();
        assert!(validate_config(&base, DEV).is_ok(), "fixture must start valid");
        let ov = apply_overlay(
            &base,
            &[row("monetization.min_donation_cents", json!(5000), 1), row("monetization.max_donation_cents", json!(1000), 2)],
            None,
            DEV,
        );
        assert_eq!(ov.problems[0].key, "*");
        assert_eq!(ov.config.monetization.min_donation_cents, base.monetization.min_donation_cents);
    }

    #[test]
    fn secrets_decrypt_with_the_key() {
        let r = ring(K1);
        let ov = apply_overlay(&Config::default(), &[secret_row(&r, "storage.s3.secret_key", json!("s3-secret"), 1)], Some(&r), DEV);
        assert_eq!(ov.config.storage.s3.secret_key, "s3-secret");
        assert!(ov.secret_problems.is_empty());
    }

    #[test]
    fn without_the_key_secrets_keep_their_env_value_and_nothing_else_breaks() {
        let mut base = Config::default();
        base.storage.s3.secret_key = "from-env".into();
        let rows = [secret_row(&ring(K1), "storage.s3.secret_key", json!("stored"), 1), row("recording.retention_days", json!(5), 2)];
        let ov = apply_overlay(&base, &rows, None, DEV);
        assert_eq!(ov.config.storage.s3.secret_key, "from-env");
        assert_eq!(ov.config.recording.retention_days, 5);
        assert_eq!(ov.secret_problems[0].key, "storage.s3.secret_key");
        assert_eq!(ov.safe_mode_reason(), None);
    }

    #[test]
    fn a_wrong_key_is_a_secret_problem_not_safe_mode() {
        let rows = [secret_row(&ring(K1), "storage.s3.secret_key", json!("stored"), 1)];
        let ov = apply_overlay(&Config::default(), &rows, Some(&ring(K2)), DEV);
        assert_eq!(ov.secret_problems.len(), 1);
        assert_eq!(ov.safe_mode_reason(), None);
    }

    #[test]
    fn a_secret_copied_onto_another_setting_does_not_decrypt() {
        let r = ring(K1);
        let mut moved = secret_row(&r, "storage.s3.access_key", json!("x"), 1);
        moved.key = "storage.s3.secret_key".into();
        let ov = apply_overlay(&Config::default(), &[moved], Some(&r), DEV);
        assert_eq!(ov.secret_problems[0].key, "storage.s3.secret_key");
        assert_eq!(ov.config.storage.s3.secret_key, "");
    }

    #[test]
    fn a_secret_stored_in_plain_json_is_rejected() {
        let ov = apply_overlay(&Config::default(), &[row("storage.s3.secret_key", json!("plain"), 1)], Some(&ring(K1)), DEV);
        assert_eq!(ov.secret_problems[0].key, "storage.s3.secret_key");
        assert_eq!(ov.config.storage.s3.secret_key, "");
    }

    #[test]
    fn a_stored_destination_needs_its_secrets_from_the_database_too() {
        let r = ring(K1);
        let mut base = Config::default();
        base.monetization.lnbits_url = "http://lnbits:5000".into();
        base.monetization.lnbits_invoice_key = "env-key".into();
        // DB moves the URL, but its key could not be decrypted and fell back to env.
        let rows = [
            row("monetization.lnbits_url", json!("https://elsewhere.example"), 1),
            secret_row(&ring(K2), "monetization.lnbits_invoice_key", json!("db-key"), 2),
        ];
        let ov = apply_overlay(&base, &rows, Some(&r), DEV);
        assert_eq!(ov.config.monetization.lnbits_url, "http://lnbits:5000", "env destination kept");
        assert_eq!(ov.config.monetization.lnbits_invoice_key, "env-key");
        assert!(ov.secret_problems.iter().any(|p| p.key == "monetization.lnbits_url"));
        assert!(!ov.from_db.contains("monetization.lnbits_url"), "reverted destination is not reported as database-sourced");
        assert_eq!(ov.safe_mode_reason(), None);
        assert!(
            ov.unpaired_destinations.is_empty(),
            "its key WAS entered in the dashboard (it just cannot be decrypted here): the stored destination is kept"
        );

        // Both from the database: the move applies.
        let rows = [
            row("monetization.lnbits_url", json!("https://elsewhere.example"), 1),
            secret_row(&r, "monetization.lnbits_invoice_key", json!("db-key"), 2),
        ];
        let ov = apply_overlay(&base, &rows, Some(&r), DEV);
        assert_eq!(ov.config.monetization.lnbits_url, "https://elsewhere.example");

        // Same value as file/env (the normal no-key import): nothing to report.
        let rows = [row("monetization.lnbits_url", json!("http://lnbits:5000"), 1)];
        let ov = apply_overlay(&base, &rows, None, DEV);
        assert!(ov.secret_problems.is_empty());
    }

    #[test]
    fn a_destination_paired_with_two_secrets_needs_both_from_the_database() {
        // storage.s3.endpoint is paired with two secrets (access_key, secret_key). If
        // only one of them is stored/decryptable and the other stays env-only, the move
        // must be refused even though *a* paired secret did come from the database.
        let r = ring(K1);
        let mut base = Config::default();
        base.storage.s3.secret_key = "env-secret".into();
        let rows = [row("storage.s3.endpoint", json!("https://new.s3.example"), 1)];
        let ov = apply_overlay(&base, &rows, None, DEV);
        assert_eq!(ov.config.storage.s3.endpoint, base.storage.s3.endpoint, "kept: secret_key is env-only");
        assert!(ov.secret_problems.iter().any(|p| p.key == "storage.s3.endpoint"));
        assert!(!ov.from_db.contains("storage.s3.endpoint"));
        assert_eq!(ov.safe_mode_reason(), None);
        assert_eq!(ov.unpaired_destinations, vec!["storage.s3.endpoint"], "secret_key was never stored");

        // One paired secret comes from the database, the other (secret_key) is still
        // env-only: the move must be refused even though *a* paired secret did come from
        // the database (an any-instead-of-all check over the pair would wrongly allow
        // this, since access_key alone is present in from_db).
        let rows = [
            row("storage.s3.endpoint", json!("https://new.s3.example"), 1),
            secret_row(&r, "storage.s3.access_key", json!("db-access"), 2),
        ];
        let ov = apply_overlay(&base, &rows, Some(&r), DEV);
        assert_eq!(ov.config.storage.s3.endpoint, base.storage.s3.endpoint, "kept: secret_key is still env-only");
        assert!(!ov.from_db.contains("storage.s3.endpoint"));
        let p = ov
            .secret_problems
            .iter()
            .find(|p| p.key == "storage.s3.endpoint")
            .expect("secret_problems should name storage.s3.endpoint");
        assert!(p.reason.contains("storage.s3.secret_key"), "reason should name the missing secret: {}", p.reason);
        assert!(
            !p.reason.contains("storage.s3.access_key"),
            "must not blame the key that DID come from the database: {}",
            p.reason
        );

        // Both paired secrets come from the database too: the move applies.
        let rows = [
            row("storage.s3.endpoint", json!("https://new.s3.example"), 1),
            secret_row(&r, "storage.s3.access_key", json!("db-access"), 2),
            secret_row(&r, "storage.s3.secret_key", json!("db-secret"), 3),
        ];
        let ov = apply_overlay(&base, &rows, Some(&r), DEV);
        assert_eq!(ov.config.storage.s3.endpoint.as_deref(), Some("https://new.s3.example"));
        assert!(ov.from_db.contains("storage.s3.endpoint"));
        assert!(ov.secret_problems.is_empty());
    }

    fn lnbits_env() -> Config {
        let mut base = Config::default();
        base.monetization.lnbits_url = "http://lnbits:5000".into();
        base.monetization.lnbits_invoice_key = "env-invoice".into();
        base.monetization.lnbits_admin_key = "env-admin".into();
        base
    }

    #[test]
    fn a_destination_whose_secrets_were_never_stored_is_reported_for_reset() {
        // Saved in the dashboard while no LNbits keys were set anywhere; the keys then came
        // from file/env. The destination is ignored, and it would take effect silently the
        // day the keys are saved in the dashboard, so the caller resets the stored row.
        let rows = [row("monetization.lnbits_url", json!("https://elsewhere.example"), 1)];
        let ov = apply_overlay(&lnbits_env(), &rows, Some(&ring(K1)), DEV);
        assert_eq!(ov.config.monetization.lnbits_url, "http://lnbits:5000");
        assert_eq!(ov.unpaired_destinations, vec!["monetization.lnbits_url"]);
        assert_eq!(ov.safe_mode_reason(), None);

        // Nothing to reset when the destination was not reverted at all.
        let ov = apply_overlay(&Config::default(), &rows, Some(&ring(K1)), DEV);
        assert_eq!(ov.config.monetization.lnbits_url, "https://elsewhere.example");
        assert!(ov.unpaired_destinations.is_empty());
    }

    #[test]
    fn a_destination_is_not_reset_while_any_of_its_missing_secrets_has_a_stored_row() {
        // The invoice key was stored (under another key ring, so it cannot be decrypted here)
        // and the admin key never was: a key problem, not an abandoned destination.
        let rows = [
            row("monetization.lnbits_url", json!("https://elsewhere.example"), 1),
            secret_row(&ring(K2), "monetization.lnbits_invoice_key", json!("db-invoice"), 2),
        ];
        let ov = apply_overlay(&lnbits_env(), &rows, Some(&ring(K1)), DEV);
        assert_eq!(ov.config.monetization.lnbits_url, "http://lnbits:5000", "still reverted");
        assert!(ov.unpaired_destinations.is_empty(), "but kept for when the key is fixed");
    }

    #[test]
    fn a_stored_mock_stripe_key_means_safe_mode_in_a_release_build() {
        let r = ring(K1);
        let mut base = Config::default();
        base.monetization.enabled = true;
        base.monetization.postgres_url = "postgres://x".into();
        base.monetization.stripe_secret_key = "sk_test_x".into();
        base.monetization.webhook_signing_secret = "whsec_x".into();
        let rows = [secret_row(&r, "monetization.stripe_secret_key", json!("sk_test_mock_x"), 1)];

        let ov = apply_overlay(&base, &rows, Some(&r), RELEASE);
        assert_eq!(ov.problems.len(), 1, "{:?}", ov.problems);
        assert_eq!(ov.problems[0].key, "*");
        assert!(ov.problems[0].reason.contains("MM_ALLOW_MOCK"), "{}", ov.problems[0].reason);
        assert_eq!(ov.config.monetization.stripe_secret_key, "sk_test_x", "safe mode runs file + env only");

        for policy in [DEV, BuildPolicy { release_build: true, allow_mock: true }] {
            let ov = apply_overlay(&base, &rows, Some(&r), policy);
            assert_eq!(ov.safe_mode_reason(), None, "{policy:?}");
            assert_eq!(ov.config.monetization.stripe_secret_key, "sk_test_mock_x", "{policy:?}");
        }
    }

    #[test]
    fn validate_config_applies_the_build_policy() {
        let mut c = Config::default();
        c.monetization.enabled = true;
        c.monetization.postgres_url = "postgres://x".into();
        c.monetization.stripe_secret_key = "sk_test_mock_x".into();
        c.monetization.webhook_signing_secret = "whsec_x".into();
        assert!(validate_config(&c, DEV).is_ok());
        assert!(validate_config(&c, RELEASE).is_err());
    }

    #[test]
    fn import_plan_covers_editable_settings_only() {
        let mut base = Config::default();
        base.storage.s3.secret_key = "s3-secret".into();
        // Bootstrap / HostCoupled settings must never be offered for import, no matter
        // how the registry state is filtered.
        base.server.public_url = Some("https://x.example".into());
        base.matrix.homeserver_url = "http://synapse:8008".into();
        base.jwt_signing_key = "k".repeat(40);
        let without = import_plan(&base, None);
        let keys: Vec<_> = without.values.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"server.cors_origins"));
        assert!(!keys.contains(&"storage.s3.secret_key"), "no key ⇒ secrets stay env-sourced");
        assert!(!keys.contains(&"jwt_signing_key") && !keys.contains(&"matrix.as_token"));
        assert!(!keys.contains(&"server.public_url"), "Bootstrap settings are never in the import plan");
        assert!(!keys.contains(&"matrix.homeserver_url"), "HostCoupled settings are never in the import plan");

        let r = ring(K1);
        let with = import_plan(&base, Some(&r));
        let (_, v) = with.values.iter().find(|(k, _)| k == "storage.s3.secret_key").unwrap();
        let Stored::Encrypted(blob) = v else { panic!("secret must be encrypted") };
        assert_eq!(r.decrypt("storage.s3.secret_key", blob).unwrap(), br#""s3-secret""#);
        assert!(!with.values.iter().any(|(k, _)| k == "storage.s3.access_key"), "empty secrets are skipped");
        assert!(
            !with.values.iter().any(|(k, _)| k == "jwt_signing_key"),
            "a Bootstrap secret stays out even with a key configured"
        );
    }

    #[test]
    fn import_plan_skips_values_that_fail_validation() {
        let mut base = Config::default();
        base.server.cors_origins = vec!["https://a.example/".into()];
        let plan = import_plan(&base, None);
        assert!(!plan.values.iter().any(|(k, _)| k == "server.cors_origins"));
        assert_eq!(plan.skipped[0].key, "server.cors_origins");
    }

    #[test]
    fn guard_import_drops_secrets_whose_lnbits_destination_was_chosen_in_the_database() {
        let mut base = Config::default();
        base.monetization.lnbits_url = "http://lnbits:5000".into();
        let values = vec![
            ("monetization.lnbits_url".to_string(), Stored::Json(json!("http://lnbits:5000"))),
            ("monetization.lnbits_invoice_key".to_string(), Stored::Json(json!("env-invoice"))),
            ("monetization.lnbits_admin_key".to_string(), Stored::Json(json!("env-admin"))),
            ("server.cors_origins".to_string(), Stored::Json(json!(["https://a.example"]))),
        ];
        let stored = [row("monetization.lnbits_url", json!("https://elsewhere.example"), 1)];
        let (kept, dropped) = guard_import(values, &stored, &base);
        let kept_keys: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!kept_keys.contains(&"monetization.lnbits_invoice_key"));
        assert!(!kept_keys.contains(&"monetization.lnbits_admin_key"));
        assert_eq!(dropped.len(), 2);
        assert!(dropped.contains(&"monetization.lnbits_invoice_key".to_string()));
        assert!(dropped.contains(&"monetization.lnbits_admin_key".to_string()));
        // The destination itself (not a secret) and an unrelated key are untouched.
        assert!(kept_keys.contains(&"monetization.lnbits_url"));
        assert!(kept_keys.contains(&"server.cors_origins"), "a secret not in URL_CREDENTIALS is left alone");
    }

    #[test]
    fn guard_import_keeps_secrets_when_the_stored_lnbits_destination_equals_base() {
        let mut base = Config::default();
        base.monetization.lnbits_url = "http://lnbits:5000".into();
        let values = vec![("monetization.lnbits_invoice_key".to_string(), Stored::Json(json!("env-invoice")))];
        let stored = [row("monetization.lnbits_url", json!("http://lnbits:5000"), 1)];
        let (kept, dropped) = guard_import(values, &stored, &base);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn guard_import_drops_s3_secrets_when_the_bucket_destination_was_chosen_in_the_database() {
        // Covers the second URL_CREDENTIALS pair (storage.s3.bucket), and that either
        // destination in a multi-destination pair is enough to trigger the guard even
        // when the OTHER destination (storage.s3.endpoint) has no stored row at all.
        let base = Config::default();
        let values = vec![
            ("storage.s3.access_key".to_string(), Stored::Json(json!("env-access"))),
            ("storage.s3.secret_key".to_string(), Stored::Json(json!("env-secret"))),
        ];
        let stored = [row("storage.s3.bucket", json!("new-bucket"), 1)];
        let (kept, dropped) = guard_import(values, &stored, &base);
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn guard_import_keeps_secrets_when_the_destination_has_no_stored_row() {
        let base = Config::default();
        let values = vec![("storage.s3.secret_key".to_string(), Stored::Json(json!("env-secret")))];
        let (kept, dropped) = guard_import(values, &[], &base);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn guard_import_leaves_non_paired_secrets_alone() {
        // server.request_webhook_url is `secret: true` but not part of URL_CREDENTIALS at
        // all; a chosen destination elsewhere must never affect it.
        let mut base = Config::default();
        base.monetization.lnbits_url = "http://lnbits:5000".into();
        let values = vec![("server.request_webhook_url".to_string(), Stored::Json(json!("https://hooks.example")))];
        let stored = [row("monetization.lnbits_url", json!("https://elsewhere.example"), 1)];
        let (kept, dropped) = guard_import(values, &stored, &base);
        assert!(dropped.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn pending_restart_lists_restart_rows_newer_than_loaded() {
        let rows = [
            row("server.drain_seconds", json!(10), 4),
            row("monetization.enabled", json!(true), 9),
            row("server.cors_origins", json!([]), 10),
        ];
        assert_eq!(pending_restart(&rows, 5, false), vec!["monetization.enabled"]);
        assert!(pending_restart(&rows, 9, false).is_empty(), "equal to its own rev is not newer");
        assert!(pending_restart(&rows, 10, false).is_empty());
    }

    #[test]
    fn pending_restart_lists_every_newer_row_when_nothing_applies_live() {
        let rows = [
            row("server.drain_seconds", json!(10), 4),
            row("monetization.enabled", json!(true), 9),
            row("server.cors_origins", json!([]), 10),
            row("gone.setting", json!(1), 11),
            row("jwt_signing_key", json!("x"), 12),
        ];
        assert_eq!(pending_restart(&rows, 5, true), vec!["monetization.enabled", "server.cors_origins"]);
        assert!(pending_restart(&rows, 10, true).is_empty(), "an unmanaged or read-only key is never pending");
    }

    #[test]
    fn moved_destinations_names_each_paired_destination_whose_next_value_differs() {
        let running = Config::default();
        let mut next = running.clone();
        assert_eq!(moved_destinations(&next, &running).count(), 0);
        next.monetization.lnbits_url = "https://elsewhere.example".into();
        next.storage.s3.bucket = "other-bucket".into();
        next.server.drain_seconds = 1; // not a destination
        assert_eq!(
            moved_destinations(&next, &running).collect::<Vec<_>>(),
            vec!["monetization.lnbits_url", "storage.s3.bucket"]
        );
    }

    #[test]
    fn unsettled_destinations_are_those_stored_or_next_elsewhere_than_running() {
        let running = Config::default();
        let next = running.clone();
        let here = json!(running.monetization.lnbits_url);
        assert!(unsettled_destinations(&[], &next, &running).is_empty(), "no rows, nothing moved");
        let settled = [row("monetization.lnbits_url", here, 1), row("storage.s3.bucket", json!(running.storage.s3.bucket), 2)];
        assert!(unsettled_destinations(&settled, &next, &running).is_empty(), "stored = running = next");

        // A pending move: the restart would run another host.
        let mut moved = next.clone();
        moved.monetization.lnbits_url = "https://elsewhere.example".into();
        assert_eq!(unsettled_destinations(&settled, &moved, &running), vec!["monetization.lnbits_url"]);

        // A stored value ignored at boot: next and running agree, the row does not.
        let ignored = [row("storage.s3.bucket", json!("other-bucket"), 3)];
        assert_eq!(unsettled_destinations(&ignored, &next, &running), vec!["storage.s3.bucket"]);

        // A destination row stored encrypted is never trusted.
        let odd = [secret_row(&ring(K1), "storage.s3.endpoint", json!(null), 4)];
        assert_eq!(unsettled_destinations(&odd, &next, &running), vec!["storage.s3.endpoint"]);
    }

    #[test]
    fn a_secret_sent_without_its_unsettled_destination_is_unconfirmed() {
        let sent = |keys: &'static [&'static str]| move |k: &str| keys.contains(&k);
        let ln = ["monetization.lnbits_url"];
        assert_eq!(
            unconfirmed_destination(sent(&["monetization.lnbits_admin_key", "server.drain_seconds"]), &ln),
            Some(("monetization.lnbits_url", vec!["monetization.lnbits_admin_key"]))
        );
        assert_eq!(
            unconfirmed_destination(sent(&["monetization.lnbits_url", "monetization.lnbits_admin_key"]), &ln),
            None,
            "naming the destination confirms it"
        );
        assert_eq!(unconfirmed_destination(sent(&["monetization.lnbits_admin_key"]), &[]), None, "settled");
        assert_eq!(unconfirmed_destination(sent(&["monetization.lnbits_url"]), &ln), None, "no secret sent");
        assert_eq!(unconfirmed_destination(sent(&["storage.s3.access_key"]), &ln), None, "another pair's secret");

        // The S3 secrets go to two destinations; naming one leaves the other unconfirmed.
        let s3 = ["storage.s3.endpoint", "storage.s3.bucket"];
        assert_eq!(
            unconfirmed_destination(sent(&["storage.s3.endpoint", "storage.s3.secret_key", "storage.s3.access_key"]), &s3),
            Some(("storage.s3.bucket", vec!["storage.s3.access_key", "storage.s3.secret_key"]))
        );
    }

    #[test]
    fn reload_live_applies_live_rows_only() {
        let mut current = Config::default();
        current.server.drain_seconds = 30;
        let rows = [row("server.cors_origins", json!(["https://b.example"]), 2), row("server.drain_seconds", json!(99), 3)];
        let next = reload_live(&current, &rows, None, DEV).unwrap();
        assert_eq!(next.server.cors_origins, vec!["https://b.example"]);
        assert_eq!(next.server.drain_seconds, 30, "restart-class waits");
    }

    #[test]
    fn reload_live_rejects_a_bad_live_row_and_changes_nothing() {
        let err = reload_live(&Config::default(), &[row("turn.ttl_secs", json!(1), 1)], None, DEV).unwrap_err();
        assert_eq!(err[0].key, "turn.ttl_secs");
    }

    #[test]
    fn reload_live_keeps_a_secret_it_cannot_decrypt() {
        let mut current = Config::default();
        current.server.request_webhook_url = Some("https://running.example/hook".into());
        let rows = [secret_row(&ring(K1), "server.request_webhook_url", json!("https://new.example/hook"), 1)];
        let next = reload_live(&current, &rows, Some(&ring(K2)), DEV).unwrap();
        assert_eq!(next.server.request_webhook_url.as_deref(), Some("https://running.example/hook"));
    }

    #[test]
    fn shadowed_env_reports_plain_and_from_file_variants() {
        let from_db: BTreeSet<String> = ["server.cors_origins", "turn.shared_secret", "video.max_bitrate"].map(String::from).into();
        let set = |v: &str| v == "MM_CORS_ORIGINS" || v == "MM_VIDEO_MAX_BITRATE_FROM_FILE" || v == "MM_E2EE_ENABLED";
        assert_eq!(shadowed_env(&from_db, set), vec!["MM_CORS_ORIGINS", "MM_VIDEO_MAX_BITRATE"]);
    }
}
