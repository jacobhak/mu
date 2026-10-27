//! A small JSON Schema validator for subagent results (`outputSchema` / `yield`).
//!
//! Covers what result schemas use: type, enum, const, properties/required/additionalProperties,
//! items/minItems/maxItems, minimum/maximum, minLength/maxLength, anyOf/oneOf/allOf. `$ref` is
//! rejected (never resolved, so a model-written schema cannot reach files or the network).
//! Unknown keywords are ignored.

use serde_json::Value;

/// Largest accepted schema, serialized: it travels in every task call.
pub const MAX_SCHEMA_BYTES: usize = 32 * 1024;
/// Nesting depth bound for schemas and values.
const MAX_DEPTH: usize = 64;
/// Errors reported to the model per submission.
const MAX_ERRORS: usize = 10;

const TYPES: [&str; 7] = ["string", "number", "integer", "boolean", "object", "array", "null"];

/// Checks that `schema` is usable: an object, small, with known `type` names and no `$ref`.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    if !schema.is_object() {
        return Err("outputSchema must be a JSON Schema object".into());
    }
    if schema.to_string().len() > MAX_SCHEMA_BYTES {
        return Err(format!("outputSchema is larger than {} KiB", MAX_SCHEMA_BYTES / 1024));
    }
    check_node(schema, "outputSchema", 0)
}

fn check_node(s: &Value, at: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("{at}: schema nested too deeply"));
    }
    let Some(m) = s.as_object() else {
        return if s.is_boolean() { Ok(()) } else { Err(format!("{at}: a schema must be an object or boolean")) };
    };
    if m.contains_key("$ref") {
        return Err(format!("{at}: $ref is not supported; inline the schema"));
    }
    match m.get("type") {
        None => {}
        Some(Value::String(t)) if TYPES.contains(&t.as_str()) => {}
        Some(Value::Array(ts)) if ts.iter().all(|t| t.as_str().is_some_and(|t| TYPES.contains(&t))) => {}
        Some(_) => return Err(format!("{at}: invalid `type`")),
    }
    if let Some(r) = m.get("required")
        && !r.as_array().is_some_and(|a| a.iter().all(Value::is_string))
    {
        return Err(format!("{at}: `required` must be an array of strings"));
    }
    if let Some(p) = m.get("properties") {
        let Some(p) = p.as_object() else { return Err(format!("{at}: `properties` must be an object")) };
        for (k, v) in p {
            check_node(v, &format!("{at}.{k}"), depth + 1)?;
        }
    }
    for k in ["items", "additionalProperties"] {
        if let Some(v) = m.get(k) {
            check_node(v, &format!("{at}.{k}"), depth + 1)?;
        }
    }
    for k in ["anyOf", "oneOf", "allOf"] {
        if let Some(v) = m.get(k) {
            let Some(a) = v.as_array() else { return Err(format!("{at}: `{k}` must be an array")) };
            for (i, v) in a.iter().enumerate() {
                check_node(v, &format!("{at}.{k}[{i}]"), depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Validates `value` against `schema`; the error list names each failing path (capped).
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errs = Vec::new();
    walk(schema, value, "$", 0, &mut errs);
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn walk(s: &Value, v: &Value, at: &str, depth: usize, errs: &mut Vec<String>) {
    if errs.len() >= MAX_ERRORS || depth > MAX_DEPTH {
        return;
    }
    let Some(m) = s.as_object() else {
        if s == &Value::Bool(false) {
            errs.push(format!("{at}: no value is allowed here"));
        }
        return;
    };
    let mut mine: Vec<String> = Vec::new();
    let mut fail = |msg: String| {
        if mine.len() < MAX_ERRORS {
            mine.push(format!("{at}: {msg}"));
        }
    };
    match m.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => fail(format!("expected {t}, got {}", kind(v))),
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            let names: Vec<&str> = ts.iter().filter_map(Value::as_str).collect();
            fail(format!("expected {}, got {}", names.join(" or "), kind(v)));
        }
        _ => {}
    }
    if let Some(Value::Array(e)) = m.get("enum")
        && !e.contains(v)
    {
        fail(format!("must be one of {}", Value::Array(e.clone())));
    }
    if let Some(c) = m.get("const")
        && c != v
    {
        fail(format!("must equal {c}"));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = m.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            fail(format!("must be >= {min}"));
        }
        if let Some(max) = m.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            fail(format!("must be <= {max}"));
        }
    }
    if let Some(s) = v.as_str() {
        let len = s.chars().count() as u64;
        if let Some(min) = m.get("minLength").and_then(Value::as_u64)
            && len < min
        {
            fail(format!("must have at least {min} characters"));
        }
        if let Some(max) = m.get("maxLength").and_then(Value::as_u64)
            && len > max
        {
            fail(format!("must have at most {max} characters"));
        }
    }
    if let Some(a) = v.as_array() {
        let len = a.len() as u64;
        if let Some(min) = m.get("minItems").and_then(Value::as_u64)
            && len < min
        {
            fail(format!("must have at least {min} items"));
        }
        if let Some(max) = m.get("maxItems").and_then(Value::as_u64)
            && len > max
        {
            fail(format!("must have at most {max} items"));
        }
        if let Some(items) = m.get("items") {
            for (i, x) in a.iter().enumerate() {
                walk(items, x, &format!("{at}[{i}]"), depth + 1, errs);
            }
        }
    }
    if let Some(o) = v.as_object() {
        if let Some(Value::Array(req)) = m.get("required") {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    fail(format!("missing required property `{k}`"));
                }
            }
        }
        let props = m.get("properties").and_then(Value::as_object);
        if let Some(props) = props {
            for (k, ps) in props {
                if let Some(x) = o.get(k) {
                    walk(ps, x, &format!("{at}.{k}"), depth + 1, errs);
                }
            }
        }
        if let Some(ap) = m.get("additionalProperties") {
            for (k, x) in o.iter().filter(|(k, _)| !props.is_some_and(|p| p.contains_key(*k))) {
                match ap {
                    Value::Bool(false) => fail(format!("unexpected property `{k}`")),
                    Value::Object(_) => walk(ap, x, &format!("{at}.{k}"), depth + 1, errs),
                    _ => {}
                }
            }
        }
    }
    let subs = |k: &str| m.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let passes = |sub: &Value| validate_at(sub, v, depth + 1);
    for sub in subs("allOf") {
        walk(&sub, v, at, depth + 1, errs);
    }
    let any = subs("anyOf");
    if !any.is_empty() && !any.iter().any(passes) {
        fail("matches none of the anyOf alternatives".into());
    }
    let one = subs("oneOf");
    if !one.is_empty() {
        let n = one.iter().filter(|s| passes(s)).count();
        if n != 1 {
            fail(format!("must match exactly one of the oneOf alternatives (matches {n})"));
        }
    }
    errs.extend(mine);
    errs.truncate(MAX_ERRORS);
}

