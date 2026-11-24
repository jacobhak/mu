mod at_imports;
pub mod modes;

pub use modes::*;

use std::path::{Path, PathBuf};

pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn non_empty(p: &Path) -> Option<ContextFile> {
    let content = std::fs::read_to_string(p).ok()?;
    (!content.trim().is_empty()).then(|| ContextFile { path: p.into(), content })
}

/// pi's agent dir, unless it is the one already read as omp's (omp honours `PI_CODING_AGENT_DIR` too).
fn pi_dir(home: &Path, agent_dir: &Path) -> Option<PathBuf> {
    let d = mu_common::paths::pi_agent_dir_in(home);
    (d != agent_dir).then_some(d)
}

/// What pi reads in one directory: `AGENTS.override.md` (which replaces the others there), `AGENTS.md`, `CLAUDE.md`.
fn dir_instructions(dir: &Path) -> Option<ContextFile> {
    ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"].iter().find_map(|n| non_empty(&dir.join(n)))
}

/// Project files farthest-first (cwd last), then the user file.
pub fn load(cwd: &Path, home: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut near_first = Vec::new();
    let mut took_omp = false;
    for dir in cwd.ancestors() {
        if cwd.starts_with(home) && dir == home {
            break;
        }
        if dir != cwd && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        let omp = (!took_omp).then(|| non_empty(&dir.join(".omp/AGENTS.md"))).flatten();
        if let Some(f) = omp {
            took_omp = true;
            near_first.push(f);
            continue;
        }
        if let Some(f) = dir_instructions(dir) {
            near_first.push(f);
        }
    }
    near_first.reverse();
    near_first.extend(non_empty(&agent_dir.join("AGENTS.md")));
    let pi = pi_dir(home, agent_dir);
    near_first.extend(pi.as_deref().and_then(dir_instructions));
    let mut roots = vec![cwd.to_path_buf(), agent_dir.to_path_buf(), home.join(".omp"), mu_common::paths::mu_home()];
    roots.extend(pi);
    for f in &mut near_first {
        f.content =
            at_imports::expand(&f.content, &f.path, home, &roots, &mut |w| WARNINGS.with(|x| x.borrow_mut().push(w)));
    }
    near_first
}

/// cwd-relative paths of AGENTS.md files 1–4 directories below cwd.
fn nested_agents_md(cwd: &Path) -> Vec<String> {
    use omp_xfs::{FileType, Fs, walk};
    use std::time::Duration;
    let opts = walk::Options { max_depth: Some(4), skip_node_modules: true, ..crate::walk::walk_options() };
    let dirs = crate::walk::block_on_walk(async {
        let fs = Fs::native();
        let collect = walk::collect(&fs, cwd, &opts, false, |_, e| {
            if e.file_type == FileType::Dir { walk::Action::Continue } else { walk::Action::Skip }
        });
        match tokio::time::timeout(Duration::from_secs(2), collect).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("timed out after 2s".to_string()),
        }
    });
    match dirs {
        Err(e) => {
            WARNINGS.with(|w| w.borrow_mut().push(format!("{}: nested AGENTS.md scan failed: {e}", cwd.display())));
            vec![]
        }
        Ok(dirs) => {
            let mut v: Vec<String> = dirs
                .iter()
                .filter(|d| cwd.join(d.rel_path()).join("AGENTS.md").is_file())
                .map(|d| format!("{}/AGENTS.md", d.path_lossy()))
                .collect();
            v.sort();
            v.truncate(200);
            v
        }
    }
}

/// Directories from `cwd` up to (excluding) `home`, skipping dot-named ancestors (never `cwd` itself).
fn walk(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in cwd.ancestors() {
        if cwd.starts_with(home) && dir == home {
            break;
        }
        if dir != cwd && dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        out.push(dir.to_path_buf());
    }
    out
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub hidden: bool,
}

