//! REST compatibility check (ADR-0011 section 5.2, check 2).
//!
//! CI's `spec-gen` job only proves that the generated OpenAPI file matches the
//! code. That catches drift, not breakage. This test compares both REST
//! contracts with a committed baseline of what the shipped apps were built
//! against and fails when a change would break one of them:
//!
//! - a route (method + path) is gone;
//! - a parameter, a request field or a response field is gone;
//! - a parameter or field changed its type;
//! - a request gained a required parameter or a required field.
//!
//! Additive changes (new routes, new optional request fields, new response
//! fields) pass.
//!
//! The contracts are `contracts/api/mm_api_v1.yaml` (hand-written; the web
//! client's types are generated from it) and
//! `contracts/api/generated/mm_api_gen.yaml` (generated from the code; CI keeps
//! it fresh). The baseline is `contracts/api/baselines/2026-11-25-development.json`,
//! a flattened "surface" of both files taken from `development` on that day.
//!
//! Updating the baseline is a reviewed decision that an app version is no
//! longer supported. To write a new one:
//!
//! ```text
//! MM_WRITE_REST_BASELINE=contracts/api/baselines/<date>-<label>.json \
//!   cargo test -p mm-api --test rest_contract_compat
//! ```
//! and point [`BASELINE`] at it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

const BASELINE: &str = "contracts/api/baselines/2026-11-25-development.json";
const CONTRACTS: &[&str] = &[
    "contracts/api/mm_api_v1.yaml",
    "contracts/api/generated/mm_api_gen.yaml",
];

/// How deep `$ref`s are followed. Deeper nesting is cut, which only makes the
/// check less strict for recursive schemas; none of today's schemas get close.
const MAX_DEPTH: usize = 12;

