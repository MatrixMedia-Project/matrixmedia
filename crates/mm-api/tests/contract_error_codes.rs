//! The error codes the contract advertises must be exactly the ones the server sends.
//!
//! The web SDK's `ErrorCode` handling is generated from the hand-written contract
//! (`contracts/api/mm_api_v1.yaml`), and that contract had drifted from mm-core: it
//! listed codes the server never sends and was missing ones it does. The SDK,
//! meanwhile, mapped a lowercase vocabulary no server produces, so every real error
//! reached web callers as `"unknown"` — and nothing noticed, because each piece was
//! only ever checked against itself.
//!
//! The server's vocabulary has two parts: mm-core's `ErrorCode` enum, and a few codes
//! that individual mm-api routes send as plain strings ([`ROUTE_LOCAL`]). The chain is
//! pinned end to end:
//!
//!   mm-core `ErrorCode` ─(this test, from the type itself)──▶ hand-written contract
//!   route-local codes   ─(this test, by their literals)─────▶ hand-written contract
//!   mm-core `ErrorCode` ─(this test + CI spec-gen)───────────▶ generated spec
//!   hand-written contract ─(CI web-test gen:types drift)────▶ SDK api-types.ts
//!   api-types.ts ─(Record<ServerErrorCode, ErrorCode>, tsc)─▶ SDK mapping (fails to
//!                                                             compile if a code has
//!                                                             no mapping)
//!
//! So a new server error code cannot ship without the SDK deciding what it means.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mm_core::error::ErrorCode;
use utoipa::openapi::{schema::Schema, RefOr};
use utoipa::PartialSchema;

/// Codes that mm-api routes send as plain strings rather than through mm-core's
/// `ErrorCode`. Each must be in the contract, and each must still be sent:
/// [`route_local_codes_are_still_sent`] finds its literal in `crates/mm-api/src`.
const ROUTE_LOCAL: &[&str] = &[
    // admin_settings.rs
    "MM_SETTINGS_READ_ONLY",
    "MM_SETTINGS_NO_KEY",
    "MM_SETTINGS_INVALID",
    "MM_SETTINGS_CONFLICT",
    "MM_SETTINGS_LOCKOUT",
    "MM_SETTINGS_REENTER_SECRETS",
    // ads_media.rs
    "MM_AD_UPLOAD_REJECTED",
    "MM_AD_UPLOAD_TIMEOUT",
];

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn contracts() -> PathBuf {
    crate_dir().join("../../contracts/api")
}

/// The `enum:` list under the `ErrorCode:` schema. Line-based on purpose — both files
/// are plain block YAML, and a YAML dependency for one list is not worth it. Comment
/// lines inside the list are skipped.
fn error_code_enum(yaml: &str) -> Vec<String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == "ErrorCode:")
        .expect("an ErrorCode schema");
    let enum_at = (start..lines.len())
        .find(|&i| lines[i].trim() == "enum:")
        .expect("ErrorCode has an enum");
    lines[enum_at + 1..]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map_while(|l| l.strip_prefix("- ").map(|c| c.trim().to_string()))
        .collect()
}

fn read(name: &str) -> String {
    std::fs::read_to_string(contracts().join(name))
        .unwrap_or_else(|e| panic!("reading {name}: {e}"))
}

/// mm-core's codes, read from the type itself rather than from a generated file, so a
/// new variant is caught by a plain local `cargo test`.
fn mm_core_codes() -> BTreeSet<String> {
    let RefOr::T(Schema::Object(schema)) = <ErrorCode as PartialSchema>::schema() else {
        panic!("mm-core's ErrorCode is no longer a plain string enum in its OpenAPI schema");
    };
    schema
        .enum_values
        .expect("ErrorCode's schema lists its values")
        .into_iter()
        .map(|v| v.as_str().expect("ErrorCode values are strings").to_string())
        .collect()
}

fn diff_message(what: &str, listed: &BTreeSet<String>, sent: &BTreeSet<String>) -> String {
    let never_sent: Vec<_> = listed.difference(sent).collect();
    let undocumented: Vec<_> = sent.difference(listed).collect();
    format!(
        "{what} ErrorCode has drifted from the server.\n\
         Listed but never sent: {never_sent:?}\n\
         Sent but not listed:   {undocumented:?}"
    )
}

/// The schema's values are the wire names: each one deserialises as mm-core's
/// `ErrorCode` and serialises back unchanged. This also proves the schema read above
/// found real codes.
#[test]
fn mm_core_schema_values_are_its_wire_codes() {
    let codes = mm_core_codes();
    assert!(codes.len() > 20, "suspiciously few codes: {codes:?}");
    for code in codes {
        let parsed: ErrorCode = serde_json::from_value(serde_json::Value::String(code.clone()))
            .unwrap_or_else(|_| panic!("{code} is in ErrorCode's schema but serde rejects it"));
        assert_eq!(serde_json::to_value(parsed).unwrap(), code.as_str());
    }
}

#[test]
fn the_contract_lists_exactly_the_codes_the_server_sends() {
    let listed = error_code_enum(&read("mm_api_v1.yaml"));
    let unique: BTreeSet<String> = listed.iter().cloned().collect();
    assert_eq!(listed.len(), unique.len(), "duplicate codes in the contract: {listed:?}");

    let mut sent = mm_core_codes();
    sent.extend(ROUTE_LOCAL.iter().map(|c| c.to_string()));
    assert!(
        unique == sent,
        "{}\nList mm-core's codes (in its order) and then ROUTE_LOCAL in \
         contracts/api/mm_api_v1.yaml, run `npm run gen:types` in web/, and map any \
         new code in web/packages/client/src/types.ts.",
        diff_message("contracts/api/mm_api_v1.yaml", &unique, &sent),
    );
}

/// CI's spec-gen job regenerates this file and fails on drift, but it needs a
/// self-hosted runner; this checks the error codes part of it locally as well.
#[test]
fn the_generated_spec_lists_mm_cores_codes() {
    let generated: BTreeSet<String> =
        error_code_enum(&read("generated/mm_api_gen.yaml")).into_iter().collect();
    let sent = mm_core_codes();
    assert!(
        generated == sent,
        "{}\nRun `cargo run -p mm-api --bin mm-openapi`.",
        diff_message("contracts/api/generated/mm_api_gen.yaml", &generated, &sent),
    );
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A route-local code is listed only while some mm-api route still sends it, and
/// only while it is not an mm-core code (then it belongs to the enum's half).
#[test]
fn route_local_codes_are_still_sent() {
    let mut files = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut files);
    let sources: Vec<String> = files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect();
    let core = mm_core_codes();
    for code in ROUTE_LOCAL {
        assert!(
            !core.contains(*code),
            "{code} is now an mm-core ErrorCode; remove it from ROUTE_LOCAL"
        );
        let literal = format!("\"{code}\"");
        assert!(
            sources.iter().any(|s| s.contains(&literal)),
            "{code} is in ROUTE_LOCAL but no mm-api source sends it; remove it from \
             ROUTE_LOCAL and from contracts/api/mm_api_v1.yaml"
        );
    }
}