#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub body: String,
    pub description: String,
    /// A pi prompt template: pi's argument quoting and `${1:-default}` / `${@:N:L}` substitutions apply.
    pub pi: bool,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Front {
    name: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    hide: bool,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

thread_local! {
    /// Problems met while reading context files; drained by `Context::load_all`.
    static WARNINGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Splits leading `---` YAML frontmatter from the body.
fn frontmatter<'a>(path: &Path, text: &'a str) -> (Front, &'a str) {
    frontmatter_as(path, text)
}

/// The front matter is a first line `---`, then YAML, then a line that is exactly `---`.
/// An empty block is fine; invalid YAML is reported (naming the file) and read as empty.
fn frontmatter_as<'a, T: serde::de::DeserializeOwned + Default>(path: &Path, text: &'a str) -> (T, &'a str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (T::default(), text);
    };
    let mut pos = 0;
    let mut close = None;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            close = Some((pos, pos + line.len()));
            break;
        }
        pos += line.len();
    }
    let Some((yaml_end, body_start)) = close else { return (T::default(), text) };
    let yaml = &rest[..yaml_end];
    let front = if yaml.trim().is_empty() {
        T::default()
    } else {
        serde_yaml_ng::from_str(yaml).unwrap_or_else(|e| {
            WARNINGS.with(|w| w.borrow_mut().push(format!("{}: invalid front matter: {e}", path.display())));
            T::default()
        })
    };
    (front, &rest[body_start..])
}

/// Skill roots in precedence order; the flag marks roots searched recursively, as pi does (the others are one level deep).
fn skill_roots(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool) -> Vec<(PathBuf, bool)> {
    let dirs = walk(cwd, home);
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    roots.extend(dirs.iter().map(|d| (d.join(".omp/skills"), false)));
    roots.extend(dirs.iter().map(|d| (d.join(".pi/skills"), true)));
    roots.push((agent_dir.join("skills"), false));
    roots.extend(pi_dir(home, agent_dir).map(|d| (d.join("skills"), true)));
    roots.extend(dirs.iter().map(|d| (d.join(".claude/skills"), false)));
    if claude_user {
        roots.push((home.join(".claude/skills"), false));
    }
    for d in &dirs {
        roots.push((d.join(".agents/skills"), false));
        roots.push((d.join(".agent/skills"), false));
    }
    roots.push((home.join(".agents/skills"), false));
    roots
}

/// Directories under `root` holding a `SKILL.md`: the search stops descending where one is found, and skips
/// dot-directories and `node_modules`.
fn nested_skill_dirs(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if dir.join("SKILL.md").is_file() {
            out.push(dir);
        } else if depth > 1 {
            nested_skill_dirs(&dir, depth - 1, out);
        }
    }
}