type Surface = BTreeMap<String, String>;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load_yaml(rel: &str) -> Value {
    // MM_REST_CONTRACTS_ROOT: read the contracts from another tree (used to
    // write the baseline from the development commit it describes).
    let root = std::env::var("MM_REST_CONTRACTS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root());
    let path = root.join(rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn resolve<'a>(root: &'a Value, mut v: &'a Value) -> &'a Value {
    for _ in 0..MAX_DEPTH {
        let Some(r) = v.get("$ref").and_then(Value::as_str) else {
            return v;
        };
        let Some(ptr) = r.strip_prefix('#') else {
            return v;
        };
        match root.pointer(ptr) {
            Some(target) => v = target,
            None => panic!("unresolvable $ref {r}"),
        }
    }
    v
}

/// `type=… required=…[ enum=<JSON array>]` — `enum` last, so it may hold spaces.
fn describe(s: &Value, required: bool) -> String {
    let mut d = format!("type={} required={required}", type_of(s));
    let values = match (s.get("enum"), s.get("const")) {
        (Some(Value::Array(a)), _) => Some(a.clone()),
        (_, Some(c)) => Some(vec![c.clone()]),
        _ => None,
    };
    if let Some(mut v) = values {
        v.sort_by_key(|x| x.to_string());
        d.push_str(&format!(" enum={}", Value::Array(v)));
    }
    d
}

fn enum_part(desc: &str) -> Option<Vec<Value>> {
    let (_, e) = desc.split_once(" enum=")?;
    serde_json::from_str::<Vec<Value>>(e).ok()
}

fn type_of(s: &Value) -> String {
    let mut types: Vec<String> = match s.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect(),
        _ if s.get("properties").is_some() => vec!["object".into()],
        _ if s.get("items").is_some() => vec!["array".into()],
        _ if s.get("oneOf").is_some() => vec!["oneOf".into()],
        _ if s.get("anyOf").is_some() => vec!["anyOf".into()],
        _ if s.get("allOf").is_some() => vec!["object".into()],
        _ => vec!["any".into()],
    };
    if s.get("nullable").and_then(Value::as_bool) == Some(true) {
        types.push("null".into());
    }
    types.sort();
    types.dedup();
    types.join("|")
}

/// Flatten `schema` into `out`: one entry per field path, `type=… required=…`.
fn walk(
    root: &Value,
    schema: &Value,
    key: &str,
    required: bool,
    request: bool,
    out: &mut Surface,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let s = resolve(root, schema);
    let _ = request;
    out.insert(key.to_string(), describe(s, required));
    walk_children(root, s, key, request, out, depth);
}

fn walk_children(
    root: &Value,
    s: &Value,
    key: &str,
    request: bool,
    out: &mut Surface,
    depth: usize,
) {
    let required: Vec<&str> = s
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if let Some(props) = s.get("properties").and_then(Value::as_object) {
        for (name, sub) in props {
            walk(
                root,
                sub,
                &format!("{key}.{name}"),
                required.contains(&name.as_str()),
                request,
                out,
                depth + 1,
            );
        }
    }
    if let Some(items) = s.get("items") {
        walk(
            root,
            items,
            &format!("{key}[]"),
            true,
            request,
            out,
            depth + 1,
        );
    }
    if let Some(ap) = s.get("additionalProperties").filter(|v| v.is_object()) {
        walk(
            root,
            ap,
            &format!("{key}{{}}"),
            false,
            request,
            out,
            depth + 1,
        );
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        // allOf merges: its branches' fields belong to this object.
        for branch in all {
            walk_children(root, resolve(root, branch), key, request, out, depth + 1);
        }
    }
    for k in ["oneOf", "anyOf"] {
        if let Some(branches) = s.get(k).and_then(Value::as_array) {
            for (i, b) in branches.iter().enumerate() {
                walk(
                    root,
                    b,
                    &format!("{key}|{i}"),
                    true,
                    request,
                    out,
                    depth + 1,
                );
            }
        }
    }
}

const METHODS: &[&str] = &["get", "put", "post", "delete", "patch", "head", "options"];

/// The breaking-change-relevant surface of one OpenAPI document.
fn surface(doc: &Value) -> Surface {
    let mut out = Surface::new();
    let Some(paths) = doc.get("paths").and_then(Value::as_object) else {
        return out;
    };
    for (path, item) in paths {
        let shared_params: Vec<Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let op_id = format!("{} {path}", method.to_uppercase());
            out.insert(format!("op {op_id}"), "present".into());

            let mut params = shared_params.clone();
            params.extend(
                op.get("parameters")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            for p in &params {
                let p = resolve(doc, p);
                let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
                let loc = p.get("in").and_then(Value::as_str).unwrap_or("?");
                let required = p
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(loc == "path");
                let schema = p.get("schema").cloned().unwrap_or(Value::Null);
                out.insert(
                    format!("param {op_id} {loc}:{name}"),
                    describe(resolve(doc, &schema), required),
                );
            }

            if let Some(body) = op.get("requestBody").map(|b| resolve(doc, b)) {
                let body_required = body
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                for (ctype, media) in body
                    .get("content")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    if let Some(schema) = media.get("schema") {
                        walk(
                            doc,
                            schema,
                            &format!("request {op_id} {ctype} body"),
                            body_required,
                            true,
                            &mut out,
                            0,
                        );
                    }
                }
            }

            for (status, resp) in op
                .get("responses")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let resp = resolve(doc, resp);
                out.insert(format!("response {op_id} {status}"), "present".into());
                for (ctype, media) in resp
                    .get("content")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    if let Some(schema) = media.get("schema") {
                        walk(
                            doc,
                            schema,
                            &format!("response {op_id} {status} {ctype} body"),
                            true,
                            false,
                            &mut out,
                            0,
                        );
                    }
                }
            }
        }
    }
    out
}

/// An enum change that breaks a shipped client: a request (or parameter)
/// loses an accepted value or gains a restriction; a response may now carry a
/// value the client was never built for.
fn enum_break(key: &str, old: &str, new: &str) -> Option<String> {
    let (o, n) = (enum_part(old), enum_part(new));
    let request = key.starts_with("param ") || key.starts_with("request ");
    match (o, n) {
        (None, None) => None,
        (Some(o), Some(n)) => {
            let lost: Vec<&Value> = o.iter().filter(|v| !n.contains(v)).collect();
            let gained: Vec<&Value> = n.iter().filter(|v| !o.contains(v)).collect();
            if request && !lost.is_empty() {
                Some(format!("request enum lost values {lost:?}: {key}"))
            } else if !request && !gained.is_empty() {
                Some(format!("response enum gained values {gained:?}: {key}"))
            } else {
                None
            }
        }
        (None, Some(_)) if request => {
            Some(format!("request enum added (values restricted): {key}"))
        }
        (Some(_), None) if !request => {
            Some(format!("response enum removed (any value now): {key}"))
        }
        _ => None,
    }
}

fn is_required(desc: &str) -> bool {
    desc.contains("required=true")
}

fn type_part(desc: &str) -> &str {
    desc.split_whitespace()
        .find(|p| p.starts_with("type="))
        .unwrap_or(desc)
}

/// The parent field path of a request-field key, if it has one.
fn parent_key(key: &str) -> Option<&str> {
    let (prefix, last) = key.rsplit_once(' ')?;
    let cut = last.rfind(['.', '[', '|', '{'])?;
    Some(&key[..prefix.len() + 1 + cut])
}

