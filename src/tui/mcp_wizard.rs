//! The interactive `/mcp add` flow as a state machine: arrow-key pickers for choices, text lines
//! for values. Pure: the app shows each `Step` and feeds back an `Answer`.

use crate::plugin::mcp::discover::Found;
use crate::plugin::mcp::store::{Level, check_name};
use crate::tui::mcp_cmd::{Draft, check_url, http_entry, parse_env, parse_header, split_words, stdio_entry};
use crate::tui::mcp_presets::{Preset, presets};
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// A picker; answer with `Answer::Pick(index)`.
    Pick { title: String, items: Vec<PickItem> },
    /// A line of text. The prompt is shown verbatim (it states any default). An empty answer
    /// takes the default when there is one; `default: Some("")` means the step is optional.
    Text { prompt: String, default: Option<String> },
    /// Finished: add this server.
    Done(Draft),
    /// Finished: the user declined at the confirmation; add nothing.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Pick(usize),
    Text(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    Discovered,
    Popular,
    Command,
    Url,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Source,
    Discovered,
    Popular,
    Param(usize),
    CmdLine,
    CmdEnv,
    Url,
    Headers,
    Level,
    Name,
    Confirm,
    Done,
    Cancelled,
}

pub struct Wizard {
    found: Vec<Found>,
    presets: Vec<Preset>,
    phase: Phase,
    preset: usize,
    answers: Vec<String>,
    command: Vec<String>,
    url: String,
    entry: Value,
    default_name: Option<String>,
    level: Level,
    name: String,
    notes: Vec<String>,
}

impl Wizard {
    /// Starts at the choice of source: discovered servers (when `found` is not empty), popular
    /// presets, a custom command or a custom url. `env_set` tells whether an environment
    /// variable is set (used to default tokens to `${VAR}`).
    pub fn new(found: Vec<Found>, cwd: &Path, env_set: fn(&str) -> bool) -> Self {
        Self {
            found,
            presets: presets(cwd, env_set),
            phase: Phase::Source,
            preset: 0,
            answers: Vec::new(),
            command: Vec::new(),
            url: String::new(),
            entry: Value::Null,
            default_name: None,
            level: Level::User,
            name: String::new(),
            notes: Vec::new(),
        }
    }

    /// Starts at the picker of discovered servers (at the choice of source when there are none).
    pub fn import(found: Vec<Found>, cwd: &Path, env_set: fn(&str) -> bool) -> Self {
        let mut w = Self::new(found, cwd, env_set);
        if !w.found.is_empty() {
            w.phase = Phase::Discovered;
        }
        w
    }

    pub fn step(&self) -> Step {
        match self.phase {
            Phase::Source => Step::Pick { title: "Add an MCP server".into(), items: self.source_items() },
            Phase::Discovered => Step::Pick {
                title: "Import a server".into(),
                items: self
                    .found
                    .iter()
                    .map(|f| PickItem {
                        label: f.name.clone(),
                        detail: format!("{} · {} · {}", f.tool, f.kind, f.target),
                    })
                    .collect(),
            },
            Phase::Popular => Step::Pick {
                title: "Popular servers".into(),
                items: self
                    .presets
                    .iter()
                    .map(|p| PickItem { label: p.label.into(), detail: p.description.into() })
                    .collect(),
            },
            Phase::Param(i) => {
                let p = &self.presets[self.preset].params[i];
                Step::Text { prompt: p.prompt.clone(), default: p.default.clone() }
            }
            Phase::CmdLine => Step::Text {
                prompt: "Command line, e.g. npx -y @modelcontextprotocol/server-filesystem .".into(),
                default: None,
            },
            Phase::CmdEnv => Step::Text {
                prompt: "Environment as K=V K2=V2 (optional; ${VAR} references work)".into(),
                default: Some(String::new()),
            },
            Phase::Url => Step::Text { prompt: "Server url, e.g. https://example.com/mcp".into(), default: None },
            Phase::Headers => Step::Text {
                prompt: "Headers as quoted words, e.g. \"Authorization: Bearer ${TOKEN}\" X-Foo=bar (optional)".into(),
                default: Some(String::new()),
            },
            Phase::Level => Step::Pick {
                title: "Save to".into(),
                items: vec![
                    PickItem { label: "User".into(), detail: "all your projects".into() },
                    PickItem { label: "Project".into(), detail: "this directory only (.omp/mcp.json)".into() },
                ],
            },
            Phase::Name => match &self.default_name {
                Some(d) => Step::Text { prompt: format!("Server name [{d}]"), default: Some(d.clone()) },
                None => Step::Text {
                    prompt: "Server name (letters, digits, `_` and `-`)".into(),
                    default: None,
                },
            },
            Phase::Confirm => Step::Pick {
                title: "Add this server?".into(),
                items: vec![
                    PickItem { label: "Add and connect".into(), detail: String::new() },
                    PickItem { label: "Cancel".into(), detail: String::new() },
                ],
            },
            Phase::Done => Step::Done(Draft { level: self.level, name: self.name.clone(), entry: self.entry.clone() }),
            Phase::Cancelled => Step::Cancelled,
        }
    }

