//! MCP servers that other tools have configured on this machine, converted to omp's `mcp.json`
//! entry format so they can be copied into mu's own config. Read-only: nothing here writes.

use super::config::{Entry, Kind, connection_key, parse_entry};
use super::store::check_name;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Where to look.
#[derive(Debug, Clone)]
pub struct Roots {
    pub home: PathBuf,
    pub cwd: PathBuf,
    /// The agent directory in use (its `mcp.json` is mu's own config and is never a source).
    pub agent_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// Human label of the source: `Claude Code`, `omp (profile work)`, `Codex`, ...
    pub tool: String,
    pub file: PathBuf,
    pub name: String,
    /// Converted to omp's format, with an explicit `type` (`stdio` or `http`).
    pub entry: Value,
    /// `stdio` or `http`.
    pub kind: &'static str,
    /// What it runs or talks to, safe to display: the command, or `scheme://host` of the url.
    pub target: String,
}

type Servers = Vec<(String, Value)>;

struct Source {
    /// Position of the tool in the documented source order.
    group: u8,
    tool: String,
    file: PathBuf,
    servers: Servers,
}

/// Every importable server found under `roots`, minus the ones `configured` already has (same
/// name, or same connection). Sorted by tool, then name. Unreadable or malformed files are
/// skipped silently; servers mu cannot run (legacy SSE) are left out.
pub fn scan(roots: &Roots, configured: &[Entry]) -> Vec<Found> {
    let names: Vec<&str> = configured.iter().map(|e| e.server.name.as_str()).collect();
    let mut keys: Vec<Value> = configured.iter().map(|e| connection_key(&e.server)).collect();
    let mut found: Vec<(u8, Found)> = Vec::new();
    for src in sources(roots) {
        for (raw_name, entry) in src.servers {
            let Some(name) = sanitize(&raw_name) else { continue };
            if names.contains(&name.as_str()) {
                continue;
            }
            let Ok(Some(server)) = parse_entry(&name, &entry) else { continue };
            let key = connection_key(&server);
            if keys.contains(&key) {
                continue;
            }
            keys.push(key);
            let (kind, target) = match &server.kind {
                Kind::Stdio { command, .. } => ("stdio", command.clone()),
                Kind::Http { url, .. } => ("http", http_target(url)),
            };
            found
                .push((src.group, Found { tool: src.tool.clone(), file: src.file.clone(), name, entry, kind, target }));
        }
    }
    found.sort_by(|a, b| (a.0, &a.1.tool, &a.1.name).cmp(&(b.0, &b.1.tool, &b.1.name)));
    found.into_iter().map(|(_, f)| f).collect()
}

fn sanitize(name: &str) -> Option<String> {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .take(48)
        .collect();
    check_name(&s).ok().map(|_| s)
}

/// `scheme://host[:port]`: userinfo, path, query and fragment may carry credentials.
fn http_target(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match scheme {
        Some(s) => format!("{s}://{host}"),
        None => host.to_string(),
    }
}

struct Collector {
    own: PathBuf,
    items: Vec<Source>,
}

