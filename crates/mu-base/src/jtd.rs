//! Converts an RFC 8927 (JTD) schema from an omp agent file's `output:` into the JSON Schema subset `schema.rs` validates, mirroring omp's `jtd-to-json-schema.ts`.

use serde_json::{Map, Value, json};

const JTD_ONLY: [&str; 9] = ["timestamp", "float32", "float64", "int8", "uint8", "int16", "uint16", "int32", "uint32"];
const DOC_KEYS: [&str; 13] = [
    "definitions",
    "metadata",
    "nullable",
    "ref",
    "type",
    "enum",
    "elements",
    "properties",
    "optionalProperties",
    "additionalProperties",
    "values",
    "discriminator",
    "mapping",
];

/// An agent file's `output:` as a checked JSON Schema: JTD documents are converted, anything else is taken as JSON Schema.
pub fn output_schema(v: &Value) -> Result<Value, String> {
    let s = if is_jtd(v) { convert(v, "output", 0)? } else { v.clone() };
    crate::schema::check_schema(&s)?;
    Ok(s)
}

fn is_jtd(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    if ["elements", "values", "optionalProperties", "discriminator", "ref"].iter().any(|k| m.contains_key(*k)) {
        return true;
    }
    if m.get("type").and_then(Value::as_str).is_some_and(|t| JTD_ONLY.contains(&t)) {
        return true;
    }
    m.contains_key("properties") && !m.contains_key("type") && is_jtd_document(v)
}