    /// `Err(message)`: the answer was not usable and `step()` is the same as before.
    pub fn answer(&mut self, a: Answer) -> Result<(), String> {
        match (self.phase, a) {
            (Phase::Done | Phase::Cancelled, _) => Err("the wizard has finished".into()),
            (
                Phase::Source | Phase::Discovered | Phase::Popular | Phase::Level | Phase::Confirm,
                Answer::Pick(i),
            ) => self.pick(i),
            (Phase::Source | Phase::Discovered | Phase::Popular | Phase::Level | Phase::Confirm, Answer::Text(_)) => {
                Err("choose one of the options".into())
            }
            (_, Answer::Text(s)) => self.text(s.trim()),
            (_, Answer::Pick(_)) => Err("type an answer".into()),
        }
    }

    /// Lines to print before showing `step()` (a summary, a warning); drained.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn sources(&self) -> Vec<Source> {
        let mut v = Vec::new();
        if !self.found.is_empty() {
            v.push(Source::Discovered);
        }
        v.extend([Source::Popular, Source::Command, Source::Url]);
        v
    }

    fn source_items(&self) -> Vec<PickItem> {
        self.sources()
            .into_iter()
            .map(|s| match s {
                Source::Discovered => {
                    let mut tools: Vec<&str> = Vec::new();
                    for f in &self.found {
                        if !tools.contains(&f.tool.as_str()) {
                            tools.push(&f.tool);
                        }
                    }
                    PickItem {
                        label: format!("Discovered on this machine ({})", self.found.len()),
                        detail: tools.join(", "),
                    }
                }
                Source::Popular => {
                    let mut labels: Vec<&str> = self.presets.iter().take(4).map(|p| p.label).collect();
                    labels.push("…");
                    PickItem { label: "Popular servers".into(), detail: labels.join(", ") }
                }
                Source::Command => PickItem {
                    label: "Custom command (stdio)".into(),
                    detail: "a program that speaks MCP over stdin/stdout".into(),
                },
                Source::Url => {
                    PickItem { label: "Custom url (http)".into(), detail: "a remote server over http(s)".into() }
                }
            })
            .collect()
    }

    fn pick(&mut self, i: usize) -> Result<(), String> {
        let out_of_range = || format!("choice {} is out of range", i + 1);
        match self.phase {
            Phase::Source => {
                let source = *self.sources().get(i).ok_or_else(out_of_range)?;
                self.phase = match source {
                    Source::Discovered => Phase::Discovered,
                    Source::Popular => Phase::Popular,
                    Source::Command => Phase::CmdLine,
                    Source::Url => Phase::Url,
                };
            }
            Phase::Discovered => {
                let f = self.found.get(i).ok_or_else(out_of_range)?;
                let copied = literal_values(&f.entry);
                if copied > 0 {
                    self.notes.push(format!(
                        "Copies {copied} env value(s)/header(s) from {} as they are (they may be tokens).",
                        f.tool
                    ));
                }
                self.entry = f.entry.clone();
                self.default_name = sanitize_name(&f.name);
                self.phase = Phase::Level;
            }
            Phase::Popular => {
                let p = self.presets.get(i).ok_or_else(out_of_range)?;
                self.preset = i;
                self.answers.clear();
                self.phase = if p.params.is_empty() {
                    self.finish_preset();
                    Phase::Level
                } else {
                    Phase::Param(0)
                };
            }
            Phase::Level => {
                self.level = match i {
                    0 => Level::User,
                    1 => Level::Project,
                    _ => return Err(out_of_range()),
                };
                self.phase = Phase::Name;
            }
            Phase::Confirm => {
                self.phase = match i {
                    0 => Phase::Done,
                    1 => Phase::Cancelled,
                    _ => return Err(out_of_range()),
                };
            }
            _ => unreachable!("pick is only called for picker phases"),
        }
        Ok(())
    }

    fn text(&mut self, s: &str) -> Result<(), String> {
        match self.phase {
            Phase::Param(i) => {
                let (v, secret, count) = {
                    let preset = &self.presets[self.preset];
                    let p = &preset.params[i];
                    (resolve(s, &p.default, p.key)?, p.secret, preset.params.len())
                };
                if secret && !is_ref(&v) {
                    self.notes.push(
                        "The token is written to the config file as typed; use a ${ENV_VAR} reference to keep it out."
                            .into(),
                    );
                }
                self.answers.push(v);
                if i + 1 < count {
                    self.phase = Phase::Param(i + 1);
                } else {
                    self.finish_preset();
                    self.phase = Phase::Level;
                }
            }
            Phase::CmdLine => {
                let words = split_words(&resolve(s, &None, "a command")?)?;
                if words.first().is_none_or(|w| w.is_empty()) {
                    return Err("a command is required".into());
                }
                self.command = words;
                self.phase = Phase::CmdEnv;
            }
            Phase::CmdEnv => {
                let mut env = Map::new();
                for w in split_words(s)? {
                    let (k, v) = parse_env(&w)?;
                    env.insert(k, Value::String(v));
                }
                self.entry = stdio_entry(&self.command[0], &self.command[1..], env, None);
                self.default_name = command_name(&self.command[0]);
                self.phase = Phase::Level;
            }
            Phase::Url => {
                let url = resolve(s, &None, "a url")?;
                check_url(&url)?;
                self.url = url;
                self.phase = Phase::Headers;
            }
            Phase::Headers => {
                let mut headers = Map::new();
                for w in split_words(s)? {
                    let (k, v) = parse_header(&w)?;
                    headers.insert(k, Value::String(v));
                }
                self.entry = http_entry(&self.url, headers, None);
                self.default_name = url_host(&self.url).and_then(sanitize_name);
                self.phase = Phase::Level;
            }
            Phase::Name => {
                let name = resolve(s, &self.default_name, "a name")?;
                check_name(&name)?;
                self.name = name;
                self.notes.extend(self.summary());
                self.phase = Phase::Confirm;
            }
            _ => unreachable!("text is only called for text phases"),
        }
        Ok(())
    }

    fn finish_preset(&mut self) {
        let p = &self.presets[self.preset];
        self.entry = p.build(&self.answers);
        self.default_name = Some(p.id.to_string());
    }

    fn summary(&self) -> Vec<String> {
        let e = &self.entry;
        let level = match self.level {
            Level::User => "user",
            Level::Project => "project",
        };
        let http = e["type"].as_str() == Some("http") || (e["type"].is_null() && e["url"].is_string());
        let mut lines = vec![format!("{} · {level} · {}", self.name, if http { "http" } else { "stdio" })];
        if http {
            lines.push(format!("  url: {}", display_url(e["url"].as_str().unwrap_or_default())));
        } else {
            let mut cmd = vec![quote(e["command"].as_str().unwrap_or_default())];
            if let Some(args) = e["args"].as_array() {
                cmd.extend(args.iter().map(|a| quote(a.as_str().unwrap_or_default())));
            }
            lines.push(format!("  command: {}", cmd.join(" ")));
        }
        for key in ["env", "headers"] {
            if let Some(m) = e[key].as_object().filter(|m| !m.is_empty()) {
                let items: Vec<String> = m.iter().map(|(k, v)| format!("{k}={}", masked(v))).collect();
                lines.push(format!("  {key}: {}", items.join(", ")));
            }
        }
        lines
    }
}

