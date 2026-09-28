//! Pure logic that turns stored settings into a running `Config` (spec §5.2–5.6): the
//! boot overlay with automatic safe mode, the first-boot import plan, live reload,
//! pending-restart and shadowed-env detection. No I/O.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use super::crypto::{self, KeyRing};
use super::{ApplyClass, SettingDef, URL_CREDENTIALS, find, registry};
use crate::config::Config;

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

/// Rules no single setting can express (cross-field checks).
pub fn validate_config(c: &Config) -> Result<(), Vec<String>> {
    let mut errors = vec![];
    if let Err(e) = c.validate() {
        errors.push(e);
    }
    if let Err(e) = c.monetization.validate() {
        errors.push(e);
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

pub fn is_empty(v: &Value) -> bool {
    v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(|a| a.is_empty())
}

fn decode(def: &SettingDef, row: &StoredSetting, keys: Option<&KeyRing>) -> Result<Value, String> {
    match (&row.value, def.secret) {
        (Stored::Json(v), false) => Ok(v.clone()),
        (Stored::Encrypted(blob), true) => {
            let keys = keys
                .ok_or_else(|| format!("{} is not set, so this secret cannot be decrypted", crypto::KEY_ENV))?;
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

/// Boot overlay: `base` (file + env) plus every editable stored value.
pub fn apply_overlay(base: &Config, rows: &[StoredSetting], keys: Option<&KeyRing>) -> Overlay {
    let mut candidate = base.clone();
    let mut out = Overlay {
        config: base.clone(),
        loaded_rev: rows.iter().map(|r| r.rev).max().unwrap_or(0),
        from_db: BTreeSet::new(),
        problems: vec![],
        secret_problems: vec![],
        ignored: vec![],
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
        && let Err(errors) = validate_config(&candidate)
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

/// Re-apply the stored values of Live settings onto the running config; Restart-class
/// values stay as loaded. A secret that fails to decrypt keeps its running value. Any
/// other bad row, or an invalid result, rejects the whole reload.
pub fn reload_live(current: &Config, rows: &[StoredSetting], keys: Option<&KeyRing>) -> Result<Config, Vec<Problem>> {
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
        && let Err(errors) = validate_config(&candidate)
    {
        problems.extend(errors.into_iter().map(|reason| Problem { key: "*".into(), reason }));
    }
    if problems.is_empty() { Ok(candidate) } else { Err(problems) }
}

/// Restart-class settings saved after this instance loaded its config (spec §5.6).
pub fn pending_restart(rows: &[StoredSetting], loaded_rev: i64) -> Vec<&'static str> {
    rows.iter()
        .filter(|r| r.rev > loaded_rev)
        .filter_map(|r| find(&r.key))
        .filter(|d| d.class == ApplyClass::Restart)
        .map(|d| d.key)
        .collect()
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
        let ov = apply_overlay(&base, &[], None);
        assert!(same(&ov.config, &base));
        assert_eq!((ov.loaded_rev, ov.safe_mode_reason()), (0, None));
    }

    #[test]
    fn live_and_restart_rows_win_over_the_base() {
        let ov = apply_overlay(
            &Config::default(),
            &[row("server.cors_origins", json!(["https://a.example"]), 3), row("server.drain_seconds", json!(45), 5)],
            None,
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
            &[row("server.cors_origins", json!("not-a-list"), 7), row("recording.retention_days", json!(5), 8)],
            None,
        );
        assert!(same(&ov.config, &base), "safe mode runs file + env only");
        assert_eq!(ov.problems[0].key, "server.cors_origins");
        assert!(ov.from_db.is_empty());
        assert_eq!(ov.loaded_rev, 8, "loaded_rev still covers every row (restart-loop guard)");
        assert!(ov.safe_mode_reason().unwrap().contains("server.cors_origins"));
    }

    #[test]
    fn an_out_of_range_row_means_safe_mode() {
        let ov = apply_overlay(&Config::default(), &[row("turn.ttl_secs", json!(5), 1)], None);
        assert_eq!(ov.problems[0].key, "turn.ttl_secs");
    }

    #[test]
    fn an_invalid_combination_means_safe_mode() {
        let mut base = Config::default();
        base.monetization.enabled = true;
        base.monetization.postgres_url = "postgres://x".into();
        base.monetization.stripe_secret_key = "sk_test_x".into();
        base.monetization.webhook_signing_secret = "whsec_x".into();
        assert!(validate_config(&base).is_ok(), "fixture must start valid");
        let ov = apply_overlay(
            &base,
            &[row("monetization.min_donation_cents", json!(5000), 1), row("monetization.max_donation_cents", json!(1000), 2)],
            None,
        );
        assert_eq!(ov.problems[0].key, "*");
        assert_eq!(ov.config.monetization.min_donation_cents, base.monetization.min_donation_cents);
    }

    #[test]
    fn secrets_decrypt_with_the_key() {
        let r = ring(K1);
        let ov = apply_overlay(&Config::default(), &[secret_row(&r, "storage.s3.secret_key", json!("s3-secret"), 1)], Some(&r));
        assert_eq!(ov.config.storage.s3.secret_key, "s3-secret");
        assert!(ov.secret_problems.is_empty());
    }

    #[test]
    fn without_the_key_secrets_keep_their_env_value_and_nothing_else_breaks() {
        let mut base = Config::default();
        base.storage.s3.secret_key = "from-env".into();
        let rows = [secret_row(&ring(K1), "storage.s3.secret_key", json!("stored"), 1), row("recording.retention_days", json!(5), 2)];
        let ov = apply_overlay(&base, &rows, None);
        assert_eq!(ov.config.storage.s3.secret_key, "from-env");
        assert_eq!(ov.config.recording.retention_days, 5);
        assert_eq!(ov.secret_problems[0].key, "storage.s3.secret_key");
        assert_eq!(ov.safe_mode_reason(), None);
    }

    #[test]
    fn a_wrong_key_is_a_secret_problem_not_safe_mode() {
        let rows = [secret_row(&ring(K1), "storage.s3.secret_key", json!("stored"), 1)];
        let ov = apply_overlay(&Config::default(), &rows, Some(&ring(K2)));
        assert_eq!(ov.secret_problems.len(), 1);
        assert_eq!(ov.safe_mode_reason(), None);
    }

    #[test]
    fn a_secret_copied_onto_another_setting_does_not_decrypt() {
        let r = ring(K1);
        let mut moved = secret_row(&r, "storage.s3.access_key", json!("x"), 1);
        moved.key = "storage.s3.secret_key".into();
        let ov = apply_overlay(&Config::default(), &[moved], Some(&r));
        assert_eq!(ov.secret_problems[0].key, "storage.s3.secret_key");
        assert_eq!(ov.config.storage.s3.secret_key, "");
    }

    #[test]
    fn a_secret_stored_in_plain_json_is_rejected() {
        let ov = apply_overlay(&Config::default(), &[row("storage.s3.secret_key", json!("plain"), 1)], Some(&ring(K1)));
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
        let ov = apply_overlay(&base, &rows, Some(&r));
        assert_eq!(ov.config.monetization.lnbits_url, "http://lnbits:5000", "env destination kept");
        assert_eq!(ov.config.monetization.lnbits_invoice_key, "env-key");
        assert!(ov.secret_problems.iter().any(|p| p.key == "monetization.lnbits_url"));
        assert_eq!(ov.safe_mode_reason(), None);

        // Both from the database: the move applies.
        let rows = [
            row("monetization.lnbits_url", json!("https://elsewhere.example"), 1),
            secret_row(&r, "monetization.lnbits_invoice_key", json!("db-key"), 2),
        ];
        let ov = apply_overlay(&base, &rows, Some(&r));
        assert_eq!(ov.config.monetization.lnbits_url, "https://elsewhere.example");

        // Same value as file/env (the normal no-key import): nothing to report.
        let rows = [row("monetization.lnbits_url", json!("http://lnbits:5000"), 1)];
        let ov = apply_overlay(&base, &rows, None);
        assert!(ov.secret_problems.is_empty());
    }

    #[test]
    fn import_plan_covers_editable_settings_only() {
        let mut base = Config::default();
        base.storage.s3.secret_key = "s3-secret".into();
        let without = import_plan(&base, None);
        let keys: Vec<_> = without.values.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"server.cors_origins"));
        assert!(!keys.contains(&"storage.s3.secret_key"), "no key ⇒ secrets stay env-sourced");
        assert!(!keys.contains(&"jwt_signing_key") && !keys.contains(&"matrix.as_token"));

        let r = ring(K1);
        let with = import_plan(&base, Some(&r));
        let (_, v) = with.values.iter().find(|(k, _)| k == "storage.s3.secret_key").unwrap();
        let Stored::Encrypted(blob) = v else { panic!("secret must be encrypted") };
        assert_eq!(r.decrypt("storage.s3.secret_key", blob).unwrap(), br#""s3-secret""#);
        assert!(!with.values.iter().any(|(k, _)| k == "storage.s3.access_key"), "empty secrets are skipped");
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
    fn pending_restart_lists_restart_rows_newer_than_loaded() {
        let rows = [
            row("server.drain_seconds", json!(10), 4),
            row("monetization.enabled", json!(true), 9),
            row("server.cors_origins", json!([]), 10),
        ];
        assert_eq!(pending_restart(&rows, 5), vec!["monetization.enabled"]);
        assert!(pending_restart(&rows, 10).is_empty());
    }

    #[test]
    fn reload_live_applies_live_rows_only() {
        let mut current = Config::default();
        current.server.drain_seconds = 30;
        let rows = [row("server.cors_origins", json!(["https://b.example"]), 2), row("server.drain_seconds", json!(99), 3)];
        let next = reload_live(&current, &rows, None).unwrap();
        assert_eq!(next.server.cors_origins, vec!["https://b.example"]);
        assert_eq!(next.server.drain_seconds, 30, "restart-class waits");
    }

    #[test]
    fn reload_live_rejects_a_bad_live_row_and_changes_nothing() {
        let err = reload_live(&Config::default(), &[row("turn.ttl_secs", json!(1), 1)], None).unwrap_err();
        assert_eq!(err[0].key, "turn.ttl_secs");
    }

    #[test]
    fn reload_live_keeps_a_secret_it_cannot_decrypt() {
        let mut current = Config::default();
        current.server.request_webhook_url = Some("https://running.example/hook".into());
        let rows = [secret_row(&ring(K1), "server.request_webhook_url", json!("https://new.example/hook"), 1)];
        let next = reload_live(&current, &rows, Some(&ring(K2))).unwrap();
        assert_eq!(next.server.request_webhook_url.as_deref(), Some("https://running.example/hook"));
    }

    #[test]
    fn shadowed_env_reports_plain_and_from_file_variants() {
        let from_db: BTreeSet<String> = ["server.cors_origins", "turn.shared_secret", "video.max_bitrate"].map(String::from).into();
        let set = |v: &str| v == "MM_CORS_ORIGINS" || v == "MM_VIDEO_MAX_BITRATE_FROM_FILE" || v == "MM_E2EE_ENABLED";
        assert_eq!(shadowed_env(&from_db, set), vec!["MM_CORS_ORIGINS", "MM_VIDEO_MAX_BITRATE"]);
    }
}
