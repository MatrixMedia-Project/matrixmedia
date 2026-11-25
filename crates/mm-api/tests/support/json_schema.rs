//! A small JSON Schema (draft 2020-12) validator for the event contracts in
//! `contracts/events/`.
//!
//! It implements exactly the keywords those files use and panics on any other
//! keyword, so a schema can never pass because the validator skipped a rule it
//! does not know. Annotation keywords (`title`, `description`, `examples`, …)
//! are ignored, as the specification allows.

use serde_json::Value;

/// Keywords that carry no validation rule.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "examples",
    "default",
    "format",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// Validate `instance` against `schema`. `Err` lists every violation found,
/// each prefixed with its JSON path.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, instance, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Load a schema from `contracts/events/<name>.json`.
pub fn load_event_schema(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/events")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read schema {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse schema {}: {e}", path.display()))
}

/// Assert that `instance` validates against `contracts/events/<name>.json`.
#[track_caller]
pub fn assert_valid(name: &str, instance: &Value) {
    let schema = load_event_schema(name);
    if let Err(errors) = validate(&schema, instance) {
        panic!(
            "payload does not validate against {name}.json:\n  {}\npayload: {instance:#}",
            errors.join("\n  ")
        );
    }
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        other => panic!("unknown JSON Schema type {other:?}"),
    }
}

