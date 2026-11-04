//! Between MCP and mu's tool model: tool names, schemas, call arguments and call results,
//! following omp so sessions read the same in both.

use mu_base::message::Image;
use serde_json::{Value, json};

/// Zig's `std.hash.Wyhash` with seed 0, which is what omp's `Bun.hash` computes (the 64-char name
/// cap needs the same suffix omp writes). Ported from Zig 0.14 `lib/std/hash/wyhash.zig`.
fn wyhash(input: &[u8]) -> u64 {
    const SECRET: [u64; 4] = [0xa0761d6478bd642f, 0xe7037ed1a0b428db, 0x8ebc6af09c88c6e3, 0x589965cc75374cc3];
    fn read(n: usize, d: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        b[..n].copy_from_slice(&d[..n]);
        u64::from_le_bytes(b)
    }
    fn mum(a: u64, b: u64) -> (u64, u64) {
        let x = (a as u128).wrapping_mul(b as u128);
        (x as u64, (x >> 64) as u64)
    }
    fn mix(a: u64, b: u64) -> u64 {
        let (a, b) = mum(a, b);
        a ^ b
    }
    let seed = 0u64;
    let mut st = [seed ^ mix(seed ^ SECRET[0], SECRET[1]); 3];
    let (mut a, mut b);
    let len = input.len();
    if len <= 16 {
        if len >= 4 {
            let end = len - 4;
            let quarter = (len >> 3) << 2;
            a = (read(4, input) << 32) | read(4, &input[quarter..]);
            b = (read(4, &input[end..]) << 32) | read(4, &input[end - quarter..]);
        } else if len > 0 {
            a = ((input[0] as u64) << 16) | ((input[len >> 1] as u64) << 8) | input[len - 1] as u64;
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0;
        if len >= 48 {
            while i + 48 < len {
                for (k, s) in st.iter_mut().enumerate() {
                    let x = read(8, &input[i + 16 * k..]);
                    let y = read(8, &input[i + 16 * k + 8..]);
                    *s = mix(x ^ SECRET[k + 1], y ^ *s);
                }
                i += 48;
            }
            st[0] ^= st[1] ^ st[2];
        }
        let rest = &input[i..];
        let mut j = 0;
        while j + 16 < rest.len() {
            st[0] = mix(read(8, &rest[j..]) ^ SECRET[1], read(8, &rest[j + 8..]) ^ st[0]);
            j += 16;
        }
        a = read(8, &input[len - 16..]);
        b = read(8, &input[len - 8..]);
    }
    a ^= SECRET[1];
    b ^= st[0];
    let (a, b) = mum(a, b);
    mix(a ^ SECRET[0] ^ len as u64, b ^ SECRET[1])
}

/// `u64` in base 36, lowercase, as JavaScript's `toString(36)`.
fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Lowercase, runs of anything outside `[a-z0-9_]` become `_`, `_` runs collapse, ends are trimmed.
fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('_').to_string()
}

/// `mcp__<server>_<tool>`, omp's scheme. A leading `<server>_` on the tool is dropped. Names over
/// 64 chars keep 55 characters, then `_` and the first 8 characters of the base-36 Wyhash of the
/// full name, as omp does.
pub fn tool_name(server: &str, tool: &str) -> String {
    let server = Some(sanitize(server)).filter(|s| !s.is_empty()).unwrap_or_else(|| "server".into());
    let mut t = sanitize(tool);
    if let Some(rest) = t.strip_prefix(&format!("{server}_")) {
        t = rest.to_string();
    }
    if t.is_empty() {
        t = "tool".into();
    }
    let name = format!("mcp__{server}_{t}");
    if name.len() <= 64 {
        return name;
    }
    let hash = base36(wyhash(name.as_bytes()));
    format!("{}_{}", &name[..55], &hash[..hash.len().min(8)])
}

