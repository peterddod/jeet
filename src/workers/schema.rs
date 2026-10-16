//! A small JSON Schema check for what a worker job returns.
//!
//! It knows the subset jeet writes its schemas in: `type` (object, array,
//! string, boolean, integer, number), `properties`, `required`, `items`,
//! `enum` and `additionalProperties: false`, with `description` and `title`
//! as notes it ignores. A schema with any other keyword is refused by
//! [`check_schema`] when a job is loaded, so a job cannot go unchecked
//! without anyone knowing. `claude --json-schema` is given the same schema,
//! but its answer is checked here too: the check does not depend on it.

use anyhow::{bail, Result};
use serde_json::Value;

/// The keywords a schema may use.
const KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "additionalProperties",
    "description",
    "title",
];

const TYPES: &[&str] = &["object", "array", "string", "boolean", "integer", "number"];

/// Refuse `schema` unless it uses only what [`validate`] understands. The
/// error names the keyword and where it is, as a JSON path into the schema.
/// The top level must be an object: `claude --json-schema` takes no other.
pub fn check_schema(schema: &Value) -> Result<()> {
    check_any(schema)?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!("schema $: must have `\"type\": \"object\"` at the top");
    }
    Ok(())
}

/// [`check_schema`] for a schema that need not be an object.
fn check_any(schema: &Value) -> Result<()> {
    check_at(schema, "$")
}

/// `text` cut to 60 characters: what an answer echoes of the model's words.
fn clip(text: &str) -> String {
    const MAX: usize = 60;
    match text.chars().count() > MAX {
        true => format!("{}…", text.chars().take(MAX - 1).collect::<String>()),
        false => text.to_string(),
    }
}

fn check_at(schema: &Value, path: &str) -> Result<()> {
    let Some(map) = schema.as_object() else {
        bail!("schema {path}: must be an object");
    };
    for (key, value) in map {
        let at = format!("{path}.{key}");
        match key.as_str() {
            "type" => match value.as_str() {
                Some(t) if TYPES.contains(&t) => {}
                _ => bail!("schema {at}: must be one of {}", TYPES.join(", ")),
            },
            "properties" => {
                let Some(props) = value.as_object() else {
                    bail!("schema {at}: must be an object of schemas");
                };
                for (name, sub) in props {
                    check_at(sub, &format!("{at}.{name}"))?;
                }
            }
            "required" => {
                let names = value.as_array();
                if !names.is_some_and(|names| names.iter().all(Value::is_string)) {
                    bail!("schema {at}: must be an array of property names");
                }
            }
            "items" => check_at(value, &at)?,
            "enum" => {
                if value.as_array().is_none_or(|values| values.is_empty()) {
                    bail!("schema {at}: must be a non-empty array");
                }
            }
            "additionalProperties" => {
                if !value.is_boolean() {
                    bail!("schema {at}: must be true or false");
                }
            }
            "description" | "title" => {
                if !value.is_string() {
                    bail!("schema {at}: must be a string");
                }
            }
            other => bail!(
                "schema {at}: jeet does not check `{other}`; use only {}",
                KEYWORDS.join(", ")
            ),
        }
    }
    Ok(())
}

