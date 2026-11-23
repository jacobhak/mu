//! Parsing for `/mcp`. Pure: no I/O. The interactive `add` flow is in `mcp_wizard`.

use mu_tools::mcp::store::{Level, check_name};
use serde_json::{Map, Value};

/// A server ready to be written to a config file.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub level: Level,
    pub name: String,
    pub entry: Value,
}

#[derive(Debug, PartialEq)]
pub enum McpCmd {
    List,
    /// `/mcp add` with no arguments: run the interactive `Wizard`.
    Wizard,
    /// `/mcp import` (or `discover`): the wizard, starting at the servers other tools have configured.
    Import,
    Add(Draft),
    Remove(String),
    Enable(String),
    Disable(String),
    Reload,
    Reconnect(String),
    Test(String),
    /// OAuth sign-in to a server.
    Login(String),
    /// Trust this project's MCP servers as they are now, so they start.
    Trust,
    /// Stop this project's MCP servers until they are trusted again.
    Untrust,
    Help,
}

pub const USAGE: &str = "\
/mcp                          list servers (same as /mcp list)
/mcp add                      add a server interactively (pick a preset, or import one)
/mcp import                   copy servers other tools have configured
/mcp add <name> [flags] [--] <command> [args...]   add a stdio server
/mcp add <name> [flags] <url>                      add an http server
    --user | --project        where to save (default: user)
    -e, --env K=V             environment variable (stdio; repeatable)
    -H, --header \"K: V\"       request header (http; repeatable; K=V also works)
    --timeout <ms>            request timeout in milliseconds
    --url <url>               http(s) endpoint (instead of a bare <url>)
    --                        everything after is the command and its arguments
/mcp remove <name>            remove a server (alias: rm)
/mcp enable <name>            enable a server
/mcp disable <name>           disable a server
/mcp reload                   re-read the config files and reconnect
/mcp reconnect <name>         reconnect one server
/mcp test <name>              connect and list the server's tools
/mcp login <name>             sign in to a server that uses OAuth
/mcp trust                    trust this project's MCP servers so they start
/mcp untrust                  stop this project's MCP servers until trusted again
/mcp help                     show this help
Example: /mcp add fs --project -e DEBUG=1 -- npx -y @modelcontextprotocol/server-filesystem .";

const ADD_USAGE: &str = "usage: /mcp add <name> [--user|--project] [-e K=V]... [-H \"K: V\"]... [--timeout <ms>] \
                         [--] <command> [args...] | <url>";

pub fn parse(arg: &str) -> Result<McpCmd, String> {
    let words = split_words(arg)?;
    let Some((sub, rest)) = words.split_first() else {
        return Ok(McpCmd::List);
    };
    match sub.as_str() {
        "list" | "ls" => no_args(rest, "list", McpCmd::List),
        "help" | "-h" | "--help" | "?" => Ok(McpCmd::Help),
        "reload" => no_args(rest, "reload", McpCmd::Reload),
        "import" | "discover" => no_args(rest, sub, McpCmd::Import),
        "add" => {
            if rest.is_empty() {
                Ok(McpCmd::Wizard)
            } else {
                parse_add(rest).map(McpCmd::Add)
            }
        }
        "remove" | "rm" => one_name(rest, "remove <name>").map(McpCmd::Remove),
        "enable" => one_name(rest, "enable <name>").map(McpCmd::Enable),
        "disable" => one_name(rest, "disable <name>").map(McpCmd::Disable),
        "reconnect" => one_name(rest, "reconnect <name>").map(McpCmd::Reconnect),
        "test" => one_name(rest, "test <name>").map(McpCmd::Test),
        "login" => one_name(rest, "login <name>").map(McpCmd::Login),
        "trust" => no_args(rest, "trust", McpCmd::Trust),
        "untrust" => no_args(rest, "untrust", McpCmd::Untrust),
        other => Err(format!("unknown /mcp subcommand `{other}` (try /mcp help)")),
    }
}

fn no_args(rest: &[String], sub: &str, cmd: McpCmd) -> Result<McpCmd, String> {
    if rest.is_empty() { Ok(cmd) } else { Err(format!("usage: /mcp {sub}")) }
}

fn one_name(rest: &[String], usage: &str) -> Result<String, String> {
    match rest {
        [name] if !name.is_empty() => Ok(name.clone()),
        _ => Err(format!("usage: /mcp {usage}")),
    }
}

