//! Keeps recognisable secrets out of what mu stores and sends: tool results, user messages, plugin
//! context and spilled output pass through `redact` before they reach the session file, memory,
//! subagent results or the provider. This guards against a careless model printing a key; it is no
//! boundary, since anything with bash can encode or relocate a value before mu sees it.
//!
//! Known values (secret-named environment variables, `.env*` files in the working directory) are
//! matched exactly; unknown ones only by unambiguous key formats. Nothing heuristic, so no false
//! positives on hashes or ids.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Env,
    DotEnv,
    Shape,
    Stored,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct Hit {
    pub label: String,
    pub source: Source,
}

impl Hit {
    pub fn placeholder(&self) -> String {
        format!("[REDACTED:{}]", self.label)
    }
}

pub struct Redacted {
    pub text: String,
    /// Distinct secrets replaced, in order of first appearance.
    pub hits: Vec<Hit>,
}

#[derive(Clone, Debug)]
struct Known {
    value: String,
    hit: Hit,
}

/// `[secrets] env`: extra variables whose values count as secrets, whatever their name.
static EXTRA_ENV: OnceLock<Vec<String>> = OnceLock::new();
static ENV: OnceLock<Vec<Known>> = OnceLock::new();
/// Per working directory: when it was last checked, the files seen then and their values.
type DotEnvCache = HashMap<PathBuf, (Instant, Vec<(PathBuf, Option<SystemTime>, u64)>, Vec<Known>)>;
static DOTENV: Mutex<Option<DotEnvCache>> = Mutex::new(None);
/// A spill redacts every chunk; the directory is listed again at most this often.
const DOTENV_RECHECK: Duration = Duration::from_secs(2);
/// Placeholders handed out in this process; `guard` refuses to write them back.
static EMITTED: Mutex<BTreeSet<Hit>> = Mutex::new(BTreeSet::new());
/// Provider API keys mu stores or resolves, registered under the provider's variable name.
static STORED: Mutex<Vec<Known>> = Mutex::new(Vec::new());

/// Redacts `value` (a provider key) from now on as `[REDACTED:<label>]`. Values under 8 bytes are
/// ignored: they would match ordinary text.
pub fn register_stored(label: &str, value: &str) {
    let value = value.trim();
    if value.len() < 8 {
        return;
    }
    if let Ok(mut s) = STORED.lock()
        && !s.iter().any(|k| k.value == value)
    {
        s.push(Known { value: value.to_string(), hit: Hit { label: label.to_string(), source: Source::Stored } });
    }
}

/// `[secrets]` from config. Unconfigured (tests, early errors) means no extra variables.
pub fn configure(extra_env: Vec<String>) {
    let _ = EXTRA_ENV.set(extra_env);
}

fn extra_env() -> &'static [String] {
    EXTRA_ENV.get().map(Vec::as_slice).unwrap_or_default()
}

pub fn redact(text: &str, cwd: &Path) -> Redacted {
    if text.is_empty() {
        return Redacted { text: String::new(), hits: Vec::new() };
    }
    let env = ENV.get_or_init(|| known_from(std::env::vars(), Source::Env, extra_env()));
    let dot = dotenv_for(cwd);
    crate::auth_store::sync_redaction();
    let stored = STORED.lock().map(|s| s.clone()).unwrap_or_default();
    let r = apply(text, env.iter().chain(dot.iter()).chain(stored.iter()));
    if !r.hits.is_empty()
        && let Ok(mut e) = EMITTED.lock()
    {
        e.extend(r.hits.iter().cloned());
    }
    r
}

/// `text` with environment and stored keys (and recognisable key formats) replaced; for text
/// that did not come from a tool, such as a provider's error body.
pub fn redact_keys(text: &str) -> String {
    let env = ENV.get_or_init(|| known_from(std::env::vars(), Source::Env, extra_env()));
    crate::auth_store::sync_redaction();
    let stored = STORED.lock().map(|s| s.clone()).unwrap_or_default();
    apply(text, env.iter().chain(stored.iter())).text
}

/// Every string in `v`, in place. Returns the distinct hits.
pub fn redact_json(v: &mut serde_json::Value, cwd: &Path) -> Vec<Hit> {
    let mut hits = Vec::new();
    walk_json(v, &mut |s| {
        let r = redact(s, cwd);
        if !r.hits.is_empty() {
            *s = r.text;
            merge(&mut hits, r.hits);
        }
    });
    hits
}

