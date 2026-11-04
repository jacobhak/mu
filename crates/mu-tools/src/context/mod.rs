mod at_imports;

use std::path::{Path, PathBuf};

pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn non_empty(p: &Path) -> Option<ContextFile> {
    let content = std::fs::read_to_string(p).ok()?;
    (!content.trim().is_empty()).then(|| ContextFile { path: p.into(), content })
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
        if let Some(f) = non_empty(&dir.join("AGENTS.md")).or_else(|| non_empty(&dir.join("CLAUDE.md"))) {
            near_first.push(f);
        }
    }
    near_first.reverse();
    near_first.extend(non_empty(&agent_dir.join("AGENTS.md")));
    for f in &mut near_first {
        f.content = at_imports::expand(&f.content, &f.path, home, &mut |w| WARNINGS.with(|x| x.borrow_mut().push(w)));
    }
    near_first
}

/// cwd-relative paths of AGENTS.md files 1–4 directories below cwd.
fn nested_agents_md(cwd: &Path) -> Vec<String> {
    use pi_walker::{FileType, Fs, walk};
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

fn skill_roots(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool) -> Vec<PathBuf> {
    let dirs = walk(cwd, home);
    let mut roots = Vec::new();
    roots.extend(dirs.iter().map(|d| d.join(".omp/skills")));
    roots.push(agent_dir.join("skills"));
    roots.extend(dirs.iter().map(|d| d.join(".claude/skills")));
    if claude_user {
        roots.push(home.join(".claude/skills"));
    }
    for d in &dirs {
        roots.push(d.join(".agents/skills"));
        roots.push(d.join(".agent/skills"));
    }
    roots.push(home.join(".agents/skills"));
    roots
}

pub fn discover_skills(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in skill_roots(cwd, home, agent_dir, claude_user) {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let mut dirs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
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
pub const REVIEW_WORKER: &str = "review-worker";

/// Agents mu ships. They are not in `Context::agents`, so the main agent's `task` tool never offers them.
pub fn builtin_agent(name: &str) -> Option<AgentDef> {
    let tools = |t: &[&str]| Some(t.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let model: Vec<String> = mu_base::models::role_chain("review").iter().map(|r| format!("@{r}")).collect();
    let (description, tools, prompt) = match name {
        REVIEW_LEAD => (
            "Lead code reviewer",
            tools(&["read", "grep", "glob", "bash", "task"]),
            include_str!("../prompts/review-lead.md"),
        ),
        REVIEW_WORKER => (
            "Reviews an assigned slice of a diff; read-only; returns findings as JSON",
            tools(&["read", "grep", "glob", "bash"]),
            include_str!("../prompts/review-worker.md"),
        ),
        _ => return None,
    };
    Some(AgentDef {
        name: name.into(),
        description: description.into(),
        tools,
        model,
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
            if ["main", "sub", "task", "scout", REVIEW_LEAD, REVIEW_WORKER].contains(&lower.as_str())
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
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.extend(dirs.iter().map(|d| d.join(".omp/commands")));
    roots.push(agent_dir.join("commands"));
    roots.extend(dirs.iter().map(|d| d.join(".claude/commands")));
    roots.extend(dirs.iter().map(|d| d.join(".omp/prompts")));
    roots.push(agent_dir.join("prompts"));
    let mut out: Vec<SlashCommand> = Vec::new();
    for root in roots {
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
            out.push(SlashCommand { name, body: body.trim_start_matches('\n').into(), description });
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

impl SlashCommand {
    /// `$1..$9`, `$@`/`$ARGUMENTS`; args are appended when the body has no placeholder.
    pub fn expand(&self, args: &str) -> String {
        let parts = split_args(args);
        let joined = parts.join(" ");
        let mut had = false;
        let mut out = String::with_capacity(self.body.len());
        let b = self.body.as_str();
        let mut i = 0;
        while i < b.len() {
            let rest = &b[i..];
            if let Some(r) = rest.strip_prefix("$ARGUMENTS") {
                out.push_str(&joined);
                had = true;
                i = b.len() - r.len();
            } else if let Some(r) = rest.strip_prefix("$@") {
                out.push_str(&joined);
                had = true;
                i = b.len() - r.len();
            } else if rest.starts_with('$') && rest.as_bytes().get(1).is_some_and(|d| (b'1'..=b'9').contains(d)) {
                let n = (rest.as_bytes()[1] - b'1') as usize;
                out.push_str(parts.get(n).map_or("", |s| s.as_str()));
                had = true;
                i += 2;
            } else {
                let c = rest.chars().next().unwrap();
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
}

impl Context {
    pub fn load_all(cwd: &Path, home: &Path, agent_dir: &Path, claude_user: bool) -> Self {
        let mut rules = Vec::new();
        let user = std::fs::read_to_string(agent_dir.join("RULES.md")).ok();
        let project = walk(cwd, home).into_iter().find_map(|d| std::fs::read_to_string(d.join(".omp/RULES.md")).ok());
        for r in [user, project].into_iter().flatten() {
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
        let warnings = WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()));
        Self { warnings, files, nested, rules, skills, commands, agents }
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
        let ctx = Context::load_all(&proj, &home, &root.join("agent"), false);
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