/// The schema as advertised: `$schema` and `nullable` dropped (not valid for every provider), and
/// always an object schema.
pub fn normalize_schema(schema: &Value) -> Value {
    fn strip(v: &Value, names: bool) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| names || (*k != "$schema" && *k != "nullable"))
                    .map(|(k, x)| (k.clone(), strip(x, !names && k == "properties")))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| strip(x, false)).collect()),
            other => other.clone(),
        }
    }
    match schema {
        Value::Object(_) => {
            let mut s = strip(schema, false);
            if s.get("type").is_none() {
                s["type"] = json!("object");
            }
            s
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// A non-object becomes `{}`; optional properties given as `""` or `{}` are dropped.
pub fn clean_args(args: &Value, schema: &Value) -> Value {
    let Value::Object(m) = args else { return json!({}) };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Value::Object(
        m.iter()
            .filter(|(k, v)| {
                required.contains(&k.as_str())
                    || !(v.as_str() == Some("") || v.as_object().is_some_and(|o| o.is_empty()))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

pub struct Converted {
    pub text: String,
    pub is_error: bool,
    pub images: Vec<Image>,
    pub details: Value,
}

/// A `tools/call` result in mu's terms.
pub fn convert_result(server: &str, tool: &str, result: &Value) -> Converted {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    // Index in `parts` of the first text block (omp prefixes that one on an error).
    let mut first_text: Option<usize> = None;
    // Text blocks that parse as JSON, to tell whether structured content is already in the text.
    let mut texts_json: Vec<Value> = Vec::new();
    let mut images = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("text") => {
                let t = item["text"].as_str().unwrap_or_default().to_string();
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    texts_json.push(v);
                }
                first_text.get_or_insert(parts.len());
                parts.push(t);
            }
            Some("image") => {
                if let (Some(data), Some(mime)) = (item["data"].as_str(), item["mimeType"].as_str()) {
                    images.push(mu_base::image::externalize(Image { data: data.into(), mime_type: mime.into() }));
                }
            }
            Some("audio") => parts.push(format!("[audio: {}]", item["mimeType"].as_str().unwrap_or("unknown"))),
            Some("resource") => {
                let r = &item["resource"];
                let mut s = format!("[Resource: {}]", r["uri"].as_str().unwrap_or_default());
                if let Some(t) = r["text"].as_str() {
                    s.push('\n');
                    s.push_str(t);
                }
                parts.push(s);
            }
            Some("resource_link") => parts.push(format!("[Resource: {}]", item["uri"].as_str().unwrap_or_default())),
            _ => {}
        }
    }
    if is_error {
        match first_text {
            Some(i) => parts[i] = format!("Error: {}", parts[i]),
            None => parts.insert(0, "Error:".into()),
        }
    }
    let mut text = parts.join("\n\n");
    if let Some(sc) = result.get("structuredContent").filter(|v| !v.is_null())
        && !texts_json.iter().any(|v| v == sc)
    {
        let pretty = serde_json::to_string_pretty(sc).unwrap_or_default();
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("```json\n{pretty}\n```"));
    }
    Converted {
        text,
        is_error,
        images,
        details: json!({"serverName": server, "mcpToolName": tool, "isError": is_error}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(tool_name("puppeteer", "puppeteer_screenshot"), "mcp__puppeteer_screenshot");
        assert_eq!(tool_name("My Server!", "Do-It"), "mcp__my_server_do_it");
        assert_eq!(tool_name("s3", "list2"), "mcp__s3_list2");
        assert_eq!(tool_name("", ""), "mcp__server_tool");
        assert_eq!(tool_name("__", "--"), "mcp__server_tool");
        // omp (Bun.hash(name).toString(36).slice(0, 8)): same suffix, so long names match across tools.
        let long = tool_name("srv", &"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert_eq!(long, format!("mcp__srv_{}_20lrqdja", "x".repeat(46)));
        assert_ne!(long, tool_name("srv", &format!("{}y", "x".repeat(100))));
    }

    #[test]
    fn wyhash_matches_bun_hash() {
        // Values printed by `Bun.hash(s)` for every length class of the algorithm.
        for (len, byte, want) in [
            (0usize, b'a', 290873116282709081u64),
            (20, b'a', 17884904682611351274),
            (48, b'b', 7352207450022303847),
            (49, b'c', 9307165568041630798),
            (100, b'd', 13268255729033370198),
            (150, b'e', 14363344178712070085),
        ] {
            assert_eq!(wyhash(&vec![byte; len]), want, "len {len}");
        }
        assert_eq!(wyhash(b"a"), 2941419223392617777);
        assert_eq!(wyhash(b"abc"), 190542993387777138);
    }

    #[test]
    fn schema_normalization() {
        let s = normalize_schema(
            &json!({"$schema":"x","properties":{"nullable":{"type":"string","nullable":true},"a":{"$schema":"y","type":"integer"}}}),
        );
        assert!(s.get("$schema").is_none());
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["nullable"].is_object(), "a property named nullable stays");
        assert!(s["properties"]["nullable"].get("nullable").is_none());
        assert!(s["properties"]["a"].get("$schema").is_none());
        assert_eq!(normalize_schema(&json!(null))["type"], "object");
    }

    #[test]
    fn arguments() {
        let schema = json!({"required":["r"]});
        assert_eq!(clean_args(&json!("x"), &schema), json!({}));
        assert_eq!(clean_args(&json!({"r":"","o":"","p":{},"k":0}), &schema), json!({"r":"","k":0}));
    }

    #[test]
    fn results() {
        let r = convert_result(
            "s",
            "t",
            &json!({"content":[
                {"type":"text","text":"a"},
                {"type":"resource","resource":{"uri":"file:///x","text":"body"}},
                {"type":"audio","mimeType":"audio/wav","data":"AA=="},
                {"type":"text","text":"b"}],
              "structuredContent":{"k":1}}),
        );
        assert_eq!(
            r.text,
            "a\n\n[Resource: file:///x]\nbody\n\n[audio: audio/wav]\n\nb\n\n```json\n{\n  \"k\": 1\n}\n```"
        );
        assert!(!r.is_error);
        assert_eq!(r.details, json!({"serverName":"s","mcpToolName":"t","isError":false}));

        let e = convert_result("s", "t", &json!({"isError":true,"content":[{"type":"text","text":"boom"}]}));
        assert!(e.is_error);
        assert_eq!(e.text, "Error: boom");

        let dup = convert_result(
            "s",
            "t",
            &json!({"content":[{"type":"text","text":"{\"k\":1}"}],"structuredContent":{"k":1}}),
        );
        assert_eq!(dup.text, "{\"k\":1}", "structured content already in the text is not repeated");
        let spaced = convert_result(
            "s",
            "t",
            &json!({"content":[{"type":"text","text":"{ \"k\": 1 }"}],"structuredContent":{"k":1}}),
        );
        assert_eq!(spaced.text, "{ \"k\": 1 }", "equal JSON is detected whatever the spacing");
        let none = convert_result("s", "t", &json!({"isError":true,"content":[]}));
        assert_eq!(none.text, "Error:");
        let second = convert_result(
            "s",
            "t",
            &json!({"isError":true,"content":[{"type":"resource_link","uri":"u"},{"type":"text","text":"bad"}]}),
        );
        assert_eq!(second.text, "[Resource: u]\n\nError: bad", "the first text block gets the prefix");
    }
}
