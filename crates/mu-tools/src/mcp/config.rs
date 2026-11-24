//! omp's `mcp.json` files: where they are, how entries merge, and what one entry means.
//! Pure: reads files and the environment, starts nothing.

use super::trust;
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
    /// omp's `auth` block (`{type, resource, …}`), when present.
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

    /// The protected resource whose OAuth token this server uses: an http server's url, or a stdio
    /// server's `auth.resource`. It keys the stored credential.
    pub fn oauth_key(&self) -> Option<String> {
        match &self.kind {
            Kind::Http { url, .. } => Some(url.clone()),
            Kind::Stdio { .. } => {
                if self.auth_type() != Some("oauth") {
                    return None;
                }
                let r = self.entry["auth"]["resource"].as_str()?;
                (r.starts_with("http://") || r.starts_with("https://")).then(|| r.to_string())
            }
        }
    }

    /// The OAuth settings the bridge reads from `MU_MCP_OAUTH`, as a JSON object; absent keys are omitted.
    pub fn oauth_env(&self) -> String {
        let (auth, oauth) = (&self.entry["auth"], &self.entry["oauth"]);
        let pick = |key: &str, from: &[&Value]| from.iter().find_map(|v| v.get(key).filter(|x| !x.is_null())).cloned();
        let mut out = serde_json::Map::new();
        let fields: [(&str, Option<Value>); 7] = [
            ("clientId", pick("clientId", &[oauth, auth])),
            ("clientSecret", pick("clientSecret", &[oauth, auth])),
            ("scope", pick("scope", &[oauth])),
            ("callbackPort", pick("callbackPort", &[oauth])),
            ("callbackPath", pick("callbackPath", &[oauth])),
            ("redirectUri", pick("redirectUri", &[oauth])),
            ("tokenUrl", pick("tokenUrl", &[auth])),
        ];
        for (k, v) in fields {
            if let Some(v) = v {
                out.insert(k.into(), v);
            }
        }
        Value::Object(out).to_string()
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

pub(super) fn as_bool(v: &Value) -> Option<bool> {
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
pub(super) fn parse_entry(name: &str, e: &Value) -> Result<Option<McpServer>, String> {
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
pub(super) fn connection_key(s: &McpServer) -> Value {
    let kind = match &s.kind {
        Kind::Stdio { command, args, env, cwd } => json!({"command": command, "args": args, "env": env, "cwd": cwd}),
        Kind::Http { url, headers } => json!({"url": url, "headers": headers}),
    };
    json!({"kind": kind, "auth": s.auth})
}

/// pi's user-level `mcp.json`. It rides along with omp's real agent dir, so a caller that names some other agent
/// dir (a test, say) never reads the developer's `~/.pi`; `None` too when pi's dir is omp's (read once).
fn pi_user_file(agent: &Path) -> Option<PathBuf> {
    let pi = mu_common::paths::pi_agent_dir();
    (agent == mu_common::paths::omp_agent_dir() && pi != agent).then(|| pi.join("mcp.json"))
}

/// Whether `file` is one of pi's own (read-only for mu, and its `timeout` counts seconds, not milliseconds).
pub fn is_pi_file(cwd: &Path, agent: &Path, file: &Path) -> bool {
    file == cwd.join(".pi/mcp.json") || pi_user_file(agent).as_deref() == Some(file)
}

/// pi's entries differ from omp's in one field: `timeout` is in seconds. Its `!command` values (env, headers)
/// are not run, and an entry that needs one is refused rather than sent literally.
fn pi_entry(entry: Value) -> Result<Value, String> {
    let Value::Object(mut o) = entry else { return Ok(entry) };
    for key in ["env", "headers"] {
        if let Some(Value::Object(m)) = o.get(key)
            && let Some(k) = m.iter().find(|(_, v)| v.as_str().is_some_and(|s| s.starts_with('!'))).map(|(k, _)| k)
        {
            return Err(format!("`{key}.{k}` is a `!command` value, which mu does not run"));
        }
    }
    if let Some(t) = o.get("timeout").and_then(Value::as_f64) {
        o.insert("timeout".into(), json!((t * 1000.0).round() as u64));
    }
    Ok(Value::Object(o))
}

pub fn candidate_files(cwd: &Path, agent: &Path) -> Vec<PathBuf> {
    let mut files = vec![cwd.join(".omp/mcp.json"), cwd.join(".omp/.mcp.json"), cwd.join(".pi/mcp.json")];
    files.extend([agent.join("mcp.json"), agent.join(".mcp.json")]);
    files.extend(pi_user_file(agent));
    files.extend([cwd.join("mcp.json"), cwd.join(".mcp.json")]);
    files
}

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// A configured server whether or not it is switched on, and the file that defines it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub server: McpServer,
    pub enabled: bool,
    pub file: PathBuf,
    /// False for a project file's servers the user has not trusted yet: nothing of them was expanded.
    pub trusted: bool,
}

/// Every server for a project, in file order, including the ones switched off, plus warnings.
/// `get_env` serves `${VAR}`.
pub fn load_all_with(cwd: &Path, agent: &Path, get_env: &dyn Fn(&str) -> Option<String>) -> (Vec<Entry>, Vec<String>) {
    let mut warnings = Vec::new();
    let user_file = agent.join("mcp.json");
    let (mut disabled, mut enabled) = (Vec::new(), Vec::new());
    if let Some(Value::Object(root)) = read_json(&user_file, &mut Vec::new()) {
        disabled = names(root.get("disabledServers"));
        enabled = names(root.get("enabledServers"));
    }
    let mut claimed: Vec<String> = Vec::new();
    let mut out: Vec<Entry> = Vec::new();
    let mut keys: Vec<Value> = Vec::new();
    let mut agent_files = vec![agent.join("mcp.json"), agent.join(".mcp.json")];
    agent_files.extend(pi_user_file(agent));
    for file in candidate_files(cwd, agent) {
        let pi = is_pi_file(cwd, agent, &file);
        let Some(Value::Object(root)) = read_json(&file, &mut warnings) else { continue };
        let Some(mcp) = root.get("mcpServers") else { continue };
        let Value::Object(servers) = mcp else { continue };
        let project = trust::is_project_file(cwd, &file) && !agent_files.contains(&file);
        let trusted = !project || trust::is_trusted(&file, &trust::digest(mcp));
        for (name, raw) in servers {
            if claimed.contains(name) {
                continue;
            }
            claimed.push(name.clone());
            let entry = if trusted { expand_value(raw, get_env) } else { raw.clone() };
            let entry = if pi {
                match pi_entry(entry) {
                    Ok(e) => e,
                    Err(why) => {
                        warnings.push(format!("mcp {name}: skipped ({why}) in {}", file.display()));
                        continue;
                    }
                }
            } else {
                entry
            };
            let on = entry.get("enabled").and_then(as_bool).unwrap_or(true);
            let on = if disabled.contains(name) { false } else { on || enabled.contains(name) };
            match parse_entry(name, &entry) {
                Err(why) => warnings.push(format!("mcp {name}: skipped ({why}) in {}", file.display())),
                Ok(None) => warnings.push(format!(
                    "mcp {name}: skipped (legacy SSE servers are not supported; use type \"http\" if the server offers Streamable HTTP)"
                )),
                Ok(Some(mut s)) => {
                    s.raw_url = raw.get("url").and_then(Value::as_str).map(String::from);
                    if trusted && matches!(s.kind, Kind::Stdio { .. }) && s.wants_oauth() && s.oauth_key().is_none() {
                        warnings.push(format!(
                            "mcp server `{name}`: OAuth on a stdio server needs `auth.resource`, the URL of its protected resource; starting it without a token"
                        ));
                    }
                    let key = connection_key(&s);
                    if keys.contains(&key) {
                        continue;
                    }
                    keys.push(key);
                    out.push(Entry { server: s, enabled: on, file: file.clone(), trusted });
                }
            }
        }
    }
    (out, warnings)
}

/// The trusted servers that are switched on for a project, in file order, plus warnings.
pub fn load_with(cwd: &Path, agent: &Path, get_env: &dyn Fn(&str) -> Option<String>) -> (Vec<McpServer>, Vec<String>) {
    let (all, mut warnings) = load_all_with(cwd, agent, get_env);
    let mut held: Vec<(PathBuf, usize)> = Vec::new();
    for e in all.iter().filter(|e| !e.trusted) {
        match held.iter_mut().find(|(f, _)| *f == e.file) {
            Some((_, n)) => *n += 1,
            None => held.push((e.file.clone(), 1)),
        }
    }
    for (file, n) in held {
        warnings.push(format!(
            "mcp: {n} server(s) in {} not started: this project's MCP config is not trusted yet (in mu: /mcp trust)",
            file.display()
        ));
    }
    (all.into_iter().filter(|e| e.enabled && e.trusted).map(|e| e.server).collect(), warnings)
}

/// The project files that define servers not yet trusted: each file, its digest and its server names.
pub fn untrusted_files(cwd: &Path, agent: &Path) -> Vec<(PathBuf, String, Vec<String>)> {
    let (all, _) = load_all_with(cwd, agent, &|_| None);
    let mut out: Vec<(PathBuf, String, Vec<String>)> = Vec::new();
    for e in all.iter().filter(|e| !e.trusted) {
        if let Some((_, _, names)) = out.iter_mut().find(|(f, _, _)| *f == e.file) {
            names.push(e.server.name.clone());
            continue;
        }
        let Some(Value::Object(root)) = read_json(&e.file, &mut Vec::new()) else { continue };
        let digest = trust::digest(root.get("mcpServers").unwrap_or(&Value::Null));
        out.push((e.file.clone(), digest, vec![e.server.name.clone()]));
    }
    out
}

pub fn load_all(cwd: &Path, agent: &Path) -> (Vec<Entry>, Vec<String>) {
    load_all_with(cwd, agent, &|k| std::env::var(k).ok())
}

pub fn load(cwd: &Path, agent: &Path) -> (Vec<McpServer>, Vec<String>) {
    load_with(cwd, agent, &|k| std::env::var(k).ok())
}

fn read_json(path: &Path, warnings: &mut Vec<String>) -> Option<Value> {
    match super::read_json(path) {
        Ok(v) => Some(v),
        Err(super::JsonError::Io(_)) => None,
        Err(super::JsonError::Blank(e) | super::JsonError::Parse(e)) => {
            warnings.push(format!("mcp: {}: {e}", path.display()));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::tmp;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn env(k: &str) -> Option<String> {
        match k {
            "HOME_X" => Some("/h".into()),
            "SECRET" => Some("s3cr3t".into()),
            _ => None,
        }
    }

    /// Trusts every untrusted project file as it stands, the way the user's `/mcp trust` does.
    fn trust_all(cwd: &Path, agent: &Path) {
        mu_testutil::home();
        for (file, digest, _) in untrusted_files(cwd, agent) {
            trust::trust(&file, &digest).unwrap();
        }
    }

    #[test]
    fn project_servers_wait_for_trust() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("proj"), d.join("agent"));
        let file = cwd.join(".omp/mcp.json");
        write(&file, r#"{"mcpServers":{"p":{"command":"x","args":["${SECRET}"]}}}"#);
        // Untrusted: nothing is read from the environment, so this stub must not be called.
        let never = |k: &str| -> Option<String> { panic!("${k} expanded for an untrusted file") };
        let (all, _) = load_all_with(&cwd, &agent, &never);
        assert_eq!(all.len(), 1);
        assert!(!all[0].trusted);
        assert_eq!(
            all[0].server.kind,
            Kind::Stdio { command: "x".into(), args: vec!["${SECRET}".into()], env: vec![], cwd: None }
        );
        let (s, w) = load_with(&cwd, &agent, &never);
        assert!(s.is_empty());
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("1 server(s)") && w[0].contains("not trusted") && w[0].contains("/mcp trust"), "{w:?}");

        let (files, digest, names) = untrusted_files(&cwd, &agent).remove(0);
        assert_eq!((files.as_path(), names.as_slice()), (file.as_path(), ["p".to_string()].as_slice()));
        trust::trust(&file, &digest).unwrap();
        let (s, w) = load_with(&cwd, &agent, &env);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, Kind::Stdio { command: "x".into(), args: vec!["s3cr3t".into()], env: vec![], cwd: None });
        assert!(untrusted_files(&cwd, &agent).is_empty());

        // A changed server set is untrusted again, until it is trusted again.
        write(&file, r#"{"mcpServers":{"p":{"command":"x","args":["${SECRET}"]},"q":{"url":"http://q"}}}"#);
        let (all, _) = load_all_with(&cwd, &agent, &env);
        assert!(all.iter().all(|e| !e.trusted), "{all:?}");
        assert_eq!(untrusted_files(&cwd, &agent)[0].2, vec!["p".to_string(), "q".to_string()]);
        let (s, _) = load_with(&cwd, &agent, &env);
        assert!(s.is_empty());
        trust_all(&cwd, &agent);
        assert_eq!(load_with(&cwd, &agent, &env).0.len(), 2);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn agent_files_are_always_trusted() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        write(&agent.join("mcp.json"), r#"{"mcpServers":{"u":{"command":"x","args":["${SECRET}"]}}}"#);
        let (all, _) = load_all_with(&cwd, &agent, &env);
        assert!(all[0].trusted);
        assert_eq!(load_with(&cwd, &agent, &env).0.len(), 1);
        assert!(untrusted_files(&cwd, &agent).is_empty());
        let _ = std::fs::remove_dir_all(d);
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
        trust_all(&cwd, &agent);
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
            r#"{"mcpServers":{"h":{"url":"http://h","timeout":"0","requestIdFormat":"string","instructions":"false","auth":{"type":"oauth","clientId":"cid"},"oauth":{"scope":"a b","callbackPort":8123}},"s":{"command":"c","env":{"A":"b","N":1},"cwd":"/w"}}}"#,
        );
        let (s, w) = load_with(&cwd, &agent, &env);
        assert!(w.is_empty(), "{w:?}");
        let h = &s[0];
        assert_eq!(h.timeout_ms, Some(0));
        assert!(h.string_ids && !h.instructions && h.wants_oauth());
        assert_eq!(h.oauth_key().as_deref(), Some("http://h"));
        let env: Value = serde_json::from_str(&h.oauth_env()).unwrap();
        assert_eq!(env, json!({"clientId": "cid", "scope": "a b", "callbackPort": 8123}));
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
    fn a_stdio_server_is_keyed_by_its_oauth_resource() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        write(
            &agent.join("mcp.json"),
            r#"{"mcpServers":{"a":{"command":"c","auth":{"type":"oauth","resource":"https://r/mcp"}},"b":{"command":"c","auth":{"type":"oauth","resource":"nope"}},"c":{"command":"c","auth":{"resource":"https://r"}}}}"#,
        );
        let (s, w) = load_with(&cwd, &agent, &env);
        assert_eq!(s[0].oauth_key().as_deref(), Some("https://r/mcp"));
        assert_eq!(s[1].oauth_key(), None);
        assert_eq!(s[2].oauth_key(), None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`b`") && w[0].contains("auth.resource"), "{w:?}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_pi_project_file_is_gated_counts_seconds_and_refuses_bang_commands() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        let file = cwd.join(".pi/mcp.json");
        write(
            &file,
            r#"{"mcpServers":{"slow":{"url":"http://s","timeout":90},"bang":{"command":"c","env":{"T":"!pass show t"}},"ok":{"command":"x"}}}"#,
        );
        assert!(is_pi_file(&cwd, &agent, &file));
        let (all, w) = load_all_with(&cwd, &agent, &env);
        assert!(all.iter().all(|e| !e.trusted), "a project file waits for trust");
        assert!(w.iter().any(|w| w.contains("mcp bang") && w.contains("!command")), "{w:?}");
        trust_all(&cwd, &agent);
        let (s, _) = load_with(&cwd, &agent, &env);
        let names: Vec<_> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["slow", "ok"]);
        assert_eq!(s[0].timeout_ms, Some(90_000));
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