fn walk_json(v: &mut serde_json::Value, f: &mut dyn FnMut(&mut String)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk_json(x, f)),
        serde_json::Value::Object(o) => o.values_mut().for_each(|x| walk_json(x, f)),
        _ => {}
    }
}

pub fn merge(into: &mut Vec<Hit>, more: Vec<Hit>) {
    for h in more {
        if !into.contains(&h) {
            into.push(h);
        }
    }
}

/// The note appended to a tool result that had secrets replaced, so the model knows what the
/// placeholders are and how to use the values without seeing them.
pub fn feedback(hits: &[Hit]) -> String {
    let list: Vec<String> = hits
        .iter()
        .map(|h| match h.source {
            Source::Env => format!("{} (environment variable)", h.placeholder()),
            Source::DotEnv => format!("{} (from a .env file)", h.placeholder()),
            Source::Shape => format!("{} (recognised key format)", h.placeholder()),
            Source::Stored => format!("{} (an API key stored by mu)", h.placeholder()),
        })
        .collect();
    let mut s = format!(
        "[mu: secrets in this output were replaced before you saw them: {}. The values are not shown to you and are not stored.",
        list.join(", ")
    );
    if let Some(h) = hits.iter().find(|h| h.source == Source::Env) {
        s.push_str(&format!(" Refer to environment variables by name in commands (\"${}\").", h.label));
    }
    s.push_str(" Do not try to print the values, and never write a placeholder into a file or command.]");
    s
}

/// Placeholders this process handed out that appear in `text`.
pub fn placeholders_in(text: &str) -> Vec<Hit> {
    if !text.contains("[REDACTED:") {
        return Vec::new();
    }
    let Ok(e) = EMITTED.lock() else { return Vec::new() };
    e.iter().filter(|h| text.contains(&h.placeholder())).cloned().collect()
}

/// Refuses a `bash`, `edit` or `write` call that would write a placeholder where the secret was:
/// the placeholder is not the value, so the file or command would silently change.
pub fn guard(tool: &str, args: &serde_json::Value) -> Result<(), String> {
    if !matches!(tool, "bash" | "edit" | "write") {
        return Ok(());
    }
    let hits = placeholders_in(&args.to_string());
    let Some(first) = hits.first() else { return Ok(()) };
    let names: Vec<String> = hits.iter().map(Hit::placeholder).collect();
    let mut msg = format!(
        "{} {} a placeholder mu put in place of a secret, not the secret itself; this call would write the placeholder.",
        names.join(", "),
        if hits.len() == 1 { "is" } else { "are" }
    );
    if tool == "bash" && first.source == Source::Env {
        msg.push_str(&format!(" Use the environment variable instead: \"${}\".", first.label));
    } else {
        msg.push_str(
            " Leave the lines that hold it unchanged; if the value itself has to change, ask the user to do it.",
        );
    }
    Err(msg)
}

fn apply<'a>(text: &str, known: impl Iterator<Item = &'a Known>) -> Redacted {
    let mut known: Vec<&Known> = known.collect();
    // Longest first, so a value that contains another is replaced whole.
    known.sort_by_key(|k| std::cmp::Reverse(k.value.len()));
    let mut out = text.to_string();
    let mut hits = Vec::new();
    for k in known {
        if out.contains(&k.value) {
            out = out.replace(&k.value, &k.hit.placeholder());
            merge(&mut hits, vec![k.hit.clone()]);
        }
    }
    let (out, shaped) = shapes(&out);
    merge(&mut hits, shaped);
    Redacted { text: out, hits }
}

fn known_from(vars: impl Iterator<Item = (String, String)>, source: Source, extra: &[String]) -> Vec<Known> {
    let mut out: Vec<Known> = Vec::new();
    let mut add = |value: &str, label: String| {
        if !out.iter().any(|k| k.value == value) {
            out.push(Known { value: value.to_string(), hit: Hit { label, source } });
        }
    };
    for (name, value) in vars {
        let value = value.trim();
        let listed = extra.iter().any(|e| e == &name) && value.len() >= 4;
        if listed || (secret_name(&name) && secret_value(value)) {
            add(value, name.clone());
        }
        if let Some(pw) = url_password(value).filter(|p| secret_value(p)) {
            add(pw, format!("{name}:password"));
        }
    }
    out
}

