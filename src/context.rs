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
        if dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
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
    near_first
}

/// Directories from `cwd` up to (excluding) `home`, skipping dot-named ones.
fn walk(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in cwd.ancestors() {
        if cwd.starts_with(home) && dir == home {
            break;
        }
        if dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
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

/// Splits leading `---` YAML frontmatter from the body.
fn frontmatter(text: &str) -> (Front, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else { return (Front::default(), text) };
    let Some(end) = rest.find("\n---") else { return (Front::default(), text) };
    let front = serde_yaml_ng::from_str(&rest[..end]).unwrap_or_default();
    let after = &rest[end + 4..];
    let body = after.split_once('\n').map_or("", |(_, b)| b);
    (front, body)
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
            let (f, _) = frontmatter(&text);
            let dir_name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let name = f.name.unwrap_or(dir_name);
            let Some(description) = f.description.filter(|d| !d.trim().is_empty()) else { continue };
            if name.contains('/') || f.enabled == Some(false) || out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill { name, description: description.trim().into(), dir, hidden: f.hide || f.disable_model_invocation });
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
        let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        files.sort();
        for f in files {
            let Some(name) = f.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            if out.iter().any(|c| c.name == name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            let (_, body) = frontmatter(&text);
            out.push(SlashCommand { name, body: body.trim_start_matches('\n').into() });
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

pub struct Context {
    pub files: Vec<ContextFile>,
    pub rules: Vec<String>,
    pub skills: Vec<Skill>,
    pub commands: Vec<SlashCommand>,
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
        Self {
            files: load(cwd, home, agent_dir),
            rules,
            skills: discover_skills(cwd, home, agent_dir, claude_user),
            commands: discover_commands(cwd, home, agent_dir),
        }
    }

    /// `/skill:<name> [args]` prompt text.
    pub fn skill_prompt(&self, name: &str, args: &str) -> Option<String> {
        let sk = self.skills.iter().find(|s| s.name == name)?;
        let path = sk.dir.join("SKILL.md");
        let text = std::fs::read_to_string(&path).ok()?;
        let (_, body) = frontmatter(&text);
        let mut out = format!("<skill name=\"{name}\" path=\"{}\">\n{}\n</skill>", path.display(), body.trim());
        if !args.trim().is_empty() {
            out.push_str(&format!("\n\n{}", args.trim()));
        }
        Some(out)
    }
}

pub struct PromptInput<'a> {
    pub model: &'a str,
    pub cwd: &'a Path,
    pub ctx: &'a Context,
    pub plan: Option<&'a str>,
    pub subagent: bool,
    pub plugin_ctx: &'a std::collections::BTreeMap<String, String>,
}

pub fn system_prompt(i: &PromptInput) -> String {
    let os = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let date = jiff::Zoned::now().strftime("%Y-%m-%d");
    let mut s = format!(
        "You are mu, a coding agent running in the user's terminal. Work in the current directory with the provided tools.\n\
- Read a file before editing it. Edit with `edit`, using the `[path#TAG]` header from your latest read, write or edit of that file.\n\
- Use `write` only to create a file or replace it entirely.\n\
- Search with `grep` and `glob` rather than `bash`.\n\
- Be concise; the user sees each tool call.\n\n\
<workstation>\n- OS: linux {}\n- Arch: {}\n- Model: {}\n- Date: {date}\n- Working directory: {}\n</workstation>\n",
        os.trim(),
        std::env::consts::ARCH,
        i.model,
        i.cwd.display()
    );
    if i.subagent {
        s.push_str("\nYou are a subagent. Your final message is returned verbatim to the parent agent as your result; make it complete and self-contained.\n");
    }
    if !i.ctx.files.is_empty() {
        s.push_str("\n<repo-rules>\nMUST follow these context files for all tasks:\n");
        for f in &i.ctx.files {
            s.push_str(&format!("<file path=\"{}\">\n{}\n</file>\n", f.path.display(), f.content.trim_end()));
        }
        s.push_str("</repo-rules>\n");
    }
    for r in &i.ctx.rules {
        s.push_str(&format!("\n<rules>\n{r}\n</rules>\n"));
    }
    let listed: Vec<&Skill> = i.ctx.skills.iter().filter(|k| !k.hidden).collect();
    if !listed.is_empty() {
        s.push_str("\nMatching skill → MUST read `skill://<name>` first.\n<skills>\n");
        for k in listed {
            s.push_str(&format!("- {}: {}\n", k.name, k.description));
        }
        s.push_str("</skills>\n");
    }
    if let Some(p) = i.plan {
        s.push('\n');
        s.push_str(p.trim_end());
        s.push('\n');
    }
    for c in i.plugin_ctx.values().filter(|c| !c.trim().is_empty()) {
        s.push('\n');
        s.push_str(c.trim_end());
        s.push('\n');
    }
    s
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
    fn skills_commands_and_templates() {
        let root = std::env::temp_dir().join(format!("mu-ctx2-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        let w = |p: PathBuf, c: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        w(proj.join(".omp/skills/a/SKILL.md"), "---\nname: alpha\ndescription: first\n---\nbody A");
        w(agent.join("skills/alpha/SKILL.md"), "---\ndescription: shadowed\n---\nx");
        w(agent.join("skills/hid/SKILL.md"), "---\ndescription: d\nhide: true\n---\nx");
        w(agent.join("skills/nodesc/SKILL.md"), "---\nname: nodesc\n---\nx");
        w(agent.join("skills/off/SKILL.md"), "---\ndescription: d\nenabled: false\n---\nx");
        w(proj.join(".omp/commands/greet.md"), "---\nargument-hint: <who>\n---\nSay hi to $1 ($@)");
        w(proj.join(".omp/commands/plain.md"), "Do the thing");
        w(agent.join("RULES.md"), "user rule");
        let ctx = Context::load_all(&proj, &home, &agent, false);
        let names: Vec<_> = ctx.skills.iter().map(|s| (s.name.as_str(), s.description.as_str(), s.hidden)).collect();
        assert_eq!(names, [("alpha", "first", false), ("hid", "d", true)]);
        let greet = ctx.commands.iter().find(|c| c.name == "greet").unwrap();
        assert_eq!(greet.expand("Bob \"a b\""), "Say hi to Bob (Bob a b)");
        let plain = ctx.commands.iter().find(|c| c.name == "plain").unwrap();
        assert_eq!(plain.expand("extra"), "Do the thing\n\nextra");
        let sp = system_prompt(&PromptInput { model: "m", cwd: &proj, ctx: &ctx, plan: None, subagent: false, plugin_ctx: &Default::default() });
        assert!(sp.contains("- alpha: first\n") && !sp.contains("- hid:") && sp.contains("<rules>\nuser rule"));
        assert!(ctx.skill_prompt("alpha", "go").unwrap().ends_with("body A\n</skill>\n\ngo"));
    }
}