fn check(schema: &Value, inst: &Value, path: &str, errors: &mut Vec<String>) {
    let obj = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{path}: no value is allowed here"));
            return;
        }
        Value::Object(o) => o,
        other => panic!("schema at {path} is not an object or boolean: {other}"),
    };

    for (key, rule) in obj {
        if ANNOTATIONS.contains(&key.as_str()) || key.starts_with("x-") {
            continue;
        }
        match key.as_str() {
            "type" => {
                let ok = match rule {
                    Value::String(t) => type_matches(t, inst),
                    Value::Array(ts) => ts.iter().any(|t| type_matches(t.as_str().unwrap(), inst)),
                    _ => panic!("bad `type` at {path}"),
                };
                if !ok {
                    errors.push(format!("{path}: expected type {rule}, got {inst}"));
                }
            }
            "const" => {
                if inst != rule {
                    errors.push(format!("{path}: expected {rule}, got {inst}"));
                }
            }
            "enum" => {
                let options = rule.as_array().expect("enum is an array");
                if !options.contains(inst) {
                    errors.push(format!("{path}: {inst} is not one of {rule}"));
                }
            }
            "required" => {
                if let Some(o) = inst.as_object() {
                    for name in rule.as_array().expect("required is an array") {
                        let name = name.as_str().expect("required entries are strings");
                        if !o.contains_key(name) {
                            errors.push(format!("{path}: missing required property {name:?}"));
                        }
                    }
                }
            }
            "properties" => {
                if let Some(o) = inst.as_object() {
                    for (name, sub) in rule.as_object().expect("properties is an object") {
                        if let Some(v) = o.get(name) {
                            check(sub, v, &format!("{path}.{name}"), errors);
                        }
                    }
                }
            }
            "additionalProperties" => {
                if let Some(o) = inst.as_object() {
                    let declared = obj.get("properties").and_then(Value::as_object);
                    for (name, v) in o {
                        if declared.is_some_and(|d| d.contains_key(name)) {
                            continue;
                        }
                        match rule {
                            Value::Bool(false) => {
                                errors.push(format!("{path}: property {name:?} is not allowed"))
                            }
                            other => check(other, v, &format!("{path}.{name}"), errors),
                        }
                    }
                }
            }
            "minProperties" | "maxProperties" => {
                if let Some(o) = inst.as_object() {
                    let n = rule.as_u64().expect("count") as usize;
                    let bad = if key == "minProperties" {
                        o.len() < n
                    } else {
                        o.len() > n
                    };
                    if bad {
                        errors.push(format!("{path}: {key} {n}, has {}", o.len()));
                    }
                }
            }
            "items" => {
                if let Some(a) = inst.as_array() {
                    for (i, v) in a.iter().enumerate() {
                        check(rule, v, &format!("{path}[{i}]"), errors);
                    }
                }
            }
            "minItems" | "maxItems" => {
                if let Some(a) = inst.as_array() {
                    let n = rule.as_u64().expect("count") as usize;
                    let bad = if key == "minItems" {
                        a.len() < n
                    } else {
                        a.len() > n
                    };
                    if bad {
                        errors.push(format!("{path}: {key} {n}, has {}", a.len()));
                    }
                }
            }
            "uniqueItems" => {
                if rule == &Value::Bool(true)
                    && let Some(a) = inst.as_array()
                {
                    for (i, v) in a.iter().enumerate() {
                        if a[..i].contains(v) {
                            errors.push(format!("{path}: item {i} is a duplicate"));
                        }
                    }
                }
            }
            "minimum" | "maximum" => {
                if let Some(n) = inst.as_f64() {
                    let limit = rule.as_f64().expect("numeric limit");
                    let bad = if key == "minimum" {
                        n < limit
                    } else {
                        n > limit
                    };
                    if bad {
                        errors.push(format!("{path}: {n} breaks {key} {limit}"));
                    }
                }
            }
            "minLength" | "maxLength" => {
                if let Some(s) = inst.as_str() {
                    let n = rule.as_u64().expect("length") as usize;
                    let len = s.chars().count();
                    let bad = if key == "minLength" { len < n } else { len > n };
                    if bad {
                        errors.push(format!("{path}: length {len} breaks {key} {n}"));
                    }
                }
            }
            "pattern" => {
                if let Some(s) = inst.as_str() {
                    let re = regex::Regex::new(rule.as_str().expect("pattern is a string"))
                        .expect("pattern compiles");
                    if !re.is_match(s) {
                        errors.push(format!("{path}: {s:?} does not match {rule}"));
                    }
                }
            }
            "oneOf" | "anyOf" | "allOf" => {
                let branches = rule.as_array().expect("branches");
                let passing = branches
                    .iter()
                    .filter(|b| {
                        let mut sub = Vec::new();
                        check(b, inst, path, &mut sub);
                        sub.is_empty()
                    })
                    .count();
                let ok = match key.as_str() {
                    "oneOf" => passing == 1,
                    "anyOf" => passing >= 1,
                    _ => passing == branches.len(),
                };
                if !ok {
                    errors.push(format!(
                        "{path}: {key} matched {passing} of {} branches",
                        branches.len()
                    ));
                }
            }
            other => panic!(
                "the test validator does not implement JSON Schema keyword {other:?} (at {path}); \
                 add it before using it in a contract"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_validator_rejects_what_it_should() {
        let schema = json!({
            "type": "object",
            "required": ["a"],
            "properties": {
                "a": { "type": "integer", "minimum": 1 },
                "b": { "type": "string", "pattern": "^x", "maxLength": 3 },
                "c": { "enum": ["p", "q"] }
            },
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({ "a": 1, "b": "xy", "c": "p" })).is_ok());
        assert!(validate(&schema, &json!({})).is_err(), "missing required");
        assert!(validate(&schema, &json!({ "a": 0 })).is_err(), "minimum");
        assert!(validate(&schema, &json!({ "a": 1.5 })).is_err(), "integer");
        assert!(
            validate(&schema, &json!({ "a": 1, "b": "y" })).is_err(),
            "pattern"
        );
        assert!(
            validate(&schema, &json!({ "a": 1, "b": "xxxx" })).is_err(),
            "maxLength"
        );
        assert!(
            validate(&schema, &json!({ "a": 1, "c": "r" })).is_err(),
            "enum"
        );
        assert!(
            validate(&schema, &json!({ "a": 1, "z": 1 })).is_err(),
            "additionalProperties"
        );
        let one_of = json!({ "oneOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(validate(&one_of, &json!(1)).is_ok());
        assert!(validate(&one_of, &json!(true)).is_err());
    }
}