/// Every breaking difference between `baseline` and `current`.
fn breaking_changes(baseline: &Surface, current: &Surface) -> Vec<String> {
    let mut problems = Vec::new();
    for (key, old) in baseline {
        match current.get(key) {
            None if key.starts_with("op ") => {
                problems.push(format!("route removed: {}", &key[3..]))
            }
            None if key.starts_with("response ") && old == "present" => {
                problems.push(format!("response removed: {}", &key[9..]))
            }
            None => problems.push(format!("field removed: {key}")),
            Some(new) if type_part(new) != type_part(old) => problems.push(format!(
                "type changed: {key}: {} -> {}",
                type_part(old),
                type_part(new)
            )),
            Some(new)
                if (key.starts_with("param ") || key.starts_with("request "))
                    && is_required(new)
                    && !is_required(old) =>
            {
                problems.push(format!("request field became required: {key}"))
            }
            Some(new) if key.starts_with("response ") && is_required(old) && !is_required(new) => {
                problems.push(format!("response field became optional: {key}"))
            }
            Some(new) => {
                if let Some(p) = enum_break(key, old, new) {
                    problems.push(p);
                }
            }
        }
    }
    for (key, new) in current {
        if baseline.contains_key(key) || !is_required(new) {
            continue;
        }
        let in_existing_op = |k: &str| {
            let op = k.split_whitespace().take(3).collect::<Vec<_>>();
            op.len() == 3 && baseline.contains_key(&format!("op {} {}", op[1], op[2]))
        };
        if key.starts_with("param ") && in_existing_op(key) {
            problems.push(format!("new required parameter: {key}"));
        }
        if key.starts_with("request ") && in_existing_op(key) {
            // A required field inside a new optional object is not a new demand on old
            // clients; one whose parent existed (or is the body itself) is.
            let parent_existed = parent_key(key).is_none_or(|p| baseline.contains_key(p));
            let is_body = key.ends_with(" body");
            if parent_existed || is_body {
                problems.push(format!("new required request field: {key}"));
            }
        }
    }
    problems
}

fn current_surfaces() -> BTreeMap<String, Surface> {
    CONTRACTS
        .iter()
        .map(|c| (c.to_string(), surface(&load_yaml(c))))
        .collect()
}

