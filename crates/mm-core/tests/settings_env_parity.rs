//! Every env var the registry names must actually move its field in
//! `apply_env_overrides`. ONE test in this binary on purpose: it mutates the
//! process environment, which is only sound with no concurrent readers.

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

#[test]
fn every_env_var_named_by_the_registry_is_honoured() {
    let base = Config::default();
    let mut ignored = vec![];
    for def in registry() {
        let Some(var) = def.env else { continue };
        let default = (def.get)(&base);
        let value = sample(def.kind, &default);
        // SAFETY: the only test in this binary; nothing else reads the environment.
        unsafe { std::env::set_var(var, &value) };
        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        unsafe { std::env::remove_var(var) };
        if (def.get)(&cfg) == default {
            ignored.push(format!("{var} -> {} (sample {value:?})", def.key));
        }
    }
    assert!(
        ignored.is_empty(),
        "the registry names env vars that apply_env_overrides ignores: {ignored:#?}"
    );
}