impl Collector {
    fn add(
        &mut self,
        group: u8,
        tool: &str,
        file: PathBuf,
        load: fn(&Path) -> Option<Value>,
        read: &dyn Fn(&Value) -> Servers,
    ) {
        if same_file(&file, &self.own) {
            return;
        }
        let Some(root) = load(&file) else { return };
        let servers = read(&root);
        if !servers.is_empty() {
            self.items.push(Source { group, tool: tool.to_string(), file, servers });
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// In the order the tools are documented: the first source of a connection wins.
fn sources(roots: &Roots) -> Vec<Source> {
    let home = &roots.home;
    let mut c = Collector { own: roots.agent_dir.join("mcp.json"), items: Vec::new() };
    let mcp_servers = |root: &Value| map_servers(root, "mcpServers", std_entry);

    c.add(0, "omp", home.join(".omp/agent/mcp.json"), load_json, &mcp_servers);
    let mut profiles: Vec<String> = std::fs::read_dir(home.join(".omp/profiles"))
        .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    profiles.sort();
    for p in profiles {
        let file = home.join(".omp/profiles").join(&p).join("agent/mcp.json");
        c.add(1, &format!("omp (profile {p})"), file, load_json, &mcp_servers);
    }
    c.add(2, "pi", home.join(".pi/agent/mcp.json"), load_json, &mcp_servers);

    let claude = home.join(".claude.json");
    c.add(3, "Claude Code", claude.clone(), load_json, &mcp_servers);
    let cwd = roots.cwd.to_string_lossy().into_owned();
    c.add(4, "Claude Code (this project)", claude, load_json, &|root| {
        let project = root.get("projects").and_then(|p| p.get(cwd.as_str())).unwrap_or(&Value::Null);
        map_servers(project, "mcpServers", std_entry)
    });

    for rel in
        ["Library/Application Support/Claude/claude_desktop_config.json", ".config/Claude/claude_desktop_config.json"]
    {
        c.add(5, "Claude Desktop", home.join(rel), load_json, &mcp_servers);
    }
    c.add(6, "Codex", home.join(".codex/config.toml"), load_toml, &|root| {
        map_servers(root, "mcp_servers", codex_entry)
    });
    c.add(7, "Gemini CLI", home.join(".gemini/settings.json"), load_json, &|root| {
        map_servers(root, "mcpServers", gemini_entry)
    });
    for file in [home.join(".config/opencode/opencode.json"), roots.cwd.join("opencode.json")] {
        c.add(8, "OpenCode", file, load_json, &|root| map_servers(root, "mcp", opencode_entry));
    }
    c.items
}

fn load_json(path: &Path) -> Option<Value> {
    super::read_json(path).ok()
}

fn load_toml(path: &Path) -> Option<Value> {
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    serde_json::to_value(table).ok()
}

fn map_servers(root: &Value, key: &str, convert: fn(&Value) -> Option<Value>) -> Servers {
    let Some(Value::Object(m)) = root.get(key) else { return Vec::new() };
    m.iter().filter_map(|(name, e)| Some((name.clone(), convert(e)?))).collect()
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from)
}

fn stringify(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(stringify).collect(),
        _ => Vec::new(),
    }
}

fn pairs(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Object(m)) => m.iter().filter_map(|(k, v)| Some((k.clone(), stringify(v)?))).collect(),
        _ => Vec::new(),
    }
}

fn string_object(pairs: Vec<(String, String)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k, Value::String(v))).collect())
}

fn stdio(command: &str, args: Vec<String>, env: Vec<(String, String)>, cwd: Option<String>) -> Value {
    let mut o = Map::new();
    o.insert("type".into(), "stdio".into());
    o.insert("command".into(), command.into());
    if !args.is_empty() {
        o.insert("args".into(), args.into());
    }
    if !env.is_empty() {
        o.insert("env".into(), string_object(env));
    }
    if let Some(cwd) = cwd {
        o.insert("cwd".into(), cwd.into());
    }
    Value::Object(o)
}

fn http(url: &str, headers: Vec<(String, String)>) -> Value {
    let mut o = Map::new();
    o.insert("type".into(), "http".into());
    o.insert("url".into(), url.into());
    if !headers.is_empty() {
        o.insert("headers".into(), string_object(headers));
    }
    Value::Object(o)
}

/// omp, pi, Claude Code and Claude Desktop: `{type?, command, args, env, cwd, url, headers}`.
fn std_entry(e: &Value) -> Option<Value> {
    let o = e.as_object()?;
    let (command, url) = (text(o.get("command")), text(o.get("url")));
    let ty = match o.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None if command.is_some() => "stdio",
        None if url.is_some() => "http",
        None => return None,
    };
    match ty {
        "stdio" => Some(stdio(&command?, strings(o.get("args")), pairs(o.get("env")), text(o.get("cwd")))),
        "http" => Some(http(&url?, pairs(o.get("headers")))),
        _ => None,
    }
}