pub fn discover_skills(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (root, recursive) in skill_roots(cwd, home, agent_dir, claude_user) {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if recursive {
            nested_skill_dirs(&root, 6, &mut dirs);
        } else {
            let Ok(rd) = std::fs::read_dir(&root) else { continue };
            dirs = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
        }
        for dir in dirs {
            let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else { continue };
            let (f, _) = frontmatter(&dir.join("SKILL.md"), &text);
            let dir_name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let name = f.name.unwrap_or(dir_name);
            let Some(description) = f.description.filter(|d| !d.trim().is_empty()) else { continue };
            if name.contains('/') || f.enabled == Some(false) || out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill {
                name,
                description: description.trim().into(),
                dir,
                hidden: f.hide || f.disable_model_invocation,
            });
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Vec<String>,
    pub thinking: Option<String>,
    pub prompt: String,
    /// Result schema from frontmatter `output:` (JTD converted); `None` = free-text result.
    pub output: Option<serde_json::Value>,
}

pub const REVIEW_LEAD: &str = "review-lead";

/// The scout every `task` call gets unless it names another agent: read-only, no shell.
pub const SCOUT: &str = "scout";
/// The default worker of a `task` call: it reads, edits and runs commands.
pub const TASK: &str = "task";

/// Agents mu ships. They are not in `Context::agents`, so the main agent's `task` tool never offers them.
pub fn builtin_agent(name: &str) -> Option<AgentDef> {
    let tools = |t: &[&str]| Some(t.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let model =
        |role: &str| -> Vec<String> { mu_base::models::role_chain(role).iter().map(|r| format!("@{r}")).collect() };
    let (description, tools, role, prompt) = match name {
        SCOUT => (
            "Read-only research scouts",
            tools(&["read", "grep", "glob", "web_search", "fetch"]),
            SCOUT,
            include_str!("../prompts/scout.md"),
        ),
        // No allowlist: a worker gets every tool the run offers, MCP and plugin tools included.
        TASK => ("A worker that reads, edits and runs commands", None, TASK, include_str!("../prompts/task.md")),
        REVIEW_LEAD => (
            "Lead code reviewer",
            tools(&["read", "grep", "glob", "bash", "task"]),
            "review",
            include_str!("../prompts/review-lead.md"),
        ),
        _ => return None,
    };
    Some(AgentDef {
        name: name.into(),
        description: description.into(),
        tools,
        model: model(role),
        thinking: None,
        prompt: prompt.into(),
        output: None,
    })
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct AgentFront {
    name: Option<String>,
    description: Option<String>,
    tools: Option<serde_yaml_ng::Value>,
    model: Option<serde_yaml_ng::Value>,
    #[serde(rename = "thinkingLevel")]
    thinking_level: Option<String>,
    output: Option<serde_json::Value>,
}

/// A YAML sequence of strings, or one string split on `,`.
fn string_list(v: &serde_yaml_ng::Value) -> Option<Vec<String>> {
    let items: Vec<String> = match v {
        serde_yaml_ng::Value::String(s) => s.split(',').map(|x| x.trim().to_string()).collect(),
        serde_yaml_ng::Value::Sequence(q) => {
            q.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).collect()
        }
        _ => return None,
    };
    Some(items.into_iter().filter(|x| !x.is_empty()).collect())
}

/// omp-format custom agents: `<dir>/.omp/agents/*.md` (nearest first), then `<agent_dir>/agents/*.md`.
pub fn discover_agents(cwd: &Path, home: &Path, agent_dir: &Path) -> Vec<AgentDef> {
    let mut roots: Vec<PathBuf> = walk(cwd, home).iter().map(|d| d.join(".omp/agents")).collect();
    roots.push(agent_dir.join("agents"));
    let mut out: Vec<AgentDef> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let mut files: Vec<PathBuf> =
            rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let (f, body) = frontmatter_as::<AgentFront>(&path, &text);
            let (Some(name), Some(description)) =
                (f.name.map(|n| n.trim().to_string()), f.description.map(|d| d.trim().to_string()))
            else {
                continue;
            };
            if name.is_empty() || description.is_empty() {
                continue;
            }
            let lower = name.to_lowercase();
            if ["main", "sub", "task", "scout", REVIEW_LEAD].contains(&lower.as_str())
                || out.iter().any(|a| a.name == name)
            {
                continue;
            }
            let output = match f.output.as_ref().map(mu_base::jtd::output_schema) {
                None => None,
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    WARNINGS.with(|w| w.borrow_mut().push(format!("{}: invalid `output` schema: {e}", path.display())));
                    continue;
                }
            };
            out.push(AgentDef {
                name,
                description,
                tools: f.tools.as_ref().and_then(string_list),
                model: f.model.as_ref().and_then(string_list).unwrap_or_default(),
                thinking: f.thinking_level,
                prompt: body.trim().to_string(),
                output,
            });
        }
    }
    out
}

pub fn discover_commands(cwd: &Path, home: &Path, agent_dir: &Path) -> Vec<SlashCommand> {
    let dirs = walk(cwd, home);
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    let mut add = |p: PathBuf| roots.push((p, false));
    dirs.iter().for_each(|d| add(d.join(".omp/commands")));
    add(agent_dir.join("commands"));
    dirs.iter().for_each(|d| add(d.join(".claude/commands")));
    dirs.iter().for_each(|d| add(d.join(".omp/prompts")));
    add(agent_dir.join("prompts"));
    roots.extend(dirs.iter().map(|d| (d.join(".pi/prompts"), true)));
    roots.extend(pi_dir(home, agent_dir).map(|d| (d.join("prompts"), true)));
    for d in dirs.iter().map(PathBuf::as_path).chain([home]) {
        roots.push((d.join(".agent/commands"), false));
        roots.push((d.join(".agents/commands"), false));
    }
    let mut out: Vec<SlashCommand> = Vec::new();
    for (root, pi) in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let mut files: Vec<PathBuf> =
            rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        files.sort();
        for f in files {
            let Some(name) = f.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            if out.iter().any(|c| c.name == name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let (front, body) = frontmatter(&f, &text);
            let description =
                front.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).unwrap_or_else(|| {
                    body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().chars().take(60).collect()
                });
            out.push(SlashCommand { name, body: body.trim_start_matches('\n').into(), description, pi });
        }
    }
    out
}