/// A variable whose name says it holds a secret (`*_API_KEY`, `*_TOKEN`, `DB_PASSWORD`, …) and not
/// something about one (`*_KEY_PATH`, `*_KEY_ID`, `SSH_AUTH_SOCK`).
fn secret_name(name: &str) -> bool {
    const YES: &[&str] = &[
        "KEY",
        "KEYS",
        "APIKEY",
        "SECRET",
        "SECRETS",
        "TOKEN",
        "TOKENS",
        "PASSWORD",
        "PASSWD",
        "PASS",
        "PAT",
        "CREDENTIAL",
        "CREDENTIALS",
    ];
    const NO: &[&str] = &[
        "PATH", "FILE", "DIR", "SOCK", "SOCKET", "ID", "URL", "URI", "HOST", "PORT", "USER", "USERNAME", "NAME",
        "TYPE", "MODE", "ENDPOINT", "REGION",
    ];
    let up = name.to_ascii_uppercase();
    let segs: Vec<&str> = up.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).collect();
    segs.iter().any(|s| YES.contains(s)) && !segs.iter().any(|s| NO.contains(s))
}

/// Long enough and varied enough to be a credential rather than a word, flag or path.
fn secret_value(v: &str) -> bool {
    if v.len() < 8 || v.starts_with('/') || v.starts_with('~') || v.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let classes = [
        v.bytes().any(|b| b.is_ascii_lowercase()),
        v.bytes().any(|b| b.is_ascii_uppercase()),
        v.bytes().any(|b| b.is_ascii_digit()),
        v.bytes().any(|b| !b.is_ascii_alphanumeric()),
    ];
    classes.iter().filter(|c| **c).count() >= 2 || v.len() >= 20
}

/// The password in `scheme://user:password@host…`.
fn url_password(v: &str) -> Option<&str> {
    let rest = &v[v.find("://")? + 3..];
    let auth = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let creds = &auth[..auth.rfind('@')?];
    let pw = &creds[creds.find(':')? + 1..];
    (!pw.is_empty()).then_some(pw)
}

fn dotenv_for(cwd: &Path) -> Vec<Known> {
    dotenv_within(cwd, DOTENV_RECHECK)
}

fn dotenv_within(cwd: &Path, recheck: Duration) -> Vec<Known> {
    let Ok(mut guard) = DOTENV.lock() else { return Vec::new() };
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((at, _, known)) = cache.get(cwd)
        && at.elapsed() < recheck
    {
        return known.clone();
    }
    let files = dotenv_files(cwd);
    if let Some((at, seen, known)) = cache.get_mut(cwd)
        && *seen == files
    {
        *at = Instant::now();
        return known.clone();
    }
    let mut vars = Vec::new();
    for (path, ..) in &files {
        if let Ok(s) = std::fs::read_to_string(path) {
            vars.extend(parse_dotenv(&s));
        }
    }
    let extra = extra_env();
    let known = known_from(vars.into_iter(), Source::DotEnv, extra);
    cache.insert(cwd.to_path_buf(), (Instant::now(), files, known.clone()));
    known
}

/// `.env` and `.env.*` in `cwd`, except templates, with what tells a change apart.
fn dotenv_files(cwd: &Path) -> Vec<(PathBuf, Option<SystemTime>, u64)> {
    const TEMPLATES: &[&str] = &["example", "sample", "template", "dist", "defaults"];
    let Ok(rd) = std::fs::read_dir(cwd) else { return Vec::new() };
    let mut out: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let ok = name == ".env"
                || name.strip_prefix(".env.").is_some_and(|rest| !TEMPLATES.iter().any(|t| rest.ends_with(t)));
            let md = e.metadata().ok().filter(|m| m.is_file())?;
            ok.then(|| (e.path(), md.modified().ok(), md.len()))
        })
        .collect();
    out.sort();
    out
}

fn parse_dotenv(s: &str) -> Vec<(String, String)> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            let l = l.strip_prefix("export ").unwrap_or(l);
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