/// Text answer: empty takes the default; no default means required.
fn resolve(s: &str, default: &Option<String>, what: &str) -> Result<String, String> {
    if !s.is_empty() {
        return Ok(s.to_string());
    }
    default.clone().ok_or_else(|| format!("{what} is required"))
}

fn is_ref(v: &str) -> bool {
    v.find("${").is_some_and(|i| v[i..].contains('}'))
}

fn masked(v: &Value) -> String {
    match v.as_str() {
        Some(s) if is_ref(s) => s.to_string(),
        _ => "•••".to_string(),
    }
}

/// How many env and header values are literals rather than `${…}` references.
fn literal_values(entry: &Value) -> usize {
    ["env", "headers"]
        .iter()
        .filter_map(|k| entry[*k].as_object())
        .flat_map(|m| m.values())
        .filter(|v| !v.as_str().is_some_and(is_ref))
        .count()
}

fn quote(a: &str) -> String {
    if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\') {
        return a.to_string();
    }
    format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A valid server name made of `s`, or `None` when nothing usable is left.
fn sanitize_name(s: &str) -> Option<String> {
    let mut n: String =
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    n.truncate(48);
    let n = n.trim_matches('-').to_string();
    check_name(&n).ok().map(|_| n)
}

fn command_name(command: &str) -> Option<String> {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let stem = base.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map_or(base, |(s, _)| s);
    sanitize_name(stem)
}