fn validate_at(s: &Value, v: &Value, depth: usize) -> bool {
    let mut errs = Vec::new();
    walk(s, v, "$", depth, &mut errs);
    errs.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object","required":["verdict","items"],"additionalProperties":false,"properties":{
            "verdict":{"enum":["pass","fail"]},
            "n":{"type":"integer","minimum":0},
            "items":{"type":"array","maxItems":2,"items":{"type":"object","required":["t"],"properties":{"t":{"type":"string"}}}}}})
    }

    #[test]
    fn accepts_a_conforming_value() {
        assert!(validate(&schema(), &json!({"verdict":"pass","n":3,"items":[{"t":"a"}]})).is_ok());
    }

    #[test]
    fn rejects_with_paths() {
        let e = validate(&schema(), &json!({"verdict":"maybe","n":-1.5,"x":1,"items":[{"t":1},{},{}]})).unwrap_err();
        let all = e.join("\n");
        for want in ["$.verdict", "$.n", "unexpected property `x`", "$.items[0].t", "$.items[1]", "at most 2"] {
            assert!(all.contains(want), "{want} not in {all}");
        }
        assert!(validate(&schema(), &json!([])).unwrap_err()[0].contains("expected object"));
    }

    #[test]
    fn combinators_and_unions() {
        let s = json!({"anyOf":[{"type":"string"},{"type":["integer","null"]}]});
        assert!(validate(&s, &json!("x")).is_ok() && validate(&s, &json!(null)).is_ok());
        assert!(validate(&s, &json!(1.5)).is_err());
        let one = json!({"oneOf":[{"type":"number"},{"type":"integer"}]});
        assert!(validate(&one, &json!(1)).is_err() && validate(&one, &json!(1.5)).is_ok());
    }

    #[test]
    fn check_schema_rejects_bad_schemas() {
        assert!(check_schema(&schema()).is_ok());
        assert!(check_schema(&json!("x")).is_err());
        assert!(check_schema(&json!({"type":"strng"})).is_err());
        assert!(check_schema(&json!({"properties":{"a":{"$ref":"http://x"}}})).unwrap_err().contains("$ref"));
        assert!(check_schema(&json!({"description":"x".repeat(MAX_SCHEMA_BYTES)})).is_err());
    }

    #[test]
    fn errors_are_capped() {
        let e = validate(&json!({"items":{"type":"string"}}), &Value::Array(vec![json!(1); 50])).unwrap_err();
        assert_eq!(e.len(), MAX_ERRORS);
    }
}