#[test]
fn the_rest_contracts_keep_everything_the_baseline_promised() {
    let current = current_surfaces();

    if let Ok(target) = std::env::var("MM_WRITE_REST_BASELINE") {
        let path = repo_root().join(&target);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = serde_json::to_string_pretty(&current).unwrap() + "\n";
        std::fs::write(&path, text).unwrap();
        eprintln!("wrote REST baseline {}", path.display());
        return;
    }

    let path = repo_root().join(BASELINE);
    let baseline: BTreeMap<String, Surface> = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
    )
    .expect("baseline parses");

    let mut problems = Vec::new();
    for (contract, base) in &baseline {
        let cur = current
            .get(contract)
            .unwrap_or_else(|| panic!("baseline names {contract}, which is not checked any more"));
        for p in breaking_changes(base, cur) {
            problems.push(format!("{contract}: {p}"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} breaking change(s) against {BASELINE}:\n  {}",
        problems.len(),
        problems.join("\n  ")
    );
}

#[test]
fn the_surface_covers_every_route_of_both_contracts() {
    for c in CONTRACTS {
        let doc = load_yaml(c);
        let routes = doc["paths"]
            .as_object()
            .unwrap()
            .values()
            .map(|item| METHODS.iter().filter(|m| item.get(**m).is_some()).count())
            .sum::<usize>();
        let s = surface(&doc);
        assert_eq!(
            s.keys().filter(|k| k.starts_with("op ")).count(),
            routes,
            "{c}"
        );
        assert!(routes > 30, "{c} has only {routes} routes");
    }
}

/// One failing case per rule, so a comparator that stopped detecting a rule
/// fails here.
#[test]
fn the_comparator_catches_each_kind_of_break() {
    let base_doc: Value = serde_json::json!({
        "paths": {
            "/streams": {
                "post": {
                    "parameters": [ { "name": "dry", "in": "query", "required": false, "schema": { "type": "boolean" } } ],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Req" } } } },
                    "responses": { "200": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Resp" } } } } }
                }
            },
            "/old": { "get": { "responses": { "200": {} } } }
        },
        "components": { "schemas": {
            "Req": { "type": "object", "required": ["room_id"], "properties": { "room_id": { "type": "string" }, "title": { "type": "string" } } },
            "Resp": { "type": "object", "properties": { "stream_id": { "type": "string" }, "count": { "type": "integer" } } }
        } }
    });
    let base = surface(&base_doc);
    assert!(breaking_changes(&base, &base).is_empty());

    let mutate = |f: &dyn Fn(&mut Value)| {
        let mut d = base_doc.clone();
        f(&mut d);
        breaking_changes(&base, &surface(&d))
    };

    let p = mutate(&|d| {
        d["paths"].as_object_mut().unwrap().remove("/old");
    });
    assert!(
        p.iter().any(|x| x.starts_with("route removed: GET /old")),
        "{p:?}"
    );

    let p = mutate(&|d| {
        d["components"]["schemas"]["Resp"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("stream_id");
    });
    assert!(
        p.iter()
            .any(|x| x.starts_with("field removed") && x.contains("stream_id")),
        "{p:?}"
    );

    let p = mutate(&|d| {
        d["components"]["schemas"]["Resp"]["properties"]["count"]["type"] = "string".into()
    });
    assert!(
        p.iter()
            .any(|x| x.starts_with("type changed") && x.contains("count")),
        "{p:?}"
    );

    let p = mutate(&|d| {
        d["components"]["schemas"]["Req"]["required"] = serde_json::json!(["room_id", "title"])
    });
    assert!(
        p.iter()
            .any(|x| x.starts_with("request field became required") && x.contains("title")),
        "{p:?}"
    );

    let p = mutate(&|d| {
        d["components"]["schemas"]["Req"]["properties"]["media_type"] =
            serde_json::json!({ "type": "string" });
        d["components"]["schemas"]["Req"]["required"] =
            serde_json::json!(["room_id", "media_type"]);
    });
    assert!(
        p.iter()
            .any(|x| x.starts_with("new required request field") && x.contains("media_type")),
        "{p:?}"
    );

    let p = mutate(&|d| d["paths"]["/streams"]["post"]["parameters"][0]["required"] = true.into());
    assert!(
        p.iter()
            .any(|x| x.starts_with("request field became required") && x.contains("query:dry")),
        "{p:?}"
    );

    // A response field that every client could rely on becomes optional.
    let mut with_required = base_doc.clone();
    with_required["components"]["schemas"]["Resp"]["required"] = serde_json::json!(["stream_id"]);
    let base_req = surface(&with_required);
    let p = breaking_changes(&base_req, &surface(&base_doc));
    assert!(
        p.iter()
            .any(|x| x.starts_with("response field became optional") && x.contains("stream_id")),
        "{p:?}"
    );

    // Enums: a response gains a value; a request loses one.
    let mut enums = base_doc.clone();
    enums["components"]["schemas"]["Resp"]["properties"]["status"] =
        serde_json::json!({ "type": "string", "enum": ["active", "ended"] });
    enums["components"]["schemas"]["Req"]["properties"]["media"] =
        serde_json::json!({ "type": "string", "enum": ["audio", "video"] });
    let base_enum = surface(&enums);
    let mut gained = enums.clone();
    gained["components"]["schemas"]["Resp"]["properties"]["status"]["enum"] =
        serde_json::json!(["active", "ended", "paused"]);
    let p = breaking_changes(&base_enum, &surface(&gained));
    assert!(
        p.iter()
            .any(|x| x.starts_with("response enum gained values")),
        "{p:?}"
    );
    let mut lost = enums.clone();
    lost["components"]["schemas"]["Req"]["properties"]["media"]["enum"] =
        serde_json::json!(["audio"]);
    let p = breaking_changes(&base_enum, &surface(&lost));
    assert!(
        p.iter().any(|x| x.starts_with("request enum lost values")),
        "{p:?}"
    );
    // The other directions are additive.
    let mut fine = enums.clone();
    fine["components"]["schemas"]["Resp"]["properties"]["status"]["enum"] =
        serde_json::json!(["active"]);
    fine["components"]["schemas"]["Req"]["properties"]["media"]["enum"] =
        serde_json::json!(["audio", "video", "screen"]);
    assert!(breaking_changes(&base_enum, &surface(&fine)).is_empty());

    // Additive changes pass.
    let p = mutate(&|d| {
        d["components"]["schemas"]["Resp"]["properties"]["event_namespaces"] =
            serde_json::json!({ "type": "object" });
        d["components"]["schemas"]["Req"]["properties"]["note"] =
            serde_json::json!({ "type": "string" });
        d["paths"]["/new"] = serde_json::json!({ "get": { "responses": { "200": {} } } });
    });
    assert!(p.is_empty(), "{p:?}");
}
