//! omp's `mcp.json` files: where they are, how entries merge, and what one entry means.
//! Pure: reads files and the environment, starts nothing.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Stdio { command: String, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<String> },
    Http { url: String, headers: Vec<(String, String)> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub kind: Kind,
    /// omp's `auth` block (`{type, credentialId}`), when present.
    pub auth: Option<Value>,
    /// Milliseconds; `Some(0)` means no timeout, `None` means the default (30 s).
    pub timeout_ms: Option<u64>,
    /// `instructions: false` turns the server's instructions off.
    pub instructions: bool,
    /// `requestIdFormat: "string"`: ids go out as strings.
    pub string_ids: bool,
    /// The entry after `${VAR}` expansion, the input of the config hash.
    pub entry: Value,
    /// The url as written, before `${VAR}` expansion (omp also looks a credential up by it).
    pub raw_url: Option<String>,
}

impl McpServer {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            Kind::Stdio { .. } => "stdio",
            Kind::Http { .. } => "http",
        }
    }

    pub fn url(&self) -> Option<&str> {
        match &self.kind {
            Kind::Http { url, .. } => Some(url),
            Kind::Stdio { .. } => None,
        }
    }

    pub fn auth_type(&self) -> Option<&str> {
        self.auth.as_ref()?.get("type")?.as_str()
    }

    pub fn credential_id(&self) -> Option<&str> {
        self.auth.as_ref()?.get("credentialId")?.as_str().filter(|s| !s.is_empty())
    }

    /// Whether this server is known to need a stored OAuth credential.
    pub fn wants_oauth(&self) -> bool {
        self.auth_type() == Some("oauth")
    }
}

/// `${VAR}` and `${VAR:-default}`; an unset variable without a default expands to nothing.
pub fn expand(s: &str, get: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let body = &after[..end];
        let (name, default) = match body.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (body, None),
        };
        match get(name).filter(|v| !v.is_empty()) {
            Some(v) => out.push_str(&v),
            None => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn expand_value(v: &Value, get: &dyn Fn(&str) -> Option<String>) -> Value {
    match v {
        Value::String(s) => Value::String(expand(s, get)),
        Value::Array(a) => Value::Array(a.iter().map(|x| expand_value(x, get)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), expand_value(x, get))).collect()),
        other => other.clone(),
    }
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn as_millis(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn string_map(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str().map(String::from).or_else(|| scalar(v))?)))
            .collect(),
        _ => Vec::new(),
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// One entry, already expanded. `Ok(None)`: a legacy SSE server (skipped with a warning by the caller).
fn parse_entry(name: &str, e: &Value) -> Result<Option<McpServer>, String> {
    let Value::Object(o) = e else { return Err("not an object".into()) };
    let ty = o.get("type").and_then(Value::as_str);
    let command = o.get("command").and_then(Value::as_str).filter(|s| !s.is_empty());
    let url = o.get("url").and_then(Value::as_str).filter(|s| !s.is_empty());
    let ty = match (ty, command, url) {
        (Some(t), _, _) => t,
        (None, Some(_), _) => "stdio",
        (None, None, Some(_)) => "http",
        _ => return Err("needs `command` or `url`".into()),
    };
    let kind = match ty {
        "stdio" => {
            let command = command.ok_or("stdio server without `command`")?.to_string();
            let args = match o.get("args") {
                Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                _ => Vec::new(),
            };
            let cwd = o.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            Kind::Stdio { command, args, env: string_map(o.get("env")), cwd }
        }
        "http" => Kind::Http {
            url: url.ok_or("http server without `url`")?.to_string(),
            headers: string_map(o.get("headers")),
        },
        "sse" => return Ok(None),
        other => return Err(format!("unknown type `{other}`")),
    };
    let auth = match o.get("auth") {
        Some(a @ Value::Object(_)) => Some(a.clone()),
        _ => None,
    };
    Ok(Some(McpServer {
        name: name.into(),
        kind,
        auth,
        timeout_ms: o.get("timeout").and_then(as_millis),
        instructions: o.get("instructions").and_then(as_bool).unwrap_or(true),
        string_ids: o.get("requestIdFormat").and_then(Value::as_str) == Some("string"),
        entry: e.clone(),
        raw_url: None,
    }))
}

/// What makes two entries "the same connection" (omp's dedupe key).
fn connection_key(s: &McpServer) -> Value {
    let kind = match &s.kind {
        Kind::Stdio { command, args, env, cwd } => json!({"command": command, "args": args, "env": env, "cwd": cwd}),
        Kind::Http { url, headers } => json!({"url": url, "headers": headers}),
    };
    json!({"kind": kind, "auth": s.auth})
}