/// Whitespace split honouring double quotes.
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// pi's argument syntax: whitespace-separated, with `"…"` or `'…'` quoting only where an argument starts and
/// only when the quote closes at the end of an argument; `\"` inside keeps the quote. Anything else is plain text.
fn split_args_pi(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < c.len() {
        if c[i].is_whitespace() {
            i += 1;
            continue;
        }
        let q = c[i];
        if q == '"' || q == '\'' {
            let (mut j, mut arg) = (i + 1, String::new());
            let mut end = None;
            while j < c.len() {
                if c[j] == '\\' && c.get(j + 1) == Some(&q) {
                    arg.push(q);
                    j += 2;
                } else if c[j] == q {
                    end = Some(j);
                    break;
                } else {
                    arg.push(c[j]);
                    j += 1;
                }
            }
            if let Some(e) = end.filter(|&e| c.get(e + 1).is_none_or(|n| n.is_whitespace())) {
                out.push(arg);
                i = e + 1;
                continue;
            }
        }
        let start = i;
        while i < c.len() && !c[i].is_whitespace() {
            i += 1;
        }
        out.push(c[start..i].iter().collect());
    }
    out
}

/// `${…}` bodies of pi templates: `N:-default`, `@:-default`, `@:N`, `@:N:L` (positions count from 1).
fn pi_braced(body: &str, parts: &[String]) -> Option<String> {
    let joined = parts.join(" ");
    if let Some(d) = body.strip_prefix("@:-") {
        return Some(if joined.is_empty() { d.into() } else { joined });
    }
    if let Some(r) = body.strip_prefix("@:") {
        let (n, l) = match r.split_once(':') {
            Some((n, l)) => (n.parse::<usize>().ok()?, Some(l.parse::<usize>().ok()?)),
            None => (r.parse::<usize>().ok()?, None),
        };
        let from = n.saturating_sub(1).min(parts.len());
        let to = l.map_or(parts.len(), |l| (from + l).min(parts.len()));
        return Some(parts[from..to].join(" "));
    }
    let (n, d) = body.split_once(":-")?;
    let n = n.parse::<usize>().ok().filter(|n| *n >= 1)?;
    Some(parts.get(n - 1).filter(|p| !p.is_empty()).map_or(d, |p| p.as_str()).into())
}

impl SlashCommand {
    /// `$1..$9`, `$@`/`$ARGUMENTS`; args are appended when the body has no placeholder. A pi template also takes
    /// `$10`, `${1:-default}`, `${@:-default}`, `${@:N}` and `${@:N:L}`.
    pub fn expand(&self, args: &str) -> String {
        let parts = if self.pi { split_args_pi(args) } else { split_args(args) };
        let joined = parts.join(" ");
        let mut had = false;
        let mut out = String::with_capacity(self.body.len());
        let b = self.body.as_str();
        let mut i = 0;
        while i < b.len() {
            let rest = &b[i..];
            let braced = self
                .pi
                .then(|| rest.strip_prefix("${")?.split_once('}'))
                .flatten()
                .and_then(|(body, after)| Some((pi_braced(body, &parts)?, after)));
            if let Some((text, after)) = braced {
                out.push_str(&text);
                had = true;
                i = b.len() - after.len();
            } else if let Some(r) = rest.strip_prefix("$ARGUMENTS") {
                out.push_str(&joined);
                had = true;
                i = b.len() - r.len();
            } else if let Some(r) = rest.strip_prefix("$@") {
                out.push_str(&joined);
                had = true;
                i = b.len() - r.len();
            } else if rest.starts_with('$') && rest.as_bytes().get(1).is_some_and(|d| (b'1'..=b'9').contains(d)) {
                let digits = if self.pi { rest[1..].bytes().take_while(u8::is_ascii_digit).count() } else { 1 };
                let n: usize = rest[1..=digits].parse().unwrap_or(1);
                out.push_str(parts.get(n - 1).map_or("", |s| s.as_str()));
                had = true;
                i += 1 + digits;
            } else {
                let c = rest.chars().next().expect("rest is non-empty");
                out.push(c);
                i += c.len_utf8();
            }
        }
        if !had && !joined.is_empty() {
            out.push_str("\n\n");
            out.push_str(&joined);
        }
        out
    }
}