/// `(scheme, authority without userinfo, the rest from the path on)` for `scheme://…`.
fn split_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some((scheme, authority, &rest[end..]))
}

fn url_host(url: &str) -> Option<&str> {
    let (_, authority, _) = split_url(url)?;
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };
    Some(host)
}

/// The url without userinfo, query and fragment.
fn display_url(url: &str) -> String {
    match split_url(url) {
        Some((scheme, authority, rest)) => {
            let path = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
            format!("{scheme}://{authority}{path}")
        }
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn none_set(_: &str) -> bool {
        false
    }

    fn found(tool: &str, name: &str, entry: Value) -> Found {
        let http = entry["type"] == "http";
        Found {
            tool: tool.into(),
            file: PathBuf::from("/x/mcp.json"),
            name: name.into(),
            kind: if http { "http" } else { "stdio" },
            target: if http { "https://h".into() } else { entry["command"].as_str().unwrap().into() },
            entry,
        }
    }

    fn stdio_found() -> Found {
        found(
            "Claude Code",
            "fs",
            json!({"type": "stdio", "command": "npx", "args": ["-y", "pkg"], "env": {"TOKEN": "abc", "R": "${R}"}}),
        )
    }

    fn http_found() -> Found {
        found(
            "Codex",
            "api",
            json!({"type": "http", "url": "https://h/mcp", "headers": {"Authorization": "Bearer ${T}"}}),
        )
    }

    fn wiz(f: Vec<Found>) -> Wizard {
        Wizard::new(f, Path::new("/work/proj"), none_set)
    }

    fn pick(w: &mut Wizard, i: usize) {
        w.answer(Answer::Pick(i)).unwrap_or_else(|e| panic!("pick {i}: {e}"));
    }

    fn text(w: &mut Wizard, s: &str) {
        w.answer(Answer::Text(s.into())).unwrap_or_else(|e| panic!("text `{s}`: {e}"));
    }

    fn text_err(w: &mut Wizard, s: &str) -> String {
        let before = w.step();
        let e = w.answer(Answer::Text(s.into())).expect_err(s);
        assert_eq!(w.step(), before, "step changed after a rejected answer");
        e
    }

    fn pick_err(w: &mut Wizard, i: usize) -> String {
        let before = w.step();
        let e = w.answer(Answer::Pick(i)).expect_err("pick");
        assert_eq!(w.step(), before, "step changed after a rejected answer");
        e
    }

    fn items(w: &Wizard) -> (String, Vec<PickItem>) {
        match w.step() {
            Step::Pick { title, items } => (title, items),
            other => panic!("expected a picker, got {other:?}"),
        }
    }

    fn prompt(w: &Wizard) -> (String, Option<String>) {
        match w.step() {
            Step::Text { prompt, default } => (prompt, default),
            other => panic!("expected a text step, got {other:?}"),
        }
    }

    fn done(w: &Wizard) -> Draft {
        match w.step() {
            Step::Done(d) => d,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Level pick, name answer, then the confirmation; returns the summary.
    fn finish(w: &mut Wizard, level: usize, name: &str) -> Vec<String> {
        assert_eq!(items(w).0, "Save to");
        pick(w, level);
        w.take_notes();
        text(w, name);
        let summary = w.take_notes();
        let (title, it) = items(w);
        assert_eq!(title, "Add this server?");
        assert_eq!(it.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["Add and connect", "Cancel"]);
        pick(w, 0);
        summary
    }

    #[test]
    fn source_step_lists_every_source() {
        let w = wiz(vec![stdio_found(), http_found(), found("Codex", "b", json!({"type": "stdio", "command": "c"}))]);
        let (title, it) = items(&w);
        assert_eq!(title, "Add an MCP server");
        let labels: Vec<_> = it.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Discovered on this machine (3)", "Popular servers", "Custom command (stdio)", "Custom url (http)"]
        );
        assert_eq!(it[0].detail, "Claude Code, Codex");

        let (_, it) = items(&wiz(vec![]));
        assert_eq!(it.len(), 3);
        assert_eq!(it[0].label, "Popular servers");
    }

    #[test]
    fn discovered_stdio_full_run() {
        let mut w = wiz(vec![http_found(), stdio_found()]);
        pick(&mut w, 0);
        let (title, it) = items(&w);
        assert_eq!(title, "Import a server");
        assert_eq!(it[1], PickItem { label: "fs".into(), detail: "Claude Code · stdio · npx".into() });
        assert_eq!(it[0].detail, "Codex · http · https://h");
        pick(&mut w, 1);
        assert_eq!(
            w.take_notes(),
            ["Copies 1 env value(s)/header(s) from Claude Code as they are (they may be tokens)."]
        );
        pick(&mut w, 1);
        assert_eq!(prompt(&w), ("Server name [fs]".into(), Some("fs".into())));
        text(&mut w, "");
        let summary = w.take_notes();
        assert_eq!(summary[0], "fs · project · stdio");
        assert_eq!(summary[1], "  command: npx -y pkg");
        assert_eq!(summary[2], "  env: TOKEN=•••, R=${R}");
        assert!(!summary.concat().contains("abc"));
        pick(&mut w, 0);
        assert_eq!(done(&w), Draft { level: Level::Project, name: "fs".into(), entry: stdio_found().entry });
    }

    #[test]
    fn discovered_http_full_run() {
        let mut w = wiz(vec![http_found()]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        assert!(w.take_notes().is_empty(), "only references: nothing to warn about");
        pick(&mut w, 0);
        text(&mut w, "mine");
        let summary = w.take_notes();
        assert_eq!(summary[0], "mine · user · http");
        assert_eq!(summary[1], "  url: https://h/mcp");
        assert_eq!(summary[2], "  headers: Authorization=Bearer ${T}");
        pick(&mut w, 0);
        assert_eq!(done(&w), Draft { level: Level::User, name: "mine".into(), entry: http_found().entry });
    }

    #[test]
    fn discovered_name_is_made_valid() {
        let mut w = wiz(vec![found("T", "my.server v2", json!({"type": "stdio", "command": "c"}))]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        pick(&mut w, 0);
        assert_eq!(prompt(&w).1.as_deref(), Some("my-server-v2"));
    }

    #[test]
    fn warning_counts_headers_and_env() {
        let f = found(
            "T",
            "x",
            json!({"type": "http", "url": "https://h", "headers": {"A": "1", "B": "${B}", "C": "3"}}),
        );
        let mut w = wiz(vec![f]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        assert_eq!(w.take_notes(), ["Copies 2 env value(s)/header(s) from T as they are (they may be tokens)."]);
    }

    #[test]
    fn every_preset_runs_to_done() {
        let ps = presets(Path::new("/work/proj"), none_set);
        for (i, p) in ps.iter().enumerate() {
            let mut w = wiz(vec![]);
            pick(&mut w, 0);
            let (title, it) = items(&w);
            assert_eq!(title, "Popular servers");
            assert_eq!(it[i], PickItem { label: p.label.into(), detail: p.description.into() });
            pick(&mut w, i);
            let mut answers = Vec::new();
            for q in &p.params {
                let (pr, default) = prompt(&w);
                assert_eq!(pr, q.prompt);
                assert_eq!(default, q.default);
                let a = if q.default.is_some() { "" } else { "${MY_TOKEN}" };
                answers.push(q.default.clone().unwrap_or_else(|| a.to_string()));
                text(&mut w, a);
            }
            finish(&mut w, 0, "");
            let d = done(&w);
            assert_eq!(d.name, p.id);
            assert_eq!(d.level, Level::User);
            assert_eq!(d.entry, p.build(&answers), "{}", p.id);
            assert!(d.entry["type"].is_string());
        }
    }

    #[test]
    fn filesystem_preset_defaults_to_cwd_and_accepts_a_directory() {
        let mut w = wiz(vec![]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        assert_eq!(
            prompt(&w),
            ("Directory the server may access [/work/proj]".into(), Some("/work/proj".into()))
        );
        text(&mut w, "/srv/data");
        finish(&mut w, 1, "files");
        let d = done(&w);
        assert_eq!(d.name, "files");
        assert_eq!(
            d.entry,
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/srv/data"]})
        );
    }

    #[test]
    fn github_without_env_default_requires_a_token() {
        let mut w = wiz(vec![]);
        pick(&mut w, 0);
        pick(&mut w, 7);
        let (pr, default) = prompt(&w);
        assert!(pr.contains("${ENV_VAR}"));
        assert_eq!(default, None);
        assert!(text_err(&mut w, "").contains("required"));

        text(&mut w, "ghp_secret");
        let notes = w.take_notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("as typed"));
        pick(&mut w, 0);
        text(&mut w, "");
        let summary = w.take_notes();
        assert_eq!(summary[0], "github · user · http");
        assert_eq!(summary[1], "  url: https://api.githubcopilot.com/mcp/");
        assert_eq!(summary[2], "  headers: Authorization=•••");
        assert!(!summary.concat().contains("ghp_secret"));
        pick(&mut w, 0);
        assert_eq!(done(&w).entry["headers"]["Authorization"], "Bearer ghp_secret");
    }

    #[test]
    fn github_with_env_default_stores_the_reference() {
        let mut w = Wizard::new(vec![], Path::new("/w"), |v| v == "GITHUB_TOKEN");
        pick(&mut w, 0);
        pick(&mut w, 7);
        assert_eq!(prompt(&w).1.as_deref(), Some("${GITHUB_TOKEN}"));
        text(&mut w, "");
        assert!(w.take_notes().is_empty(), "a reference needs no warning");
        pick(&mut w, 0);
        text(&mut w, "");
        let summary = w.take_notes();
        assert_eq!(summary[2], "  headers: Authorization=Bearer ${GITHUB_TOKEN}");
        pick(&mut w, 0);
        assert_eq!(done(&w).entry["headers"]["Authorization"], "Bearer ${GITHUB_TOKEN}");
    }

    #[test]
    fn custom_stdio_full_run() {
        let mut w = wiz(vec![]);
        pick(&mut w, 1);
        let (pr, default) = prompt(&w);
        assert_eq!(pr, "Command line, e.g. npx -y @modelcontextprotocol/server-filesystem .");
        assert_eq!(default, None);
        assert!(text_err(&mut w, "").contains("required"));
        assert!(text_err(&mut w, "''").contains("required"));
        assert!(text_err(&mut w, "npx 'oops").contains("unterminated"));
        text(&mut w, r#"/usr/local/bin/my-server.py --root "a b""#);
        let (pr, default) = prompt(&w);
        assert_eq!(pr, "Environment as K=V K2=V2 (optional; ${VAR} references work)");
        assert_eq!(default.as_deref(), Some(""));
        assert!(text_err(&mut w, "NOEQ").contains("K=V"));
        text(&mut w, "A=1 B=${X}");
        pick(&mut w, 0);
        assert_eq!(prompt(&w), ("Server name [my-server]".into(), Some("my-server".into())));
        text(&mut w, "");
        let summary = w.take_notes();
        assert_eq!(summary[0], "my-server · user · stdio");
        assert_eq!(summary[1], "  command: /usr/local/bin/my-server.py --root \"a b\"");
        assert_eq!(summary[2], "  env: A=•••, B=${X}");
        pick(&mut w, 0);
        assert_eq!(
            done(&w).entry,
            json!({
                "type": "stdio",
                "command": "/usr/local/bin/my-server.py",
                "args": ["--root", "a b"],
                "env": {"A": "1", "B": "${X}"}
            })
        );
    }

    #[test]
    fn custom_stdio_without_env_and_default_name() {
        let mut w = wiz(vec![]);
        pick(&mut w, 1);
        text(&mut w, "node server.js");
        text(&mut w, "");
        pick(&mut w, 0);
        assert_eq!(prompt(&w).1.as_deref(), Some("node"));
        text(&mut w, "n1");
        pick(&mut w, 0);
        assert_eq!(done(&w).entry, json!({"type": "stdio", "command": "node", "args": ["server.js"]}));
    }

    #[test]
    fn custom_stdio_without_a_usable_default_name_requires_one() {
        let mut w = wiz(vec![]);
        pick(&mut w, 1);
        text(&mut w, "./...");
        text(&mut w, "");
        pick(&mut w, 0);
        let (pr, default) = prompt(&w);
        assert_eq!(default, None);
        assert!(pr.starts_with("Server name"));
        assert!(text_err(&mut w, "").contains("required"));
    }

    #[test]
    fn custom_http_full_run() {
        let mut w = wiz(vec![]);
        pick(&mut w, 2);
        assert_eq!(prompt(&w), ("Server url, e.g. https://example.com/mcp".into(), None));
        assert!(text_err(&mut w, "").contains("required"));
        assert!(text_err(&mut w, "ftp://x").contains("not an http"));
        assert!(text_err(&mut w, "example.com").contains("not an http"));
        text(&mut w, "https://user:pw@api.example.com:8443/mcp?key=secret#frag");
        let (pr, default) = prompt(&w);
        assert_eq!(pr, "Headers as quoted words, e.g. \"Authorization: Bearer ${TOKEN}\" X-Foo=bar (optional)");
        assert_eq!(default.as_deref(), Some(""));
        assert!(!text_err(&mut w, "novalue").is_empty());
        assert!(text_err(&mut w, "'unterminated").contains("unterminated"));
        text(&mut w, r#""Authorization: Bearer ${TOKEN}" X-Foo=bar"#);
        pick(&mut w, 1);
        assert_eq!(prompt(&w).1.as_deref(), Some("api-example-com"));
        text(&mut w, "");
        let summary = w.take_notes();
        assert_eq!(summary[0], "api-example-com · project · http");
        assert_eq!(summary[1], "  url: https://api.example.com:8443/mcp");
        assert_eq!(summary[2], "  headers: Authorization=Bearer ${TOKEN}, X-Foo=•••");
        let all = summary.concat();
        assert!(!all.contains("secret") && !all.contains("pw@") && !all.contains("frag"), "{all}");
        pick(&mut w, 0);
        assert_eq!(
            done(&w).entry,
            json!({
                "type": "http",
                "url": "https://user:pw@api.example.com:8443/mcp?key=secret#frag",
                "headers": {"Authorization": "Bearer ${TOKEN}", "X-Foo": "bar"}
            })
        );
    }

    #[test]
    fn custom_http_without_headers() {
        let mut w = wiz(vec![]);
        pick(&mut w, 2);
        text(&mut w, "http://localhost:1/mcp");
        text(&mut w, "  ");
        pick(&mut w, 0);
        assert_eq!(prompt(&w).1.as_deref(), Some("localhost"));
        text(&mut w, "");
        w.take_notes();
        pick(&mut w, 0);
        assert_eq!(done(&w).entry, json!({"type": "http", "url": "http://localhost:1/mcp"}));
    }

    #[test]
    fn invalid_names_keep_the_step() {
        let mut w = wiz(vec![]);
        pick(&mut w, 1);
        text(&mut w, "cmd");
        text(&mut w, "");
        pick(&mut w, 0);
        assert!(text_err(&mut w, "bad name").contains("not a valid server name"));
        assert!(text_err(&mut w, "a/b").contains("not a valid server name"));
        assert!(text_err(&mut w, &"x".repeat(49)).contains("not a valid server name"));
        assert!(w.take_notes().is_empty());
        text(&mut w, "ok_1-x");
        assert_eq!(items(&w).0, "Add this server?");
    }

    #[test]
    fn wrong_answer_kind_and_out_of_range_picks_keep_the_step() {
        let mut w = wiz(vec![stdio_found()]);
        assert!(text_err(&mut w, "1").contains("choose"));
        assert!(pick_err(&mut w, 4).contains("out of range"));
        pick(&mut w, 0);
        assert!(pick_err(&mut w, 1).contains("out of range"));
        pick(&mut w, 0);
        assert!(pick_err(&mut w, 2).contains("out of range"));
        pick(&mut w, 0);
        assert!(w.answer(Answer::Pick(0)).unwrap_err().contains("type an answer"));
        assert_eq!(prompt(&w).1.as_deref(), Some("fs"));
        text(&mut w, "");
        assert!(pick_err(&mut w, 2).contains("out of range"));
        assert!(text_err(&mut w, "yes").contains("choose"));

        let mut w = wiz(vec![]);
        pick(&mut w, 0);
        assert!(pick_err(&mut w, 8).contains("out of range"));
    }

    #[test]
    fn cancel_ends_without_a_draft() {
        let mut w = wiz(vec![]);
        pick(&mut w, 1);
        text(&mut w, "cmd");
        text(&mut w, "");
        pick(&mut w, 0);
        text(&mut w, "x");
        assert!(!w.take_notes().is_empty());
        pick(&mut w, 1);
        assert_eq!(w.step(), Step::Cancelled);
        assert!(w.answer(Answer::Pick(0)).is_err());
        assert_eq!(w.step(), Step::Cancelled);
    }

    #[test]
    fn finished_wizard_rejects_answers() {
        let mut w = wiz(vec![http_found()]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        finish(&mut w, 0, "");
        assert!(w.answer(Answer::Pick(0)).is_err());
        assert!(w.answer(Answer::Text("x".into())).is_err());
        assert_eq!(done(&w).name, "api");
    }

    #[test]
    fn import_starts_at_the_discovered_list() {
        let mut w = Wizard::import(vec![stdio_found(), http_found()], Path::new("/w"), none_set);
        let (title, it) = items(&w);
        assert_eq!(title, "Import a server");
        assert_eq!(it.len(), 2);
        pick(&mut w, 1);
        w.take_notes();
        finish(&mut w, 0, "");
        assert_eq!(done(&w).entry, http_found().entry);
    }

    #[test]
    fn import_with_nothing_found_falls_back_to_the_source_step() {
        let w = Wizard::import(vec![], Path::new("/w"), none_set);
        let (title, it) = items(&w);
        assert_eq!(title, "Add an MCP server");
        assert_eq!(it.len(), 3);
    }

    #[test]
    fn notes_are_drained() {
        let mut w = wiz(vec![stdio_found()]);
        pick(&mut w, 0);
        pick(&mut w, 0);
        assert_eq!(w.take_notes().len(), 1);
        assert!(w.take_notes().is_empty());
    }

    #[test]
    fn url_helpers() {
        assert_eq!(display_url("https://u:p@h.com:1/a/b?q=1#f"), "https://h.com:1/a/b");
        assert_eq!(display_url("http://h"), "http://h");
        assert_eq!(display_url("https://h?x=1"), "https://h");
        assert_eq!(url_host("https://u:p@h.com:1/a"), Some("h.com"));
        assert_eq!(url_host("http://[::1]:80/"), Some("[::1]"));
        assert_eq!(url_host("nourl"), None);
        assert_eq!(sanitize_name("[::1]"), Some("1".to_string()));
        assert_eq!(sanitize_name("..."), None);
        assert_eq!(command_name("C:\\bin\\srv.exe"), Some("srv".to_string()));
        assert_eq!(command_name("npx"), Some("npx".to_string()));
        assert_eq!(command_name(".hidden"), Some("hidden".to_string()));
    }

    #[test]
    fn references_are_recognised() {
        assert!(is_ref("${A}"));
        assert!(is_ref("Bearer ${A:-x}"));
        assert!(!is_ref("abc"));
        assert!(!is_ref("${unclosed"));
        assert_eq!(masked(&json!("${A}")), "${A}");
        assert_eq!(masked(&json!("a")), "•••");
        assert_eq!(masked(&json!(3)), "•••");
    }
}