struct Shape {
    prefix: &'static str,
    label: &'static str,
    body: fn(u8) -> bool,
    min: usize,
    max: usize,
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn keyish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}
fn upper_digit(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

/// Formats that are keys wherever they appear. Earlier entries win (`sk-ant-` before `sk-`).
const SHAPES: &[Shape] = &[
    Shape { prefix: "github_pat_", label: "github-token", body: word, min: 22, max: 255 },
    Shape { prefix: "ghp_", label: "github-token", body: alnum, min: 36, max: 255 },
    Shape { prefix: "gho_", label: "github-token", body: alnum, min: 36, max: 255 },
    Shape { prefix: "ghu_", label: "github-token", body: alnum, min: 36, max: 255 },
    Shape { prefix: "ghs_", label: "github-token", body: alnum, min: 36, max: 255 },
    Shape { prefix: "ghr_", label: "github-token", body: alnum, min: 36, max: 255 },
    Shape { prefix: "sk-ant-", label: "anthropic-key", body: keyish, min: 20, max: 400 },
    Shape { prefix: "sk-", label: "openai-key", body: keyish, min: 20, max: 400 },
    Shape { prefix: "AKIA", label: "aws-access-key", body: upper_digit, min: 16, max: 16 },
    Shape { prefix: "ASIA", label: "aws-access-key", body: upper_digit, min: 16, max: 16 },
    Shape { prefix: "AIza", label: "google-api-key", body: keyish, min: 35, max: 35 },
    Shape { prefix: "xoxb-", label: "slack-token", body: keyish, min: 10, max: 255 },
    Shape { prefix: "xoxp-", label: "slack-token", body: keyish, min: 10, max: 255 },
    Shape { prefix: "xoxa-", label: "slack-token", body: keyish, min: 10, max: 255 },
    Shape { prefix: "xoxs-", label: "slack-token", body: keyish, min: 10, max: 255 },
    Shape { prefix: "xoxr-", label: "slack-token", body: keyish, min: 10, max: 255 },
    Shape { prefix: "sk_live_", label: "stripe-key", body: alnum, min: 24, max: 255 },
    Shape { prefix: "rk_live_", label: "stripe-key", body: alnum, min: 24, max: 255 },
];

const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_KEY: &str = "PRIVATE KEY-----";

fn shapes(text: &str) -> (String, Vec<Hit>) {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut hits = Vec::new();
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !b[i].is_ascii() || (i > 0 && word(b[i - 1])) {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let mut end = None;
        let mut label = "";
        if rest.starts_with(PEM_BEGIN) && rest.lines().next().is_some_and(|l| l.trim_end().ends_with(PEM_KEY)) {
            // To the END line, or to the end of the text when it was cut off.
            let e = rest.find("-----END ").map_or(rest.len(), |s| {
                let after = &rest[s..];
                s + after.find(PEM_KEY).map_or(after.len(), |k| k + PEM_KEY.len())
            });
            end = Some(i + e);
            label = "private-key";
        } else if let Some(s) = SHAPES.iter().find(|s| rest.starts_with(s.prefix)) {
            let body = &b[i + s.prefix.len()..];
            let n = body.iter().take(s.max + 1).take_while(|c| (s.body)(**c)).count();
            if n >= s.min && n <= s.max {
                end = Some(i + s.prefix.len() + n);
                label = s.label;
            }
        }
        match end {
            Some(e) => {
                out.push_str(&text[copied..i]);
                let hit = Hit { label: label.into(), source: Source::Shape };
                out.push_str(&hit.placeholder());
                merge(&mut hits, vec![hit]);
                i = e;
                copied = e;
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    (out, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(v: &[(&str, &str)]) -> impl Iterator<Item = (String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn names_that_hold_secrets() {
        for n in ["GEMINI_API_KEY", "MODAL_TOKEN", "AWS_SECRET_ACCESS_KEY", "DB_PASSWORD", "apiKey", "GH_PAT"] {
            assert!(secret_name(n), "{n}");
        }
        for n in ["SSH_AUTH_SOCK", "KEY_PATH", "AWS_ACCESS_KEY_ID", "GIT_AUTHOR_NAME", "PWD", "KEYBOARD", "TOKEN_URL"] {
            assert!(!secret_name(n), "{n}");
        }
    }

    #[test]
    fn values_that_look_like_credentials() {
        assert!(secret_value("AIzaSyD-x9Q2abc"));
        assert!(secret_value("abcdefghijklmnopqrstuvwxyz"));
        for v in ["postgres", "short", "/home/u/.ssh/id", "12345678901", "changeme"] {
            assert!(!secret_value(v), "{v}");
        }
    }

    #[test]
    fn known_values_are_replaced_with_their_name() {
        let k = known_from(
            vars(&[
                ("MODAL_TOKEN", "as-Xy12_tok3n"),
                ("HOME", "/home/x"),
                ("DATABASE_URL", "postgres://u:Pa55word!@db/x"),
            ]),
            Source::Env,
            &[],
        );
        let r = apply("token=as-Xy12_tok3n url=postgres://u:Pa55word!@db/x home=/home/x", k.iter());
        assert_eq!(
            r.text,
            "token=[REDACTED:MODAL_TOKEN] url=postgres://u:[REDACTED:DATABASE_URL:password]@db/x home=/home/x"
        );
        assert_eq!(r.hits.len(), 2);
        assert_eq!(r.hits[0].label, "MODAL_TOKEN");
    }

    #[test]
    fn extra_names_from_config_count_regardless_of_name() {
        let k = known_from(vars(&[("WEIRD", "zzzz9")]), Source::Env, &["WEIRD".into()]);
        assert_eq!(apply("x zzzz9 y", k.iter()).text, "x [REDACTED:WEIRD] y");
    }

    #[test]
    fn longest_value_wins() {
        let k = known_from(vars(&[("A_KEY", "abcdEFGH12"), ("B_KEY", "abcdEFGH12345678")]), Source::Env, &[]);
        assert_eq!(apply("abcdEFGH12345678", k.iter()).text, "[REDACTED:B_KEY]");
    }

    #[test]
    fn key_formats() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let (out, hits) = shapes(&format!("x {gh} y AKIAABCDEFGHIJKLMNOP sk-ant-api03-abcdefghijklmnopqrstuv"));
        assert_eq!(out, "x [REDACTED:github-token] y [REDACTED:aws-access-key] [REDACTED:anthropic-key]");
        assert_eq!(hits.len(), 3);
        // Not keys: inside a word, too short, too long for a fixed-length format.
        for s in ["task-abcdefghijklmnopqrstuvwxyz", "ghp_short", "AKIAABCDEFGHIJKLMNOPQ", "the sk- prefix"] {
            assert_eq!(shapes(s).0, s, "{s}");
        }
    }

    #[test]
    fn redact_keys_applies_the_key_formats_too() {
        let gh = format!("ghp_{}", "a1B2".repeat(9));
        let body = format!("{{\"error\": \"bad token {gh}\"}}");
        assert_eq!(redact_keys(&body), "{\"error\": \"bad token [REDACTED:github-token]\"}");
    }

    #[test]
    fn private_key_blocks_including_cut_off_ones() {
        let pem = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nb";
        assert_eq!(shapes(pem).0, "a\n[REDACTED:private-key]\nb");
        assert_eq!(shapes("-----BEGIN RSA PRIVATE KEY-----\nAAAA").0, "[REDACTED:private-key]");
        let public = "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----";
        assert_eq!(shapes(public).0, public);
    }

    #[test]
    fn dotenv_files_are_read_and_templates_skipped() {
        let d = std::env::temp_dir().join(format!("mu-secrets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".env"), "# c\nexport STRIPE_SECRET=\"Sx9-abcdef12\"\nPORT=3000\n").unwrap();
        std::fs::write(d.join(".env.example"), "OTHER_TOKEN=Ex4mple-value\n").unwrap();
        let k = dotenv_within(&d, Duration::ZERO);
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].hit, Hit { label: "STRIPE_SECRET".into(), source: Source::DotEnv });
        // A changed file is read again.
        std::fs::write(d.join(".env"), "STRIPE_SECRET=Nw-value-9999\n").unwrap();
        assert_eq!(dotenv_within(&d, Duration::ZERO)[0].value, "Nw-value-9999");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn guard_refuses_writing_a_placeholder_back() {
        let h = Hit { label: "GUARD_TEST_TOKEN".into(), source: Source::Env };
        EMITTED.lock().unwrap().insert(h.clone());
        let cmd = serde_json::json!({"command": format!("curl -H 'X: {}'", h.placeholder())});
        let e = guard("bash", &cmd).unwrap_err();
        assert!(e.contains("\"$GUARD_TEST_TOKEN\""), "{e}");
        assert!(guard("edit", &serde_json::json!({"input": format!("+K={}", h.placeholder())})).is_err());
        assert!(guard("read", &cmd).is_ok());
        // A placeholder mu never handed out is ordinary text.
        assert!(guard("bash", &serde_json::json!({"command": "echo [REDACTED:NEVER_SEEN]"})).is_ok());
    }

    #[test]
    fn feedback_names_the_variables() {
        let f = feedback(&[
            Hit { label: "MODAL_TOKEN".into(), source: Source::Env },
            Hit { label: "github-token".into(), source: Source::Shape },
        ]);
        assert!(f.contains("[REDACTED:MODAL_TOKEN] (environment variable)"), "{f}");
        assert!(f.contains("\"$MODAL_TOKEN\""), "{f}");
    }
}
