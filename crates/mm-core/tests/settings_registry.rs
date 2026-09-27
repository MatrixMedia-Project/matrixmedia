//! Registry completeness guards. These are what make "adding a setting to mm-core
//! makes it appear in the dashboard, and CI fails if a setting is unclassified" true.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mm_core::config::Config;
use mm_core::settings::{ENV_ONLY, EXCLUDED, ValueKind, registry};
use serde_json::Value;

/// Every leaf field of `Config` as a dotted path, parsed from config.rs.
///
/// Parsing the source (not serialising `Config`) is deliberate: the 15+ secret fields
/// are `#[serde(skip_serializing)]`, so a JSON walk alone never sees them.
fn config_fields_from_source() -> BTreeSet<String> {
    let src = include_str!("../src/config.rs");
    let mut structs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("pub struct ").and_then(|r| r.strip_suffix(" {")) {
            current = Some(name.trim().to_string());
            structs.entry(name.trim().to_string()).or_default();
            continue;
        }
        if current.is_some() && t == "}" {
            current = None;
            continue;
        }
        if let (Some(name), Some(rest)) = (&current, t.strip_prefix("pub ")) {
            if let Some((field, ty)) = rest.split_once(':') {
                let field = field.trim();
                if field.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    let ty = ty.trim().trim_end_matches(',').trim().to_string();
                    structs.get_mut(name).unwrap().push((field.to_string(), ty));
                }
            }
        }
    }
    fn walk(
        structs: &BTreeMap<String, Vec<(String, String)>>,
        name: &str,
        prefix: &str,
        out: &mut BTreeSet<String>,
    ) {
        for (field, ty) in &structs[name] {
            let path = if prefix.is_empty() { field.clone() } else { format!("{prefix}.{field}") };
            if structs.contains_key(ty.as_str()) {
                walk(structs, ty, &path, out);
            } else {
                out.insert(path);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&structs, "Config", "", &mut out);
    out
}

fn json_leaves(v: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                json_leaves(v, &p, out);
            }
        }
        _ => {
            out.insert(prefix.to_string());
        }
    }
}

#[test]
fn every_config_field_is_registered_or_excluded() {
    let fields = config_fields_from_source();
    let registered: BTreeSet<&str> = registry().iter().map(|d| d.key).collect();
    let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|e| e.key).collect();

    let unclassified: Vec<_> = fields
        .iter()
        .filter(|f| !registered.contains(f.as_str()) && !excluded.contains(f.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "Config fields with no settings-registry entry and no exclusion: {unclassified:?}\n\
         Add a `setting!` line (or an EXCLUDED entry with the reason) in \
         crates/mm-core/src/settings/entries.rs."
    );

    let stale: Vec<_> = registered
        .iter()
        .chain(excluded.iter())
        .filter(|k| !fields.contains(**k))
        .collect();
    assert!(stale.is_empty(), "registry/exclusion keys that are not Config fields: {stale:?}");

    let both: Vec<_> = registered.intersection(&excluded).collect();
    assert!(both.is_empty(), "keys both registered and excluded: {both:?}");
}

#[test]
fn the_source_parser_sees_every_serialized_field_and_the_secrets() {
    let from_source = config_fields_from_source();
    let mut from_json = BTreeSet::new();
    json_leaves(&serde_json::to_value(Config::default()).unwrap(), "", &mut from_json);
    let missed: Vec<_> = from_json.difference(&from_source).collect();
    assert!(missed.is_empty(), "the config.rs parser missed serialised fields: {missed:?}");
    assert!(
        from_source.contains("jwt_signing_key") && !from_json.contains("jwt_signing_key"),
        "the parser must see skip_serializing secrets that the JSON walk cannot"
    );
}

#[test]
fn every_entry_round_trips_its_default() {
    for d in registry() {
        let v = (d.get)(&Config::default());
        let mut c = Config::default();
        (d.set)(&mut c, v.clone()).unwrap_or_else(|e| panic!("{}: {e}", d.key));
        assert_eq!((d.get)(&c), v, "{}", d.key);
    }
}