/// Shell-like word splitting: whitespace separates; quotes and backslashes group.
pub fn split_words(s: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => cur.push(c),
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => cur.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub(crate) fn check_url(url: &str) -> Result<(), String> {
    if is_http_url(url) { Ok(()) } else { Err(format!("`{url}` is not an http:// or https:// url")) }
}

pub(crate) fn parse_env(word: &str) -> Result<(String, String), String> {
    match word.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("`{word}` is not a valid environment entry (expected K=V)")),
    }
}

pub(crate) fn parse_header(word: &str) -> Result<(String, String), String> {
    let split = word.find([':', '=']).map(|i| (&word[..i], &word[i + 1..]));
    match split {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
        _ => Err(format!("`{word}` is not a valid header (expected \"K: V\" or K=V)")),
    }
}

pub(crate) fn stdio_entry(command: &str, args: &[String], env: Map<String, Value>, timeout: Option<u64>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), "stdio".into());
    m.insert("command".into(), command.into());
    if !args.is_empty() {
        m.insert("args".into(), Value::Array(args.iter().map(|a| Value::String(a.clone())).collect()));
    }
    if !env.is_empty() {
        m.insert("env".into(), Value::Object(env));
    }
    if let Some(t) = timeout {
        m.insert("timeout".into(), t.into());
    }
    Value::Object(m)
}

pub(crate) fn http_entry(url: &str, headers: Map<String, Value>, timeout: Option<u64>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), "http".into());
    m.insert("url".into(), url.into());
    if !headers.is_empty() {
        m.insert("headers".into(), Value::Object(headers));
    }
    if let Some(t) = timeout {
        m.insert("timeout".into(), t.into());
    }
    Value::Object(m)
}

