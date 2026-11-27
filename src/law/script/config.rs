// SPDX-FileCopyrightText: 2026-Present Nils Andreas Svee mailto:contact@lochnair.net (github @Lochnair)
//
// SPDX-License-Identifier: MPL-2.0

//! The configuration a script declares, as plain values. Each option's
//! default is also its type: a table's fields are its options, and a list's
//! first element is the template for every element. An empty list takes any
//! elements.
//!
//! `describe` gives options JSON Schema keywords, keyed like `config`. Inside
//! a table option, `properties` describes its fields; inside a list option,
//! `items` describes its elements.

use anyhow::{Context, bail, ensure};
use serde_json::{Map, Value, json};

/// Each bound, as the error words it and the test a value must pass.
type Bound = (&'static str, &'static str, fn(f64, f64) -> bool);
const BOUNDS: [Bound; 4] = [
    ("minimum", "at least", |n, bound| n >= bound),
    ("maximum", "at most", |n, bound| n <= bound),
    ("exclusiveMinimum", "above", |n, bound| n > bound),
    ("exclusiveMaximum", "below", |n, bound| n < bound),
];

#[derive(Clone, Debug)]
pub(crate) struct ScriptConfig {
    /// Always a table.
    defaults: Value,
    /// The keywords of the whole table: its `properties` are `describe`.
    describe: Map<String, Value>,
}

impl ScriptConfig {
    pub fn new(defaults: Map<String, Value>, describe: Map<String, Value>) -> anyhow::Result<Self> {
        ensure!(
            !defaults.contains_key("path"),
            "config.path is reserved for the script path"
        );
        check_properties(&defaults, &describe, "describe")?;
        let defaults = Value::Object(defaults);
        let describe = Map::from_iter([("properties".to_owned(), Value::Object(describe))]);
        // The defaults must pass their own rules, and a list's elements must
        // fit its first, which fills the fields they leave out.
        let mut errors = Vec::new();
        let defaults = conform(
            &defaults,
            defaults.clone(),
            Some(&describe),
            "config",
            &mut errors,
        );
        ensure!(
            errors.is_empty(),
            "invalid script config:\n  {}",
            errors.join("\n  ")
        );
        Ok(Self { defaults, describe })
    }

    /// The configured values over the defaults. Nothing is coerced: `"6"` is
    /// not a number.
    pub fn apply(&self, given: Map<String, Value>) -> anyhow::Result<Value> {
        let mut errors = Vec::new();
        let config = conform(
            &self.defaults,
            Value::Object(given),
            Some(&self.describe),
            "law.script",
            &mut errors,
        );
        ensure!(
            errors.is_empty(),
            "script config validation failed:\n  {}",
            errors.join("\n  ")
        );
        Ok(config)
    }

    /// The `law.script` table as JSON Schema: `path` and the declared options.
    pub fn json_schema(&self) -> Value {
        let mut schema = schema_of(Some(&self.defaults), Some(&self.describe));
        schema["properties"]["path"] =
            json!({ "type": "string", "description": "The script to run." });
        schema["required"] = json!(["path"]);
        schema
    }
}

/// `given` checked against `default` and `describe`, with omitted fields
/// filled from the default. Each problem is pushed to `errors`.
fn conform(
    default: &Value,
    given: Value,
    describe: Option<&Map<String, Value>>,
    at: &str,
    errors: &mut Vec<String>,
) -> Value {
    let keyword = |name: &str| describe?.get(name)?.as_object();
    let value = match (default, given) {
        (Value::Null, _) => {
            errors.push(format!("{at}: needs a default"));
            return Value::Null;
        }
        (Value::Object(fields), Value::Object(mut given)) => {
            for key in given.keys().filter(|key| !fields.contains_key(*key)) {
                errors.push(format!("{at}.{key}: not an option"));
            }
            let properties = keyword("properties");
            let fields = fields.iter().map(|(key, default)| {
                let value = match given.remove(key) {
                    Some(value) => {
                        let describe = properties.and_then(|p| p.get(key)?.as_object());
                        conform(default, value, describe, &format!("{at}.{key}"), errors)
                    }
                    None => default.clone(),
                };
                (key.clone(), value)
            });
            Value::Object(fields.collect())
        }
        (Value::Array(template), Value::Array(items)) => {
            let describe = keyword("items");
            let items = items.into_iter().enumerate().map(|(i, item)| {
                let at = format!("{at}[{i}]");
                match template.first() {
                    Some(template) => conform(template, item, describe, &at, errors),
                    None => {
                        errors.extend(violation(&item, describe).map(|p| format!("{at}: {p}")));
                        item
                    }
                }
            });
            Value::Array(items.collect())
        }
        (Value::Bool(_), value @ Value::Bool(_))
        | (Value::Number(_), value @ Value::Number(_))
        | (Value::String(_), value @ Value::String(_)) => value,
        (_, _) => {
            errors.push(format!("{at}: expected {}", type_name(default)));
            return default.clone();
        }
    };
    errors.extend(violation(&value, describe).map(|p| format!("{at}: {p}")));
    value
}