fn codex_entry(e: &Value) -> Option<Value> {
    let o = e.as_object()?;
    if o.contains_key("command") {
        return std_entry(e);
    }
    let mut headers = pairs(o.get("http_headers"));
    if let Some(var) = text(o.get("bearer_token_env_var"))
        && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    {
        headers.push(("Authorization".into(), format!("Bearer ${{{var}}}")));
    }
    Some(http(&text(o.get("url"))?, headers))
}

/// A bare `url` is a legacy SSE server.
fn gemini_entry(e: &Value) -> Option<Value> {
    let o = e.as_object()?;
    if let Some(command) = text(o.get("command")) {
        return Some(stdio(&command, strings(o.get("args")), pairs(o.get("env")), text(o.get("cwd"))));
    }
    Some(http(&text(o.get("httpUrl"))?, pairs(o.get("headers"))))
}

fn opencode_entry(e: &Value) -> Option<Value> {
    let o = e.as_object()?;
    match o.get("type").and_then(Value::as_str)? {
        "local" => {
            let mut argv = strings(o.get("command")).into_iter();
            let command = argv.next().filter(|c| !c.is_empty())?;
            Some(stdio(&command, argv.collect(), pairs(o.get("environment")), None))
        }
        "remote" => Some(http(&text(o.get("url"))?, pairs(o.get("headers")))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::tmp;
    use serde_json::json;

    fn put(base: &Path, rel: &str, content: &str) {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn roots(base: &Path) -> Roots {
        let cwd = base.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        Roots { home: base.join("home"), cwd, agent_dir: base.join("home/.mu/agent") }
    }

    fn configured(name: &str, entry: Value) -> Entry {
        Entry { server: parse_entry(name, &entry).unwrap().unwrap(), enabled: true, file: PathBuf::new() }
    }

    fn fixtures(base: &Path, r: &Roots) {
        put(
            base,
            "home/.omp/agent/mcp.json",
            r#"{"mcpServers":{
                "fs":{"command":"npx","args":["-y","@x/fs"],"env":{"A":"1"}},
                "remote":{"url":"https://api.example.com:8443/mcp/secret?k=1","headers":{"X":"y"}}}}"#,
        );
        put(
            base,
            "home/.omp/profiles/work/agent/mcp.json",
            r#"{"mcpServers":{"work-db":{"type":"stdio","command":"db-mcp","enabled":false}}}"#,
        );
        put(base, "home/.pi/agent/mcp.json", r#"{"mcpServers":{"pi-one":{"command":"pi-mcp"}}}"#);
        let mut projects = Map::new();
        projects.insert(
            r.cwd.to_string_lossy().into_owned(),
            json!({"mcpServers":{"proj":{"type":"http","url":"https://u:p@proj.example.com/mcp"}}}),
        );
        projects.insert("/other".into(), json!({"mcpServers":{"nope":{"command":"x"}}}));
        let claude = json!({
            "mcpServers":{
                "cc":{"type":"stdio","command":"cc-mcp","args":["--token","sekret"]},
                "old":{"type":"sse","url":"https://sse.example.com/sse"},
                "fs-copy":{"command":"npx","args":["-y","@x/fs"],"env":{"A":"1"}}},
            "projects": projects});
        put(base, "home/.claude.json", &claude.to_string());
        put(
            base,
            "home/Library/Application Support/Claude/claude_desktop_config.json",
            r#"{"mcpServers":{"desk":{"command":"desk-mcp","env":{"N":5,"B":true}}}}"#,
        );
        put(
            base,
            "home/.codex/config.toml",
            "[mcp_servers.cx]\ncommand = \"cx\"\nargs = [\"a\"]\n[mcp_servers.cx.env]\nK = \"v\"\n\
             [mcp_servers.cxweb]\nurl = \"https://cx.example.com/mcp\"\nbearer_token_env_var = \"CX_TOKEN\"\n\
             [mcp_servers.cxweb.http_headers]\n\"X-A\" = \"b\"\n",
        );
        put(
            base,
            "home/.gemini/settings.json",
            r#"{"mcpServers":{
                "gem":{"command":"gem","cwd":"/tmp"},
                "gemhttp":{"httpUrl":"https://gem.example.com/mcp","headers":{"A":"b"}},
                "gemsse":{"url":"https://gem.example.com/sse"}}}"#,
        );
        put(
            base,
            "home/.config/opencode/opencode.json",
            r#"{"mcp":{
                "oc":{"type":"local","command":["oc-bin","--flag"],"environment":{"E":"1"}},
                "ocr":{"type":"remote","url":"https://oc.example.com/mcp","headers":{"H":"v"}}}}"#,
        );
        put(base, "proj/opencode.json", r#"{"mcp":{"ocp":{"type":"local","command":["ocp"]}}}"#);
    }

    fn tool_names(found: &[Found]) -> Vec<(&str, &str)> {
        found.iter().map(|f| (f.tool.as_str(), f.name.as_str())).collect()
    }

    fn get<'a>(found: &'a [Found], name: &str) -> &'a Found {
        found.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("{name} not found"))
    }

    #[test]
    fn finds_every_source_in_order() {
        let base = tmp();
        let r = roots(&base);
        fixtures(&base, &r);
        let found = scan(&r, &[]);
        assert_eq!(
            tool_names(&found),
            [
                ("omp", "fs"),
                ("omp", "remote"),
                ("omp (profile work)", "work-db"),
                ("pi", "pi-one"),
                ("Claude Code", "cc"),
                ("Claude Code (this project)", "proj"),
                ("Claude Desktop", "desk"),
                ("Codex", "cx"),
                ("Codex", "cxweb"),
                ("Gemini CLI", "gem"),
                ("Gemini CLI", "gemhttp"),
                ("OpenCode", "oc"),
                ("OpenCode", "ocp"),
                ("OpenCode", "ocr"),
            ]
        );
        assert_eq!(get(&found, "fs").file, base.join("home/.omp/agent/mcp.json"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn converts_entries() {
        let base = tmp();
        let r = roots(&base);
        fixtures(&base, &r);
        let found = scan(&r, &[]);
        assert_eq!(
            serde_json::to_string(&get(&found, "fs").entry).unwrap(),
            r#"{"type":"stdio","command":"npx","args":["-y","@x/fs"],"env":{"A":"1"}}"#
        );
        assert_eq!(get(&found, "work-db").entry, json!({"type":"stdio","command":"db-mcp"}));
        assert_eq!(get(&found, "pi-one").entry, json!({"type":"stdio","command":"pi-mcp"}));
        assert_eq!(get(&found, "desk").entry["env"], json!({"N":"5","B":"true"}));
        assert_eq!(get(&found, "cx").entry, json!({"type":"stdio","command":"cx","args":["a"],"env":{"K":"v"}}));
        assert_eq!(
            get(&found, "cxweb").entry,
            json!({"type":"http","url":"https://cx.example.com/mcp",
                   "headers":{"X-A":"b","Authorization":"Bearer ${CX_TOKEN}"}})
        );
        assert_eq!(get(&found, "gem").entry, json!({"type":"stdio","command":"gem","cwd":"/tmp"}));
        assert_eq!(
            get(&found, "gemhttp").entry,
            json!({"type":"http","url":"https://gem.example.com/mcp","headers":{"A":"b"}})
        );
        assert_eq!(
            get(&found, "oc").entry,
            json!({"type":"stdio","command":"oc-bin","args":["--flag"],"env":{"E":"1"}})
        );
        assert_eq!(
            get(&found, "ocr").entry,
            json!({"type":"http","url":"https://oc.example.com/mcp","headers":{"H":"v"}})
        );
        assert!(found.iter().all(|f| f.name != "gemsse" && f.name != "old" && f.name != "nope"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn same_connection_appears_once_and_targets_are_safe() {
        let base = tmp();
        let r = roots(&base);
        fixtures(&base, &r);
        let found = scan(&r, &[]);
        assert_eq!(found.iter().filter(|f| f.entry["command"] == "npx").count(), 1);
        assert_eq!(get(&found, "fs").kind, "stdio");
        assert_eq!(get(&found, "fs").target, "npx");
        assert_eq!(get(&found, "cc").target, "cc-mcp");
        assert_eq!(get(&found, "remote").kind, "http");
        assert_eq!(get(&found, "remote").target, "https://api.example.com:8443");
        assert_eq!(get(&found, "proj").target, "https://proj.example.com");
        for f in &found {
            for secret in ["sekret", "secret", "u:p", "-y", "?k=1"] {
                assert!(!f.target.contains(secret), "{} leaks {secret}", f.target);
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn drops_configured_by_name_and_by_connection() {
        let base = tmp();
        let r = roots(&base);
        fixtures(&base, &r);
        let cfg = [
            configured("fs", json!({"command":"other"})),
            configured("renamed", json!({"type":"stdio","command":"cc-mcp","args":["--token","sekret"]})),
            configured(
                "web",
                json!({"type":"http","url":"https://cx.example.com/mcp","headers":{"X-A":"b","Authorization":"Bearer ${CX_TOKEN}"}}),
            ),
        ];
        let found = scan(&r, &cfg);
        for gone in ["fs", "cc", "cxweb"] {
            assert!(found.iter().all(|f| f.name != gone), "{gone} should be filtered");
        }
        assert_eq!(found.len(), 14 - 3 + 1, "the name-filtered `fs` still lets its twin `fs-copy` through");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn skips_malformed_files_and_mus_own_file() {
        let base = tmp();
        let mut r = roots(&base);
        fixtures(&base, &r);
        put(&base, "home/.pi/agent/mcp.json", "{not json");
        put(&base, "home/.codex/config.toml", "= = =");
        r.agent_dir = base.join("home/.omp/agent");
        let found = scan(&r, &[]);
        assert!(found.iter().all(|f| f.tool != "pi" && f.tool != "Codex" && f.tool != "omp"));
        assert!(found.iter().any(|f| f.tool == "omp (profile work)"));
        let nowhere = Roots { home: base.join("missing"), cwd: base.join("missing"), ..r };
        assert!(scan(&nowhere, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sanitises_names() {
        let base = tmp();
        let r = roots(&base);
        let long = "a".repeat(60);
        let cfg =
            json!({"mcpServers":{"my server!":{"command":"a"},"":{"command":"b"},(long.clone()):{"command":"c"}}});
        put(&base, "home/.pi/agent/mcp.json", &cfg.to_string());
        let found = scan(&r, &[]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|f| f.name == "my-server-"));
        assert!(found.iter().any(|f| f.name == "a".repeat(48)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn same_name_different_servers_are_both_kept() {
        let base = tmp();
        let r = roots(&base);
        put(&base, "home/.pi/agent/mcp.json", r#"{"mcpServers":{"x":{"command":"one"}}}"#);
        put(&base, "home/.gemini/settings.json", r#"{"mcpServers":{"x":{"command":"two"}}}"#);
        let found = scan(&r, &[]);
        assert_eq!(tool_names(&found), [("pi", "x"), ("Gemini CLI", "x")]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn http_target_strips_everything_but_scheme_and_host() {
        assert_eq!(http_target("https://user:pw@host.example:9/a/b?c=d#e"), "https://host.example:9");
        assert_eq!(http_target("http://h"), "http://h");
        assert_eq!(http_target("https://h?token=1"), "https://h");
    }
}