fn is_jtd_document(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    if m.keys().any(|k| !DOC_KEYS.contains(&k.as_str())) {
        return false;
    }
    let ok = |k: &str, f: &dyn Fn(&Value) -> bool| m.get(k).is_none_or(f);
    let all = |x: &Value| x.as_object().is_some_and(|o| o.values().all(is_jtd_document));
    ok("type", &|t| t.as_str().is_some_and(|t| t == "boolean" || t == "string" || JTD_ONLY.contains(&t)))
        && ok("enum", &|e| e.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
        && ok("ref", &Value::is_string)
        && ok("discriminator", &Value::is_string)
        && ok("nullable", &Value::is_boolean)
        && ok("additionalProperties", &Value::is_boolean)
        && ok("elements", &is_jtd_document)
        && ok("values", &is_jtd_document)
        && ok("properties", &all)
        && ok("optionalProperties", &all)
        && ok("definitions", &all)
        && ok("mapping", &all)
}

fn int(min: i64, max: i64) -> Value {
    json!({"type":"integer","minimum":min,"maximum":max})
}

fn convert(n: &Value, at: &str, depth: usize) -> Result<Value, String> {
    if depth > 64 {
        return Err(format!("{at}: schema nested too deeply"));
    }
    let Some(m) = n.as_object() else {
        return Err(format!("{at}: a JTD schema must be an object"));
    };
    if m.contains_key("ref") {
        return Err(format!("{at}: JTD `ref` is not supported; inline the definition"));
    }
    let mut out = if let Some(e) = m.get("enum") {
        if !e.as_array().is_some_and(|a| a.iter().all(Value::is_string)) {
            return Err(format!("{at}: `enum` must be an array of strings"));
        }
        json!({"enum": e})
    } else if let Some(el) = m.get("elements") {
        json!({"type":"array","items": convert(el, &format!("{at}.elements"), depth + 1)?})
    } else if let Some(t) = m.get("type") {
        match t.as_str() {
            Some("boolean") => json!({"type":"boolean"}),
            Some("string" | "timestamp") => json!({"type":"string"}),
            Some("float32" | "float64") => json!({"type":"number"}),
            Some("int8") => int(-128, 127),
            Some("uint8") => int(0, 255),
            Some("int16") => int(-32768, 32767),
            Some("uint16") => int(0, 65535),
            Some("int32") => int(-2147483648, 2147483647),
            Some("uint32") => int(0, 4294967295),
            _ => json!({"type": t}),
        }
    } else if let Some(v) = m.get("values") {
        json!({"type":"object","additionalProperties": convert(v, &format!("{at}.values"), depth + 1)?})
    } else if m.contains_key("properties") || m.contains_key("optionalProperties") {
        let mut props = Map::new();
        let mut required = Vec::new();
        for (k, is_req) in [("properties", true), ("optionalProperties", false)] {
            let Some(group) = m.get(k) else { continue };
            let Some(group) = group.as_object() else {
                return Err(format!("{at}: `{k}` must be an object"));
            };
            for (key, sub) in group {
                props.insert(key.clone(), convert(sub, &format!("{at}.{key}"), depth + 1)?);
                if is_req {
                    required.push(json!(key));
                }
            }
        }
        let mut o = Map::new();
        o.insert("type".into(), json!("object"));
        o.insert("properties".into(), Value::Object(props));
        if !required.is_empty() {
            o.insert("required".into(), Value::Array(required));
        }
        if m.get("additionalProperties") != Some(&json!(true)) {
            o.insert("additionalProperties".into(), json!(false));
        }
        Value::Object(o)
    } else if m.contains_key("discriminator") {
        let (Some(tag_name), Some(mapping)) =
            (m["discriminator"].as_str(), m.get("mapping").and_then(Value::as_object))
        else {
            return Err(format!("{at}: `discriminator` needs a string tag and a `mapping` object"));
        };
        let mut alts = Vec::new();
        for (tag, sub) in mapping {
            let mut s = convert(sub, &format!("{at}.mapping.{tag}"), depth + 1)?;
            if let Some(o) = s.as_object_mut() {
                let props = o.entry("properties").or_insert_with(|| json!({}));
                if let Some(p) = props.as_object_mut() {
                    p.insert(tag_name.into(), json!({"const": tag}));
                }
                let req = o.entry("required").or_insert_with(|| json!([]));
                if let Some(r) = req.as_array_mut()
                    && !r.iter().any(|x| x == tag_name)
                {
                    r.push(json!(tag_name));
                }
            }
            alts.push(s);
        }
        json!({"oneOf": alts})
    } else {
        json!({})
    };
    if let Some(d) = m.get("metadata").and_then(|md| md.get("description")).filter(|d| d.is_string())
        && let Some(o) = out.as_object_mut()
    {
        o.insert("description".into(), d.clone());
    }
    if m.get("nullable") == Some(&json!(true)) {
        out = json!({"anyOf":[out, {"type":"null"}]});
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_reviewer_output() {
        let jtd = json!({
            "properties": {
                "overall_correctness": {"type": "string"},
                "explanation": {"metadata": {"description": "Plain explanation"}, "type": "string"},
                "confidence": {"type": "float32"}
            },
            "optionalProperties": {
                "findings": {"elements": {"properties": {"title": {"type": "string"}}}}
            }
        });
        let s = output_schema(&jtd).unwrap();
        assert_eq!(s["required"], json!(["overall_correctness", "explanation", "confidence"]));
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(s["properties"]["findings"]["type"], "array");
        assert_eq!(s["properties"]["explanation"]["description"], "Plain explanation");
        let e = output_schema(&json!({"properties":{"v":{"enum":["a","b"]}}})).unwrap();
        assert_eq!(e["properties"]["v"], json!({"enum":["a","b"]}));
    }

    #[test]
    fn integer_ranges_and_nullable() {
        let s = output_schema(&json!({"type":"uint8","nullable":true})).unwrap();
        assert_eq!(s, json!({"anyOf":[{"type":"integer","minimum":0,"maximum":255},{"type":"null"}]}));
    }

    #[test]
    fn discriminator() {
        let s = output_schema(&json!({"discriminator":"kind","mapping":{"a":{"properties":{"x":{"type":"string"}}}}}))
            .unwrap();
        assert_eq!(s["oneOf"][0]["properties"]["kind"], json!({"const":"a"}));
        assert_eq!(s["oneOf"][0]["required"], json!(["x", "kind"]));
        assert!(crate::schema::validate(&s, &json!({"kind":"a","x":"s"})).is_ok());
        assert!(crate::schema::validate(&s, &json!({"kind":"b","x":"s"})).is_err());
    }

    #[test]
    fn additional_properties_true_is_open() {
        let s = output_schema(&json!({"properties":{"a":{"type":"string"}},"additionalProperties":true})).unwrap();
        assert!(s.get("additionalProperties").is_none());
    }

    #[test]
    fn json_schema_passes_through() {
        let j = json!({"type":"object","properties":{"a":{"type":"integer"}}});
        assert_eq!(output_schema(&j).unwrap(), j);
    }

    #[test]
    fn errors() {
        assert!(output_schema(&json!({"properties":{"a":{"ref":"T"}}})).unwrap_err().contains("ref"));
        assert!(output_schema(&json!({"type":"strng"})).is_err());
        assert!(output_schema(&json!("x")).is_err());
    }
}