/// The schema `conform` enforces for `default`; with no default, anything.
fn schema_of(default: Option<&Value>, describe: Option<&Map<String, Value>>) -> Value {
    let keyword = |name: &str| describe?.get(name)?.as_object();
    let mut schema: Map<String, Value> = describe
        .into_iter()
        .flatten()
        .filter(|(key, _)| !matches!(key.as_str(), "properties" | "items"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut set = |key: &str, value: Value| {
        schema.insert(key.to_owned(), value);
    };
    match default {
        None | Some(Value::Null) => {}
        Some(Value::Bool(_)) => set("type", "boolean".into()),
        Some(Value::Number(_)) => set("type", "number".into()),
        Some(Value::String(_)) => set("type", "string".into()),
        Some(Value::Array(template)) => {
            set("type", "array".into());
            let items = schema_of(template.first(), keyword("items"));
            if items != json!({}) {
                set("items", items);
            }
        }
        Some(Value::Object(fields)) => {
            let properties = keyword("properties");
            let fields = fields.iter().map(|(key, default)| {
                let mut field = schema_of(
                    Some(default),
                    properties.and_then(|p| p.get(key)?.as_object()),
                );
                field["default"] = default.clone();
                (key.clone(), field)
            });
            set("type", "object".into());
            set("properties", Value::Object(fields.collect()));
            set("additionalProperties", false.into());
        }
    }
    Value::Object(schema)
}

/// Checks the keywords `describe` gives the fields of a table.
fn check_properties(
    fields: &Map<String, Value>,
    describe: &Map<String, Value>,
    at: &str,
) -> anyhow::Result<()> {
    for (key, keywords) in describe {
        let at = format!("{at}.{key}");
        let field = fields
            .get(key)
            .with_context(|| format!("{at}: not an option in config"))?;
        check_keywords(Some(field), keywords, &at)?;
    }
    Ok(())
}

/// Checks the keywords describing a value with this default, or a list
/// element of any type when there is none.
fn check_keywords(default: Option<&Value>, keywords: &Value, at: &str) -> anyhow::Result<()> {
    let Value::Object(keywords) = keywords else {
        bail!("{at} must be a table of keywords");
    };
    for (keyword, value) in keywords {
        let at = format!("{at}.{keyword}");
        let problem = match (keyword.as_str(), value, default) {
            ("title" | "description" | "unit", Value::String(_), _) => None,
            ("title" | "description" | "unit", _, _) => Some("must be a string"),
            (bound, _, _) if BOUNDS.iter().any(|(name, ..)| *name == bound) => {
                match (value, default) {
                    (Value::Number(_), None | Some(Value::Number(_))) => None,
                    (Value::Number(_), _) => Some("applies only to numbers"),
                    _ => Some("must be a number"),
                }
            }
            ("enum", Value::Array(choices), _) if !choices.is_empty() => None,
            ("enum", _, _) => Some("must be a list of the allowed values"),
            ("properties", Value::Object(describe), Some(Value::Object(fields))) => {
                check_properties(fields, describe, &at)?;
                None
            }
            ("properties", Value::Object(_), _) => Some("applies only to tables"),
            ("properties", _, _) => Some("must be a table"),
            ("items", _, Some(Value::Array(template))) => {
                check_keywords(template.first(), value, &at)?;
                None
            }
            ("items", _, _) => Some("applies only to lists"),
            _ => Some("is not a supported keyword"),
        };
        if let Some(problem) = problem {
            bail!("{at} {problem}");
        }
    }
    Ok(())
}

/// How `value` breaks the bounds or `enum` in `describe`, if it does.
fn violation(value: &Value, describe: Option<&Map<String, Value>>) -> Option<String> {
    let describe = describe?;
    if let Some(n) = value.as_f64() {
        for (keyword, words, holds) in BOUNDS {
            if let Some(bound) = describe.get(keyword)
                && !holds(n, bound.as_f64()?)
            {
                return Some(format!("must be {words} {bound}"));
            }
        }
    }
    let choices = describe.get("enum")?.as_array()?;
    // As in JSON Schema, 1 and 1.0 are the same number.
    let same = |choice: &Value| match (choice.as_f64(), value.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => choice == value,
    };
    (!choices.iter().any(same)).then(|| format!("must be one of {}", Value::Array(choices.clone())))
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declare(defaults: Value, describe: Value) -> anyhow::Result<ScriptConfig> {
        let (Value::Object(defaults), Value::Object(describe)) = (defaults, describe) else {
            panic!("defaults and describe are tables");
        };
        ScriptConfig::new(defaults, describe)
    }

    fn example() -> ScriptConfig {
        declare(
            json!({
                "delay_ms": 15, "mode": "auto", "tags": [],
                "download": { "delay_ms": 30.0, "on": false },
                "peers": [{ "host": "a", "port": 1 }],
            }),
            json!({
                "delay_ms": { "unit": "ms", "exclusiveMinimum": 0 },
                "mode": { "enum": ["auto", "manual"] },
                "download": { "properties": { "delay_ms": { "maximum": 100 } } },
                "peers": { "items": { "properties": { "port": { "minimum": 1 } } } },
            }),
        )
        .unwrap()
    }

    fn apply(config: &ScriptConfig, given: Value) -> anyhow::Result<Value> {
        let Value::Object(given) = given else {
            panic!("given is a table");
        };
        config.apply(given)
    }

    #[test]
    fn fills_defaults_and_list_elements_from_the_template() {
        let given = json!({
            "mode": "manual", "tags": [1, "x"],
            "download": { "on": true },
            "peers": [{ "host": "b" }, { "host": "c", "port": 3 }],
        });
        assert_eq!(
            apply(&example(), given).unwrap(),
            json!({
                "delay_ms": 15, "mode": "manual", "tags": [1, "x"],
                "download": { "delay_ms": 30.0, "on": true },
                "peers": [{ "host": "b", "port": 1 }, { "host": "c", "port": 3 }],
            })
        );
    }

    #[test]
    fn reports_every_problem_by_path_without_coercing() {
        let given = json!({
            "delay_ms": "20", "mode": "off", "bogus": 1,
            "download": { "delay_ms": 101, "beta": true },
            "peers": [{ "port": 0 }, "c"],
        });
        let error = apply(&example(), given).unwrap_err().to_string();
        for expected in [
            "law.script.delay_ms: expected a number",
            "law.script.mode: must be one of",
            "law.script.bogus: not an option",
            "law.script.download.delay_ms: must be at most 100",
            "law.script.download.beta: not an option",
            "law.script.peers[0].port: must be at least 1",
            "law.script.peers[1]: expected a table",
        ] {
            assert!(
                error.contains(expected),
                "{expected:?} missing from {error}"
            );
        }
    }

    #[test]
    fn rejects_inconsistent_defaults_and_misplaced_keywords() {
        for (defaults, describe, expected) in [
            (
                json!({ "l": [1, "a"] }),
                json!({}),
                "config.l[1]: expected a number",
            ),
            (
                json!({ "x": 0 }),
                json!({ "x": { "minimum": 1 } }),
                "config.x: must be at least 1",
            ),
            (
                json!({ "x": "a" }),
                json!({ "x": { "minimum": 1 } }),
                "applies only to numbers",
            ),
            (
                json!({ "x": 1 }),
                json!({ "x": { "label": "X" } }),
                "not a supported keyword",
            ),
            (
                json!({ "x": 1 }),
                json!({ "y": {} }),
                "describe.y: not an option",
            ),
        ] {
            let error = format!("{:#}", declare(defaults, describe).unwrap_err());
            assert!(
                error.contains(expected),
                "{expected:?} missing from {error}"
            );
        }
    }

    #[test]
    fn schema_states_what_apply_enforces() {
        let config = declare(
            json!({ "on": false, "peers": [{ "port": 1 }], "tags": [] }),
            json!({ "peers": { "items": { "properties": { "port": { "minimum": 1 } } } } }),
        )
        .unwrap();
        let schema = config.json_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["on"],
            json!({ "type": "boolean", "default": false })
        );
        assert_eq!(
            schema["properties"]["peers"]["items"],
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "port": { "type": "number", "minimum": 1, "default": 1 } },
            })
        );
        assert_eq!(
            schema["properties"]["tags"],
            json!({ "type": "array", "default": [] })
        );
        assert_eq!(schema["required"], json!(["path"]));
    }
}