fn parse_add(words: &[String]) -> Result<Draft, String> {
    let name = words[0].clone();
    if name.starts_with('-') {
        return Err(format!("the server name comes first, before any flags; {ADD_USAGE}"));
    }
    check_name(&name).map_err(|e| format!("{e}; {ADD_USAGE}"))?;

    let mut user = false;
    let mut project = false;
    let mut env = Map::new();
    let mut headers = Map::new();
    let mut timeout: Option<u64> = None;
    let mut url: Option<String> = None;
    let mut command: &[String] = &[];
    let mut verbatim = false;

    let mut i = 1;
    while i < words.len() {
        let w = words[i].as_str();
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (w, None),
        };
        let takes_value = matches!(flag, "-e" | "--env" | "-H" | "--header" | "--timeout" | "--url");
        let value = if takes_value {
            match inline {
                Some(v) => Some(v),
                None => {
                    i += 1;
                    Some(words.get(i).cloned().ok_or_else(|| format!("`{flag}` needs a value; {ADD_USAGE}"))?)
                }
            }
        } else {
            None
        };
        match (flag, value) {
            ("--user", None) => user = true,
            ("--project", None) => project = true,
            ("-e" | "--env", Some(v)) => {
                let (k, v) = parse_env(&v)?;
                env.insert(k, Value::String(v));
            }
            ("-H" | "--header", Some(v)) => {
                let (k, v) = parse_header(&v)?;
                headers.insert(k, Value::String(v));
            }
            ("--timeout", Some(v)) => {
                timeout = Some(v.trim().parse().map_err(|_| format!("`{v}` is not a valid timeout (milliseconds)"))?);
            }
            ("--url", Some(v)) => {
                check_url(&v)?;
                url = Some(v);
            }
            ("--", None) => {
                command = &words[i + 1..];
                verbatim = true;
                break;
            }
            _ if w.starts_with('-') => {
                return Err(format!("unknown flag `{w}`; put `--` before the command if it starts with `-`"));
            }
            _ => {
                command = &words[i..];
                break;
            }
        }
        i += 1;
    }

    let bare_url = !verbatim && url.is_none() && command.first().is_some_and(|c| is_http_url(c));
    if bare_url {
        if command.len() > 1 {
            return Err(format!("unexpected `{}` after the url; put flags before it", command[1]));
        }
        url = Some(command[0].clone());
        command = &[];
    }

    let level = match (user, project) {
        (true, true) => return Err("use either --user or --project, not both".into()),
        (_, true) => Level::Project,
        _ => Level::User,
    };

    let entry = match (url, command.split_first()) {
        (Some(_), Some(_)) => return Err("give either a url or a command, not both".into()),
        (None, None) => return Err(format!("give a command or a url; {ADD_USAGE}")),
        (Some(url), None) => {
            if !env.is_empty() {
                return Err("--env only applies to stdio servers (use --header for http)".into());
            }
            http_entry(&url, headers, timeout)
        }
        (None, Some((cmd, args))) => {
            if !headers.is_empty() {
                return Err("--header only applies to http servers (use --env for stdio)".into());
            }
            if cmd.is_empty() {
                return Err("the command is empty".into());
            }
            stdio_entry(cmd, args, env, timeout)
        }
    };
    Ok(Draft { level, name, entry })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn add(arg: &str) -> Draft {
        match parse(arg) {
            Ok(McpCmd::Add(d)) => d,
            other => panic!("expected Add for `{arg}`, got {other:?}"),
        }
    }

    fn err(arg: &str) -> String {
        parse(arg).expect_err(arg)
    }

    fn w(s: &str) -> Vec<String> {
        split_words(s).unwrap()
    }

    #[test]
    fn simple_subcommands() {
        assert_eq!(parse(""), Ok(McpCmd::List));
        assert_eq!(parse("   "), Ok(McpCmd::List));
        assert_eq!(parse("list"), Ok(McpCmd::List));
        assert_eq!(parse("ls"), Ok(McpCmd::List));
        assert_eq!(parse("reload"), Ok(McpCmd::Reload));
        assert_eq!(parse("trust"), Ok(McpCmd::Trust));
        assert_eq!(parse("untrust"), Ok(McpCmd::Untrust));
        assert!(parse("trust now").is_err());
        for h in ["help", "-h", "--help", "?"] {
            assert_eq!(parse(h), Ok(McpCmd::Help), "{h}");
        }
        assert_eq!(parse("add"), Ok(McpCmd::Wizard));
    }

    #[test]
    fn name_subcommands() {
        assert_eq!(parse("remove fs"), Ok(McpCmd::Remove("fs".into())));
        assert_eq!(parse("rm fs"), Ok(McpCmd::Remove("fs".into())));
        assert_eq!(parse("enable fs"), Ok(McpCmd::Enable("fs".into())));
        assert_eq!(parse("disable fs"), Ok(McpCmd::Disable("fs".into())));
        assert_eq!(parse("reconnect fs"), Ok(McpCmd::Reconnect("fs".into())));
        assert_eq!(parse("test fs"), Ok(McpCmd::Test("fs".into())));
        assert_eq!(parse("login slack"), Ok(McpCmd::Login("slack".into())));
    }

    #[test]
    fn name_not_charset_validated_except_on_add() {
        assert_eq!(parse("remove 'odd name!'"), Ok(McpCmd::Remove("odd name!".into())));
        assert_eq!(parse("test a.b"), Ok(McpCmd::Test("a.b".into())));
    }

    #[test]
    fn name_arg_errors() {
        for s in ["remove", "rm", "enable", "disable", "reconnect", "test", "login"] {
            assert!(err(s).starts_with("usage: /mcp "), "{s}");
        }
        for s in ["remove a b", "enable a b", "disable a b", "reconnect a b", "test a b", "login a b", "remove ''"] {
            assert!(err(s).starts_with("usage: /mcp "), "{s}");
        }
    }

    #[test]
    fn extra_args_on_argless_subcommands() {
        assert!(err("list x").starts_with("usage: /mcp list"));
        assert!(err("reload x").starts_with("usage: /mcp reload"));
    }

    #[test]
    fn unknown_subcommand() {
        let e = err("frobnicate x");
        assert!(e.starts_with("unknown /mcp subcommand `frobnicate`"), "{e}");
    }

    #[test]
    fn unterminated_quote_is_error() {
        assert!(err("remove 'x").contains("unterminated"));
    }

    #[test]
    fn split_plain_and_whitespace() {
        assert_eq!(w(""), Vec::<String>::new());
        assert_eq!(w("  a \t b\n c  "), ["a", "b", "c"]);
    }

    #[test]
    fn split_quotes() {
        assert_eq!(w(r#"a "b c" 'd e'"#), ["a", "b c", "d e"]);
        assert_eq!(w(r#"x"y z"w"#), ["xy zw"]);
        assert_eq!(w(r#""it's" 'say "hi"'"#), ["it's", r#"say "hi""#]);
    }

    #[test]
    fn split_empty_quoted_word_is_kept() {
        assert_eq!(w(r#"a "" b ''"#), ["a", "", "b", ""]);
    }

    #[test]
    fn split_backslashes() {
        assert_eq!(w(r"a\ b c"), ["a b", "c"]);
        assert_eq!(w(r#"\"q\""#), [r#""q""#]);
        assert_eq!(w(r#""a\"b""#), [r#"a"b"#]);
        assert_eq!(w(r"'a\b'"), [r"a\b"]);
    }

    #[test]
    fn split_errors() {
        assert!(split_words("'abc").unwrap_err().contains("single"));
        assert!(split_words("\"abc").unwrap_err().contains("double"));
        assert!(split_words("\"abc\\").unwrap_err().contains("double"));
        assert!(split_words("abc\\").unwrap_err().contains("backslash"));
    }

    #[test]
    fn add_stdio_minimal() {
        let d = add("add fs npx");
        assert_eq!(d.level, Level::User);
        assert_eq!(d.name, "fs");
        assert_eq!(d.entry, json!({"type": "stdio", "command": "npx"}));
    }

    #[test]
    fn add_stdio_full_entry_and_key_order() {
        let d = add("add fs --project -e A=1 --env B=two=2 --timeout 5000 -- npx -y pkg 'a b'");
        assert_eq!(d.level, Level::Project);
        assert_eq!(
            d.entry,
            json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "pkg", "a b"],
                "env": {"A": "1", "B": "two=2"},
                "timeout": 5000
            })
        );
        let keys: Vec<_> = d.entry.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["type", "command", "args", "env", "timeout"]);
        assert!(d.entry["timeout"].is_number());
        let env_keys: Vec<_> = d.entry["env"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(env_keys, ["A", "B"]);
    }

    #[test]
    fn add_command_starts_without_double_dash() {
        let d = add("add fs -e K=V node server.js --port 1");
        assert_eq!(
            d.entry,
            json!({"type": "stdio", "command": "node", "args": ["server.js", "--port", "1"], "env": {"K": "V"}})
        );
    }

    #[test]
    fn add_flags_after_command_start_belong_to_command() {
        let d = add("add fs npx -y pkg --user --project");
        assert_eq!(d.level, Level::User);
        assert_eq!(d.entry["args"], json!(["-y", "pkg", "--user", "--project"]));
    }

    #[test]
    fn add_dash_y_inside_command() {
        let d = add("add fs --user npx -y @modelcontextprotocol/server-filesystem /tmp");
        assert_eq!(d.entry["command"], "npx");
        assert_eq!(d.entry["args"], json!(["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]));
    }

    #[test]
    fn add_double_dash_is_verbatim() {
        let d = add("add x -- --weird -e A=1");
        assert_eq!(d.entry, json!({"type": "stdio", "command": "--weird", "args": ["-e", "A=1"]}));
        let d = add("add x -- https://example.com");
        assert_eq!(d.entry, json!({"type": "stdio", "command": "https://example.com"}));
    }

    #[test]
    fn add_unknown_flag_before_command() {
        let e = err("add x -y npx");
        assert!(e.contains("unknown flag `-y`") && e.contains("`--`"), "{e}");
        assert!(err("add x --nope").contains("unknown flag `--nope`"));
    }

    #[test]
    fn add_flag_missing_value() {
        for s in ["add x -e", "add x --header", "add x --timeout", "add x --url"] {
            assert!(err(s).contains("needs a value"), "{s}");
        }
    }

    #[test]
    fn add_http_bare_url() {
        let d = add("add api https://example.com/mcp");
        assert_eq!(d.entry, json!({"type": "http", "url": "https://example.com/mcp"}));
        let d = add("add api http://localhost:3000/mcp");
        assert_eq!(d.entry["url"], "http://localhost:3000/mcp");
    }

    #[test]
    fn add_http_full_entry_and_key_order() {
        let d = add(r#"add api --project -H "Authorization: Bearer x" --header X-Foo=bar --timeout 100 https://h/mcp"#);
        assert_eq!(d.level, Level::Project);
        assert_eq!(
            d.entry,
            json!({
                "type": "http",
                "url": "https://h/mcp",
                "headers": {"Authorization": "Bearer x", "X-Foo": "bar"},
                "timeout": 100
            })
        );
        let keys: Vec<_> = d.entry.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["type", "url", "headers", "timeout"]);
    }

    #[test]
    fn add_http_via_url_flag() {
        let d = add("add api --url https://h/mcp -H A=b");
        assert_eq!(d.entry, json!({"type": "http", "url": "https://h/mcp", "headers": {"A": "b"}}));
        let d = add("add api --url=https://h/mcp --timeout=7");
        assert_eq!(d.entry, json!({"type": "http", "url": "https://h/mcp", "timeout": 7}));
    }

    #[test]
    fn add_header_splitting() {
        let d = add(r#"add api -H "A: b=c" -H "B=d: e" -H " C :  f " https://h"#);
        assert_eq!(d.entry["headers"], json!({"A": "b=c", "B": "d: e", "C": "f"}));
        assert!(err("add api -H nocolon https://h").contains("not a valid header"));
        assert!(err("add api -H ': v' https://h").contains("not a valid header"));
        assert!(err("add api -H '=v' https://h").contains("not a valid header"));
    }

    #[test]
    fn add_env_splitting() {
        let d = add("add x -e A= -e B=c=d cmd");
        assert_eq!(d.entry["env"], json!({"A": "", "B": "c=d"}));
        assert!(err("add x -e NOEQ cmd").contains("not a valid environment entry"));
        assert!(err("add x -e =v cmd").contains("not a valid environment entry"));
    }

    #[test]
    fn add_duplicate_keys_later_wins() {
        let d = add("add x -e A=1 -e B=2 -e A=3 cmd");
        assert_eq!(d.entry["env"], json!({"A": "3", "B": "2"}));
        let keys: Vec<_> = d.entry["env"].as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["A", "B"]);
        let d = add("add x -H A=1 -H 'A: 2' https://h");
        assert_eq!(d.entry["headers"], json!({"A": "2"}));
    }

    #[test]
    fn add_conflicting_flags() {
        assert!(err("add x --user --project cmd").contains("not both"));
        assert!(err("add x --project --user cmd").contains("not both"));
        assert!(err("add x -e A=1 https://h").contains("--env only applies to stdio"));
        assert!(err("add x -H A=1 cmd").contains("--header only applies to http"));
        assert!(err("add x --url https://h cmd").contains("not both"));
        assert!(err("add x --url https://h -- cmd").contains("not both"));
        assert!(err("add x --url https://h https://i").contains("not both"));
    }

    #[test]
    fn add_needs_command_or_url() {
        assert!(err("add x").contains("give a command or a url"));
        assert!(err("add x --project").contains("give a command or a url"));
        assert!(err("add x --").contains("give a command or a url"));
    }

    #[test]
    fn add_empty_command() {
        assert!(err("add x -- ''").contains("command is empty"));
    }

    #[test]
    fn add_url_validation() {
        assert!(err("add x --url ftp://h").contains("not an http:// or https:// url"));
        assert!(err("add x --url h.com").contains("not an http:// or https:// url"));
    }

    #[test]
    fn add_args_after_bare_url_rejected() {
        let e = err("add x https://h --header A=b");
        assert!(e.contains("unexpected `--header`"), "{e}");
    }

    #[test]
    fn add_timeout_validation() {
        assert_eq!(add("add x --timeout 0 cmd").entry["timeout"], 0);
        for t in ["-1", "abc", "1.5", ""] {
            let e = parse(&format!("add x --timeout '{t}' cmd")).unwrap_err();
            assert!(e.contains("not a valid timeout"), "{t}: {e}");
        }
    }

    #[test]
    fn add_name_validation() {
        assert!(err("add 'bad name' cmd").contains("not a valid server name"));
        assert!(err("add --user cmd").contains("name comes first"));
        assert!(err("add a/b cmd").contains("not a valid server name"));
        assert!(err(&format!("add {} cmd", "x".repeat(49))).contains("not a valid server name"));
        assert_eq!(add("add a_b-9 cmd").name, "a_b-9");
    }

    #[test]
    fn add_quoted_args() {
        let d = add(r#"add x -- sh -c "echo 'hi there'""#);
        assert_eq!(d.entry["args"], json!(["-c", "echo 'hi there'"]));
    }

    #[test]
    fn usage_mentions_every_subcommand() {
        for s in
            ["add", "remove", "enable", "disable", "reload", "reconnect", "test", "login", "help", "--project", "--env"]
        {
            assert!(USAGE.contains(s), "{s}");
        }
    }

    #[test]
    fn import_subcommands() {
        assert_eq!(parse("import"), Ok(McpCmd::Import));
        assert_eq!(parse("discover"), Ok(McpCmd::Import));
        assert_eq!(parse("import  "), Ok(McpCmd::Import));
        assert_eq!(err("import all"), "usage: /mcp import");
        assert_eq!(err("discover x y"), "usage: /mcp discover");
    }

    #[test]
    fn usage_mentions_import() {
        assert!(USAGE.contains("/mcp import"));
        assert!(USAGE.contains("copy servers other tools have configured"));
    }

    #[test]
    fn shared_parsers() {
        assert_eq!(parse_env("A=b=c"), Ok(("A".into(), "b=c".into())));
        assert!(parse_env("=x").is_err());
        assert_eq!(parse_header("Authorization: Bearer x"), Ok(("Authorization".into(), "Bearer x".into())));
        assert_eq!(parse_header("X=y"), Ok(("X".into(), "y".into())));
        assert!(parse_header(": x").is_err());
        assert!(check_url("https://h").is_ok());
        assert!(check_url("ftp://h").is_err());
    }
}
