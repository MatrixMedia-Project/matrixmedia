//! Every env var the registry names must actually move its field in
//! `apply_env_overrides`, to the value the registry's own `env:` claims it reads —
//! not just "some field moved". ONE test in this binary on purpose: it mutates the
//! process environment, which is only sound with no concurrent readers.
//!
//! The check starts by scrubbing every `MM_*` variable from the process
//! environment. Without that, this test would be at the mercy of whatever the
//! ambient shell or CI exports (`.github/workflows/test.yml` and `just test-db`
//! both export `MM_DATABASE_URL`; an operator's shell may export others) — a
//! misspelled `env:` on one entry could still pass because the ambient value
//! (not the registry's sample) happened to move the field, and an ambient
//! `<VAR>_FROM_FILE` pointing at a missing file could fail this test for a
//! reason that has nothing to do with the registry/`apply_env_overrides` link.

use mm_core::config::Config;
use mm_core::settings::{ValueKind, registry};
use serde_json::Value;

/// An env-var spelling of a value that differs from `default`.
fn sample(kind: ValueKind, default: &Value) -> String {
    match kind {
        ValueKind::Bool => if default.as_bool() == Some(true) { "false".into() } else { "true".into() },
        ValueKind::Int { min, max } => {
            let d = default.as_i64().unwrap_or(min);
            (if d < max { d + 1 } else { d - 1 }).to_string()
        }
        ValueKind::Float { min, max } => {
            let d = default.as_f64().unwrap_or(min);
            (if d + 0.01 <= max { d + 0.01 } else { d - 0.01 }).to_string()
        }
        ValueKind::Url | ValueKind::OptUrl => "https://parity.example.org".into(),
        ValueKind::List => "https://a.example.org,https://b.example.org".into(),
        ValueKind::Choice { options } => {
            options.iter().find(|o| default.as_str() != Some(**o)).expect("a second option").to_string()
        }
        ValueKind::Text | ValueKind::OptText => "parity-sample-value".into(),
    }
}

/// The JSON value `apply_env_overrides` should produce from `sample`'s string, by
/// kind. Independent of `sample`'s own wording, so a field that moved to the
/// *wrong* value (an off-by-one parse, a `List` that didn't split, a bool parsed
/// inverted, ...) is caught, not just a field that failed to move at all.
fn expected(kind: ValueKind, value: &str) -> Value {
    match kind {
        ValueKind::Bool => Value::Bool(value.parse::<bool>().expect("sample bool parses")),
        ValueKind::Int { .. } => Value::from(value.parse::<i64>().expect("sample int parses")),
        ValueKind::Float { .. } => Value::from(value.parse::<f64>().expect("sample float parses")),
        ValueKind::Text
        | ValueKind::OptText
        | ValueKind::Url
        | ValueKind::OptUrl
        | ValueKind::Choice { .. } => Value::String(value.to_string()),
        ValueKind::List => Value::Array(
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
    }
}

/// Remove every `MM_*` variable — and so every `<VAR>_FROM_FILE` companion too —
/// from the process environment.
fn clear_mm_env() {
    let names: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_str().is_some_and(|s| s.starts_with("MM_")))
        .collect();
    for name in names {
        // SAFETY: changing the environment is unsound only while another thread reads
        // or writes it. This binary holds exactly one test and starts no thread, so every
        // environment access in it (this loop, the set/remove calls in the test, and
        // `apply_env_overrides`) happens one after another on the test's own thread.
        unsafe { std::env::remove_var(&name) };
    }
}

#[test]
fn every_env_var_named_by_the_registry_is_honoured() {
    clear_mm_env();

    // Non-vacuous: a registry that named few (or no) env vars would make every
    // check below pass trivially without covering anything. There are ~60 today.
    let named = registry().iter().filter(|d| d.env.is_some()).count();
    assert!(named >= 50, "expected at least 50 registry entries with an env var, found {named}");

    // With no `MM_*` variable set at all (not even an ambient one), applying env
    // overrides must be a no-op: every registry field must read back exactly the
    // `Config` default. This is the baseline the per-variable checks below build
    // on — if it doesn't hold, a "field moved" finding downstream could just be
    // stale process state, not `apply_env_overrides` reacting to the sample.
    let base = Config::default();
    let mut probe = Config::default();
    probe.apply_env_overrides();
    let baseline_mismatches: Vec<&str> = registry()
        .iter()
        .filter(|def| (def.get)(&probe) != (def.get)(&base))
        .map(|def| def.key)
        .collect();
    assert!(
        baseline_mismatches.is_empty(),
        "apply_env_overrides() changed these fields with no MM_* variable set at all \
         (an ambient variable escaped clear_mm_env, or apply_env_overrides has a bug): \
         {baseline_mismatches:#?}"
    );

    let mut ignored = vec![];
    let mut wrong = vec![];
    for def in registry() {
        let Some(var) = def.env else { continue };
        let default = (def.get)(&base);
        let value = sample(def.kind, &default);
        let want = expected(def.kind, &value);
        // SAFETY: no other thread touches the environment (see `clear_mm_env`).
        unsafe { std::env::set_var(var, &value) };
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        // SAFETY: no other thread touches the environment (see `clear_mm_env`).
        unsafe { std::env::remove_var(var) };
        let got = (def.get)(&cfg);
        if got == default {
            ignored.push(format!("{var} -> {} (sample {value:?})", def.key));
        } else if got != want {
            wrong.push(format!(
                "{var} -> {}: sample {value:?} produced {got}, expected {want}",
                def.key
            ));
        }
    }
    assert!(
        ignored.is_empty(),
        "the registry names env vars that apply_env_overrides ignores: {ignored:#?}"
    );
    assert!(
        wrong.is_empty(),
        "apply_env_overrides moved these fields to the wrong value: {wrong:#?}"
    );
}