/// Check `value` against `schema`, which [`check_schema`] passed. The first
/// error, with the JSON path of what is wrong in `value` (`$.items[1].why`).
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    validate_at(schema, value, "$")
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(map) = schema.as_object() else {
        return Err(format!("{path}: the schema is not an object"));
    };
    if let Some(t) = map.get("type").and_then(Value::as_str) {
        if !is_type(t, value) {
            return Err(format!("{path}: expected {t}, got {}", kind(value)));
        }
    }
    if let Some(Value::Array(allowed)) = map.get("enum") {
        if !allowed.contains(value) {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            return Err(format!(
                "{path}: must be one of {}, got {}",
                list.join(", "),
                clip(&value.to_string())
            ));
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(Value::Array(required)) = map.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}: missing required property `{name}`"));
                }
            }
        }
        let props = map.get("properties").and_then(Value::as_object);
        for (name, member) in object {
            match props.and_then(|props| props.get(name)) {
                Some(sub) => validate_at(sub, member, &format!("{path}.{name}"))?,
                None if map.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{path}: unexpected property `{}`", clip(name)));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(sub)) = (value.as_array(), map.get("items")) {
        for (i, item) in items.iter().enumerate() {
            validate_at(sub, item, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

fn is_type(t: &str, value: &Value) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        // 2.0 is an integer in JSON Schema, as 2 is.
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape triage will use: a summary and a verdict for each item.
    fn triage() -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "ref": {"type": "string"},
                            "verdict": {
                                "type": "string",
                                "enum": ["ignore", "digest", "attention", "notify_user"]
                            },
                            "why": {"type": "string"}
                        },
                        "required": ["ref", "verdict", "why"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["summary", "items"],
            "additionalProperties": false
        })
    }

    #[test]
    fn the_triage_shape_is_a_schema_and_checks_its_output() {
        let schema = triage();
        check_schema(&schema).unwrap();
        let good = json!({
            "summary": "two things",
            "items": [
                {"ref": "r1", "verdict": "ignore", "why": "a duplicate"},
                {"ref": "r2", "verdict": "notify_user", "why": "CI is red"}
            ]
        });
        validate(&schema, &good).unwrap();
        validate(&schema, &json!({"summary": "none", "items": []})).unwrap();

        let bad = |value: Value| validate(&schema, &value).unwrap_err();
        assert_eq!(
            bad(json!({"summary": "x"})),
            "$: missing required property `items`"
        );
        assert_eq!(
            bad(json!({"summary": 3, "items": []})),
            "$.summary: expected string, got number"
        );
        let wrong = json!({"summary": "x", "items": [
            {"ref": "a", "verdict": "ignore", "why": "w"},
            {"ref": "b", "verdict": "page_me", "why": "w"}
        ]});
        assert_eq!(
            bad(wrong),
            "$.items[1].verdict: must be one of \"ignore\", \"digest\", \"attention\", \
             \"notify_user\", got \"page_me\""
        );
        let extra = json!({"summary": "x", "items": [
            {"ref": "a", "verdict": "ignore", "why": "w", "also": 1}
        ]});
        assert_eq!(bad(extra), "$.items[0]: unexpected property `also`");
        assert_eq!(bad(json!([])), "$: expected object, got array");
        let missing = json!({"summary": "x", "items": [{"ref": "a"}]});
        assert_eq!(
            bad(missing),
            "$.items[0]: missing required property `verdict`"
        );
    }

    #[test]
    fn every_type_is_checked() {
        let cases = [
            ("object", json!({}), json!([])),
            ("array", json!([]), json!({})),
            ("string", json!("s"), json!(1)),
            ("boolean", json!(false), json!("false")),
            ("integer", json!(3), json!(3.5)),
            ("number", json!(3.5), json!("3")),
        ];
        for (t, good, bad) in cases {
            let schema = json!({"type": t});
            check_any(&schema).unwrap();
            validate(&schema, &good).unwrap_or_else(|e| panic!("{t}: {e}"));
            assert!(validate(&schema, &bad).is_err(), "{t}");
        }
        // A whole number written with a fraction is an integer, as in JSON Schema.
        validate(&json!({"type": "integer"}), &json!(2.0)).unwrap();
        // Null is none of them.
        assert!(validate(&json!({"type": "string"}), &Value::Null).is_err());
    }

    #[test]
    fn properties_not_named_are_allowed_unless_forbidden() {
        let open = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        validate(&open, &json!({"a": "x", "b": 1})).unwrap();
        let closed = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": false
        });
        let e = validate(&closed, &json!({"a": "x", "b": 1})).unwrap_err();
        assert_eq!(e, "$: unexpected property `b`");
        // Present but wrong is caught either way.
        let e = validate(&open, &json!({"a": 1})).unwrap_err();
        assert_eq!(e, "$.a: expected string, got number");
    }

    #[test]
    fn a_schema_with_a_keyword_jeet_does_not_check_is_refused() {
        for (schema, what) in [
            (json!({"type": "string", "pattern": "^a"}), "`pattern`"),
            (json!({"oneOf": []}), "`oneOf`"),
            (json!({"$ref": "#/x"}), "`$ref`"),
            (
                json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}}),
                "$.properties.n.minimum",
            ),
            (
                json!({"type": "array", "items": {"type": "string", "format": "uri"}}),
                "$.items.format",
            ),
        ] {
            let e = check_schema(&schema).unwrap_err().to_string();
            assert!(e.contains(what), "{schema}: {e}");
        }
    }

    #[test]
    fn a_malformed_schema_is_refused() {
        for schema in [
            json!("string"),
            json!([]),
            json!({"type": "text"}),
            json!({"type": ["string", "null"]}),
            json!({"properties": []}),
            json!({"required": "a"}),
            json!({"required": [1]}),
            json!({"enum": []}),
            json!({"enum": "a"}),
            json!({"additionalProperties": {"type": "string"}}),
            json!({"items": "string"}),
            json!({"properties": {"a": {"type": "nope"}}}),
            json!({"description": 1}),
        ] {
            assert!(check_schema(&schema).is_err(), "{schema}");
        }
        // Notes are allowed.
        check_any(&json!({"type": "string", "description": "d", "title": "t"})).unwrap();
    }

    #[test]
    fn the_top_level_must_be_an_object_and_echoes_are_short() {
        check_schema(&json!({"type": "object"})).unwrap();
        for schema in [json!({"type": "string"}), json!({}), json!({"enum": [1]})] {
            let e = check_schema(&schema).unwrap_err().to_string();
            assert!(e.contains("\"type\": \"object\""), "{schema}: {e}");
        }
        let schema = json!({"type": "object", "additionalProperties": false});
        let long = "p".repeat(500);
        let e = validate(&schema, &json!({ long.clone(): 1 })).unwrap_err();
        assert!(e.len() < 120, "{e}");
        let e = validate(&json!({"enum": ["a"]}), &json!(long)).unwrap_err();
        assert!(e.len() < 120 && e.ends_with("…"), "{e}");
    }

    #[test]
    fn an_enum_checks_values_of_any_type() {
        let schema = json!({"enum": [1, "a", null]});
        for good in [json!(1), json!("a"), Value::Null] {
            validate(&schema, &good).unwrap();
        }
        let e = validate(&schema, &json!(2)).unwrap_err();
        assert_eq!(e, "$: must be one of 1, \"a\", null, got 2");
    }
}