#[derive(Default)]
pub struct Context {
    /// Problems found while loading (bad front matter); shown as notices at startup.
    pub warnings: Vec<String>,
    pub files: Vec<ContextFile>,
    /// cwd-relative paths of AGENTS.md files 1–4 directories below cwd, listed in `<dir-context>`.
    pub nested: Vec<String>,
    pub rules: Vec<String>,
    pub skills: Vec<Skill>,
    pub commands: Vec<SlashCommand>,
    pub agents: Vec<AgentDef>,
    pub modes: Vec<ModeDef>,
}

impl Context {
    pub fn load_all(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool, mode_dir: &Path) -> Self {
        let mut rules = Vec::new();
        let user = std::fs::read_to_string(agent_dir.join("RULES.md")).ok();
        let project = walk(cwd, home).into_iter().find_map(|d| std::fs::read_to_string(d.join(".omp/RULES.md")).ok());
        // pi's APPEND_SYSTEM.md adds instructions to every request, as omp's RULES.md does.
        let pi_user = pi_dir(home, agent_dir).and_then(|d| std::fs::read_to_string(d.join("APPEND_SYSTEM.md")).ok());
        let pi_project =
            walk(cwd, home).into_iter().find_map(|d| std::fs::read_to_string(d.join(".pi/APPEND_SYSTEM.md")).ok());
        for r in [user, project, pi_user, pi_project].into_iter().flatten() {
            if !r.trim().is_empty() {
                rules.push(r.trim_end().to_string());
            }
        }
        WARNINGS.with(|w| w.borrow_mut().clear());
        let files = load(cwd, home, agent_dir);
        let nested = nested_agents_md(cwd);
        let skills = discover_skills(cwd, home, agent_dir, claude_user);
        let commands = discover_commands(cwd, home, agent_dir);
        let agents = discover_agents(cwd, home, agent_dir);
        let modes = discover_modes(cwd, home, mode_dir);
        for c in &commands {
            if modes.iter().any(|m| m.command && m.name == c.name) {
                let n = &c.name;
                WARNINGS.with(|w| w.borrow_mut().push(format!("command /{n} is shadowed by mode {n}")));
            }
        }
        let warnings = WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()));
        Self { warnings, files, nested, rules, skills, commands, agents, modes }
    }

    pub fn mode(&self, name: &str) -> Option<&ModeDef> {
        self.modes.iter().find(|m| m.name == name)
    }

    /// `/skill:<name> [args]` prompt text.
    pub fn skill_prompt(&self, name: &str, args: &str) -> Option<String> {
        let sk = self.skills.iter().find(|s| s.name == name)?;
        let path = sk.dir.join("SKILL.md");
        let text = std::fs::read_to_string(&path).ok()?;
        let (_, body) = frontmatter(&path, &text);
        let mut out = format!("<skill name=\"{name}\" path=\"{}\">\n{}\n</skill>", path.display(), body.trim());
        if !args.trim().is_empty() {
            out.push_str(&format!("\n\n{}", args.trim()));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn precedence_and_order() {
        let root = std::env::temp_dir().join(format!("mu-ctx-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("a/b");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::write(home.join("AGENTS.md"), "HOME").unwrap();
        fs::write(home.join("a/CLAUDE.md"), "A").unwrap();
        fs::write(proj.join("AGENTS.md"), "B-agents").unwrap();
        fs::write(proj.join("CLAUDE.md"), "B-claude").unwrap();
        fs::write(root.join("agent/AGENTS.md"), "USER").unwrap();
        let got: Vec<_> = load(&proj, &home, &root.join("agent")).into_iter().map(|f| f.content).collect();
        assert_eq!(got, ["A", "B-agents", "USER"]);
    }

    #[test]
    fn agents_precedence_formats_and_reserved_names() {
        let root = std::env::temp_dir().join(format!("mu-ctx3-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        fs::create_dir_all(proj.join(".omp/agents")).unwrap();
        fs::create_dir_all(agent.join("agents")).unwrap();
        let w = |p: std::path::PathBuf, t: &str| fs::write(p, t).unwrap();
        w(
            proj.join(".omp/agents/a.md"),
            "---\nname: a\ndescription: project a\ntools: read, grep\nmodel: \"@smol\"\nthinkingLevel: high\n---\nBODY\n",
        );
        w(agent.join("agents/a.md"), "---\nname: a\ndescription: user a\n---\nOTHER\n");
        w(
            agent.join("agents/b.md"),
            "---\nname: b\ndescription: user b\ntools: [read, write]\nmodel: [\"@smol\", \"@default\"]\n---\nB\n",
        );
        w(agent.join("agents/scout.md"), "---\nname: Scout\ndescription: reserved\n---\nx\n");
        w(agent.join("agents/main.md"), "---\nname: main\ndescription: reserved\n---\nx\n");
        w(agent.join("agents/nodesc.md"), "---\nname: nodesc\n---\nx\n");
        let got = discover_agents(&proj, &home, &agent);
        let names: Vec<_> = got.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(got[0].description, "project a");
        assert_eq!(got[0].tools.as_deref(), Some(&["read".to_string(), "grep".to_string()][..]));
        assert_eq!(got[0].model, ["@smol"]);
        assert_eq!(got[0].thinking.as_deref(), Some("high"));
        assert_eq!(got[0].prompt, "BODY");
        assert_eq!(got[1].tools.as_deref(), Some(&["read".to_string(), "write".to_string()][..]));
        assert_eq!(got[1].model, ["@smol", "@default"]);
        assert!(got[1].thinking.is_none());
        assert!(got.iter().all(|a| a.output.is_none()));
    }

    /// The built-in subagents carry their own instructions: without them a scout ran the main
    /// agent's prompt and was told to use a shell it does not have.
    #[test]
    fn builtin_agents_carry_instructions_and_tools() {
        let scout = builtin_agent(SCOUT).unwrap();
        assert_eq!(
            scout.tools.as_deref(),
            Some(&["read".to_string(), "grep".into(), "glob".into(), "web_search".into(), "fetch".into()][..])
        );
        assert!(scout.prompt.contains("no shell") && scout.prompt.contains("<output>"));
        let worker = builtin_agent(TASK).unwrap();
        assert!(worker.tools.is_none(), "a worker gets every tool the run offers, MCP and plugin tools included");
        assert!(worker.prompt.contains("final message is the result"));
        assert_eq!(
            builtin_agent(REVIEW_LEAD).unwrap().model,
            mu_base::models::role_chain("review").iter().map(|r| format!("@{r}")).collect::<Vec<_>>()
        );
        assert!(builtin_agent("nope").is_none());
    }

    #[test]
    fn agent_with_bad_output_is_skipped_with_warning() {
        let root = std::env::temp_dir().join(format!("mu-ctx4-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        fs::create_dir_all(proj.join(".omp/agents")).unwrap();
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            proj.join(".omp/agents/bad.md"),
            "---\nname: bad\ndescription: d\noutput:\n  properties:\n    a:\n      ref: T\n---\nx\n",
        )
        .unwrap();
        fs::write(proj.join(".omp/agents/ok.md"), "---\nname: ok\ndescription: d\n---\nx\n").unwrap();
        WARNINGS.with(|w| w.borrow_mut().clear());
        let got = discover_agents(&proj, &home, &agent);
        let names: Vec<_> = got.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["ok"]);
        let w = WARNINGS.with(|w| w.borrow().clone());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bad.md") && w[0].contains("invalid `output` schema"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn at_import_in_profile_agents_md() {
        let root = std::env::temp_dir().join(format!("mu-ctx-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&agent).unwrap();
        fs::create_dir_all(home.join(".omp/agent")).unwrap();
        fs::write(agent.join("AGENTS.md"), "@~/.omp/agent/global-instructions.md\n").unwrap();
        fs::write(home.join(".omp/agent/global-instructions.md"), "# Shared\nrule").unwrap();
        let files = load(&proj, &home, &agent);
        assert_eq!(files.last().unwrap().content.trim_end(), "# Shared\nrule");
    }

    #[test]
    fn import_warnings_reach_context() {
        let root = std::env::temp_dir().join(format!("mu-ctx-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("AGENTS.md"), "@./missing.md").unwrap();
        let ctx = Context::load_all(&proj, &home, &root.join("agent"), false, &root.join("modes"));
        assert_eq!(ctx.warnings.len(), 1, "{:?}", ctx.warnings);
        assert!(ctx.warnings[0].contains("cannot read @-import"));
    }

    #[test]
    fn frontmatter_edge_cases() {
        let p = Path::new("x/SKILL.md");
        let (f, body) = frontmatter(p, "---\n---\nbody\n");
        assert!(f.name.is_none());
        assert_eq!(body, "body\n");
        // `---` inside a value is not the closing line.
        let (f, body) = frontmatter(p, "---\nname: a\ndescription: x --- y\n---\nB\n");
        assert_eq!(f.name.as_deref(), Some("a"));
        assert_eq!(body, "B\n");
        let (f, body) = frontmatter(p, "---\r\nname: c\r\n---\r\nB");
        assert_eq!((f.name.as_deref(), body), (Some("c"), "B"));
        // Unclosed: the whole text is the body.
        assert_eq!(frontmatter(p, "---\nname: a\n").1, "---\nname: a\n");
        WARNINGS.with(|w| w.borrow_mut().clear());
        let _ = frontmatter(Path::new("bad.md"), "---\nname: [unclosed\n---\nx");
        let w = WARNINGS.with(|w| w.borrow().clone());
        assert!(w.len() == 1 && w[0].contains("bad.md"), "{w:?}");
    }

    /// A home with a pi agent dir, and an omp agent dir elsewhere.
    fn pi_home() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("mu-ctx-pi-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(home.join(".pi/agent")).unwrap();
        (root.clone(), home, proj, root.join("omp-agent"))
    }

    #[test]
    fn pi_instructions_follow_omps_and_an_override_wins_in_its_directory() {
        let (_root, home, proj, agent) = pi_home();
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("AGENTS.md"), "OMP").unwrap();
        fs::write(home.join(".pi/agent/AGENTS.md"), "PI").unwrap();
        fs::write(proj.join("AGENTS.md"), "plain").unwrap();
        fs::write(proj.join("AGENTS.override.md"), "override").unwrap();
        let got: Vec<_> = load(&proj, &home, &agent).into_iter().map(|f| f.content).collect();
        assert_eq!(got, ["override", "OMP", "PI"]);
    }

    #[test]
    fn pi_skills_are_found_recursively_and_a_project_skill_beats_a_user_one() {
        let (_root, home, proj, agent) = pi_home();
        let w = |p: PathBuf, t: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w(home.join(".pi/agent/skills/group/deep/SKILL.md"), "---\nname: deep\ndescription: nested pi skill\n---\nx");
        w(
            home.join(".pi/agent/skills/group/deep/inner/SKILL.md"),
            "---\nname: inner\ndescription: below a skill\n---\nx",
        );
        w(home.join(".pi/agent/skills/.hidden/SKILL.md"), "---\nname: hidden\ndescription: dot dir\n---\nx");
        w(home.join(".pi/agent/skills/node_modules/m/SKILL.md"), "---\nname: nm\ndescription: vendored\n---\nx");
        w(proj.join(".pi/skills/p/SKILL.md"), "---\nname: clash\ndescription: pi project\n---\nx");
        w(agent.join("skills/clash/SKILL.md"), "---\nname: clash\ndescription: omp user\n---\nx");
        w(
            proj.join(".pi/skills/manual/SKILL.md"),
            "---\nname: manual\ndescription: only by command\ndisable-model-invocation: true\n---\nx",
        );
        let got = discover_skills(&proj, &home, &agent, false);
        let names: Vec<_> = got.iter().map(|s| (s.name.as_str(), s.description.as_str(), s.hidden)).collect();
        assert_eq!(
            names,
            [("manual", "only by command", true), ("clash", "pi project", false), ("deep", "nested pi skill", false)]
        );
    }

    #[test]
    fn pi_prompts_become_commands_and_omp_wins_a_clash() {
        let (_root, home, proj, agent) = pi_home();
        let w = |p: PathBuf, t: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, t).unwrap();
        };
        w(home.join(".pi/agent/prompts/review.md"), "---\ndescription: Review\n---\nReview ${1:-the diff}\n");
        w(proj.join(".pi/prompts/both.md"), "pi both");
        w(agent.join("prompts/both.md"), "omp both");
        let cmds = discover_commands(&proj, &home, &agent);
        let get = |n: &str| cmds.iter().find(|c| c.name == n).unwrap();
        assert!(get("review").pi && get("review").description == "Review");
        assert_eq!(get("both").body, "omp both");
        assert_eq!(get("review").expand(""), "Review the diff\n");
    }

    #[test]
    fn pi_append_system_files_join_the_rules() {
        let (_root, home, proj, agent) = pi_home();
        fs::write(home.join(".pi/agent/APPEND_SYSTEM.md"), "user rule").unwrap();
        fs::create_dir_all(proj.join(".pi")).unwrap();
        fs::write(proj.join(".pi/APPEND_SYSTEM.md"), "project rule\n").unwrap();
        let ctx = Context::load_all(&proj, &home, &agent, false, &home.join("modes"));
        assert_eq!(ctx.rules, ["user rule", "project rule"]);
    }

    #[test]
    fn the_pi_dir_that_is_the_omp_dir_is_read_once() {
        let (_root, home, proj, _agent) = pi_home();
        let both = home.join(".pi/agent");
        assert_eq!(pi_dir(&home, &both), None);
        fs::write(both.join("AGENTS.md"), "ONCE").unwrap();
        assert_eq!(load(&proj, &home, &both).len(), 1);
    }

    #[test]
    fn pi_templates_split_and_substitute_like_pi() {
        let t = |body: &str| SlashCommand { name: "t".into(), body: body.into(), description: String::new(), pi: true };
        assert_eq!(t("a $1 b $2").expand(r#"one "two words""#), "a one b two words");
        assert_eq!(t("$@").expand("x 'y z' w"), "x y z w");
        assert_eq!(t("${1:-def}").expand(""), "def");
        assert_eq!(t("${1:-def}").expand("given"), "given");
        assert_eq!(t("${@:-all}").expand(""), "all");
        assert_eq!(t("${@:2}").expand("a b c"), "b c");
        assert_eq!(t("${@:2:1}").expand("a b c"), "b");
        assert_eq!(t("${@:9}").expand("a b"), "");
        assert_eq!(t("$ARGUMENTS|$10").expand("a b c d e f g h i j"), "a b c d e f g h i j|j");
        // a quote that is not at the start of an argument, or never closes, is text
        assert_eq!(t("$1|$2").expand("doesn't 'open"), "doesn't|'open");
        assert_eq!(t("$1").expand(r#""the \"fast\" path""#), r#"the "fast" path"#);
        // no placeholder: the arguments follow the body, as for every template
        assert_eq!(t("Do it").expand("now"), "Do it\n\nnow");
        // an unknown brace form stays as typed
        assert_eq!(t("${HOME} ${x:-y}").expand(""), "${HOME} ${x:-y}");
        // omp and Claude templates keep their single-digit, double-quote syntax
        let o = SlashCommand { pi: false, ..t("${1:-d} $10") };
        assert_eq!(o.expand("a"), "${1:-d} a0");
    }

    #[test]
    fn a_dot_named_cwd_keeps_its_context() {
        let d = std::env::temp_dir().join(format!("mu-ctx-{}/.work", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("AGENTS.md"), "rules").unwrap();
        let files = load(&d, Path::new("/nonexistent-home"), Path::new("/nonexistent-agent"));
        assert_eq!(files.len(), 1);
        assert!(walk(&d, Path::new("/nonexistent-home")).contains(&d));
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
    }
}