fn candidate_files(cwd: &Path, agent: &Path) -> Vec<PathBuf> {
    vec![
        cwd.join(".omp/mcp.json"),
        cwd.join(".omp/.mcp.json"),
        agent.join("mcp.json"),
        agent.join(".mcp.json"),
        cwd.join("mcp.json"),
        cwd.join(".mcp.json"),
    ]
}

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// The servers for a project, in file order, plus warnings. `get_env` serves `${VAR}`.
pub fn load_with(cwd: &Path, agent: &Path, get_env: &dyn Fn(&str) -> Option<String>) -> (Vec<McpServer>, Vec<String>) {
    let mut warnings = Vec::new();
    let user_file = agent.join("mcp.json");
    let (mut disabled, mut enabled) = (Vec::new(), Vec::new());
    if let Some(Value::Object(root)) = read_json(&user_file, &mut Vec::new()) {
        disabled = names(root.get("disabledServers"));
        enabled = names(root.get("enabledServers"));
    }
    let mut claimed: Vec<String> = Vec::new();
    let mut out: Vec<McpServer> = Vec::new();
    let mut keys: Vec<Value> = Vec::new();
    for file in candidate_files(cwd, agent) {
        let Some(Value::Object(root)) = read_json(&file, &mut warnings) else { continue };
        let Some(Value::Object(servers)) = root.get("mcpServers") else { continue };
        for (name, raw) in servers {
            if claimed.contains(name) {
                continue;
            }
            claimed.push(name.clone());
            let entry = expand_value(raw, get_env);
            let on = entry.get("enabled").and_then(as_bool).unwrap_or(true);
            let on = if disabled.contains(name) { false } else { on || enabled.contains(name) };
            match parse_entry(name, &entry) {
                Err(why) => warnings.push(format!("mcp {name}: skipped ({why}) in {}", file.display())),
                Ok(None) => warnings.push(format!(
                    "mcp {name}: skipped (legacy SSE servers are not supported; use type \"http\" if the server offers Streamable HTTP)"
                )),
                Ok(Some(mut s)) => {
                    s.raw_url = raw.get("url").and_then(Value::as_str).map(String::from);
                    let key = connection_key(&s);
                    if keys.contains(&key) {
                        continue;
                    }
                    keys.push(key);
                    if on {
                        out.push(s);
                    }
                }
            }
        }
    }
    (out, warnings)
}

pub fn load(cwd: &Path, agent: &Path) -> (Vec<McpServer>, Vec<String>) {
    load_with(cwd, agent, &|k| std::env::var(k).ok())
}

fn read_json(path: &Path, warnings: &mut Vec<String>) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(format!("mcp: {}: {e}", path.display()));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-mcpcfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn env(k: &str) -> Option<String> {
        match k {
            "HOME_X" => Some("/h".into()),
            _ => None,
        }
    }

    #[test]
    fn expansion() {
        assert_eq!(expand("a${HOME_X}b", &env), "a/hb");
        assert_eq!(expand("${NOPE:-dflt}", &env), "dflt");
        assert_eq!(expand("${NOPE}x", &env), "x");
        assert_eq!(expand("${unclosed", &env), "${unclosed");
    }

    #[test]
    fn precedence_merge_and_dedupe() {
        let d = tmp();
        let (cwd, agent) = (d.join("proj"), d.join("agent"));
        write(
            &cwd.join(".omp/mcp.json"),
            r#"{"mcpServers":{"a":{"command":"x","args":["1"]},"gone":{"command":"y","enabled":"false"}}}"#,
        );
        write(
            &agent.join("mcp.json"),
            r#"{"mcpServers":{"a":{"command":"other"},"b":{"command":"x","args":["1"]},"gone":{"command":"z"},"h":{"url":"https://e/${HOME_X}","headers":{"K":"v"}}},"disabledServers":["h"]}"#,
        );
        write(
            &cwd.join(".mcp.json"),
            r#"{"mcpServers":{"c":{"command":"c","enabled":0},"d":{"type":"sse","url":"u"}}}"#,
        );
        let (s, w) = load_with(&cwd, &agent, &env);
        let names: Vec<_> = s.iter().map(|s| s.name.as_str()).collect();
        // `a` from the project wins; `b` duplicates `a`'s connection; `gone` is disabled and keeps its name;
        // `h` is on the deny list; `c` is disabled; `d` is SSE.
        assert_eq!(names, vec!["a"], "{w:?}");
        assert!(w.iter().any(|w| w.contains("mcp d") && w.contains("SSE")), "{w:?}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn allow_list_overrides_enabled_false_and_deny_wins() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        write(
            &agent.join("mcp.json"),
            r#"{"mcpServers":{"x":{"command":"x","enabled":false},"y":{"url":"http://y","timeout":"0"}},"enabledServers":["x","y"],"disabledServers":["y"]}"#,
        );
        let (s, _) = load_with(&cwd, &agent, &env);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "x");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn fields_and_coercion() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        write(
            &agent.join("mcp.json"),
            r#"{"mcpServers":{"h":{"url":"http://h","timeout":"0","requestIdFormat":"string","instructions":"false","auth":{"type":"oauth","credentialId":"mcp_oauth_1"}},"s":{"command":"c","env":{"A":"b","N":1},"cwd":"/w"}}}"#,
        );
        let (s, w) = load_with(&cwd, &agent, &env);
        assert!(w.is_empty(), "{w:?}");
        let h = &s[0];
        assert_eq!(h.timeout_ms, Some(0));
        assert!(h.string_ids && !h.instructions && h.wants_oauth());
        assert_eq!(h.credential_id(), Some("mcp_oauth_1"));
        match &s[1].kind {
            Kind::Stdio { env, cwd, .. } => {
                assert_eq!(env, &vec![("A".to_string(), "b".to_string()), ("N".to_string(), "1".to_string())]);
                assert_eq!(cwd.as_deref(), Some("/w"));
            }
            _ => panic!(),
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn bad_entries_warn() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        write(&agent.join("mcp.json"), r#"{"mcpServers":{"n":{},"t":{"type":"wat","command":"x"}}}"#);
        let (s, w) = load_with(&cwd, &agent, &env);
        assert!(s.is_empty());
        assert_eq!(w.len(), 2, "{w:?}");
        let _ = std::fs::remove_dir_all(d);
    }
}