/// Import writes the effective value of every editable setting. If a DEFAULT failed
/// validation, a fresh install would import a row the overlay then rejects — and boot
/// straight into safe mode.
#[test]
fn every_editable_default_is_valid() {
    for d in registry().iter().filter(|d| d.editable()) {
        let v = (d.get)(&Config::default());
        assert!(d.validate(&v).is_ok(), "{} default {v} fails its own validation", d.key);
    }
}

/// Every registered secret must stay out of `serde_json::to_value(&Config)` —
/// /platform/config-full serialises the whole config.
#[test]
fn every_secret_is_skip_serialized() {
    const S: &str = "mm-test-secret-7f3a";
    for d in registry().iter().filter(|d| d.secret) {
        let mut c = Config::default();
        (d.set)(&mut c, Value::String(format!("https://x.example/{S}"))).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains(S), "{} is secret but serialises", d.key);
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// A new `env::var("MM_…")` outside config.rs is a setting the dashboard can't see —
/// or, if it names a registered setting, a read that bypasses `Config` (and so
/// ignores whatever the dashboard has stored). Either way it must fail unless the
/// var is on `ENV_ONLY`: registered settings must be read through `Config`, never
/// by a direct `env::var` call elsewhere.
#[test]
fn every_direct_env_read_is_env_only() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let env_only: BTreeSet<&str> = ENV_ONLY.iter().map(|e| e.var).collect();
    let mut offenders = BTreeSet::new();
    for krate in std::fs::read_dir(crates).unwrap().flatten() {
        if krate.file_name() == "mm-fakestripe" {
            continue; // standalone test double, not part of mm-core's runtime
        }
        let src = krate.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = vec![];
        rust_files(&src, &mut files);
        for f in files {
            if f.ends_with("test_support.rs") || f.ends_with("mm-core/src/config.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap();
            for pat in ["env::var(\"MM_", "env::var_os(\"MM_"] {
                for (i, _) in text.match_indices(pat) {
                    let rest = &text[i + pat.len() - 3..];
                    let var = &rest[..rest.find('"').unwrap()];
                    if !env_only.contains(var) {
                        offenders.insert(format!("{}: {var}", f.strip_prefix(crates).unwrap().display()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "direct MM_* env reads outside config.rs that are not on ENV_ONLY: {offenders:#?}\n\
         A registered setting must be read through Config, not env::var directly (a direct \
         read ignores the dashboard). Move the value into Config (preferred) or add it to \
         ENV_ONLY with the reason it must stay in .env."
    );
}

/// Every registry entry's `set` must actually change the stored value, not silently
/// no-op or coerce back to the default — the dashboard's save path relies on `set`
/// to make the write real.
#[test]
fn set_writes_a_distinct_value() {
    for d in registry() {
        let default_v = (d.get)(&Config::default());
        let sample = match d.kind {
            ValueKind::Bool => {
                let b = default_v.as_bool().expect("Bool default is a bool");
                Value::Bool(!b)
            }
            ValueKind::Int { min, max } => {
                let n = default_v.as_i64().expect("Int default is an int");
                Value::from(if n != min { min } else { max })
            }
            ValueKind::Float { min, max } => {
                let n = default_v.as_f64().expect("Float default is a float");
                Value::from(if n != min { min } else { max })
            }
            ValueKind::Text | ValueKind::OptText => Value::String("sample-x".to_string()),
            ValueKind::Url | ValueKind::OptUrl => Value::String("https://sample.example".to_string()),
            ValueKind::List => Value::Array(vec![Value::String("sample-x".to_string())]),
            ValueKind::Choice { options } => {
                let current = default_v.as_str().unwrap_or_default();
                let alt = options.iter().find(|o| **o != current).unwrap_or(&options[0]);
                Value::String((*alt).to_string())
            }
        };
        let mut c = Config::default();
        (d.set)(&mut c, sample.clone()).unwrap_or_else(|e| panic!("{}: {e}", d.key));
        let got = (d.get)(&c);
        assert_eq!(got, sample, "{}: get after set did not return the sample value", d.key);
        assert_ne!(got, default_v, "{}: sample value equals the default — test proves nothing", d.key);
    }
}
