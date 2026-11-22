//! Declarative modes: markdown files with YAML frontmatter that set a prompt, model, tool policy,
//! write policy and exit dialog. Plan mode is the built-in `plan` file; users override it and add more.

use super::{WARNINGS, frontmatter_as, string_list, walk};
use mu_base::events::{ActionChoice, ActionInput, ActionSpec, ModeAction, ModeEffect, ReviewRequest};
use serde::Deserialize;
use serde_yaml_ng::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const PLAN_MD: &str = include_str!("../prompts/modes/plan.md");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writes {
    All,
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelEntry {
    Never,
    Ask,
    Allow,
}

#[derive(Clone, Debug)]
pub struct ExitDef {
    /// Exit tool name, e.g. `propose_plan`.
    pub tool: String,
    /// Exit tool description (a template).
    pub description: String,
    /// File suffix and noun: `local://<slug>-<document>.md`.
    pub document: String,
    /// Effect and label template (`{n}` = document, `{name}` = mode), in dialog order; each effect at most once.
    pub actions: Vec<(ModeEffect, String)>,
    pub default: ModeEffect,
    /// Hidden prompt after execute, compact and keep.
    pub approved: String,
    /// Prompt for refining a parked document.
    pub refine: String,
    /// Focus text of the compact effect.
    pub compact_focus: String,
    /// Appended to the mode prompt in `mu plan new|refine`.
    pub headless: String,
}

#[derive(Clone, Debug)]
pub struct ModeDef {
    pub name: String,
    pub description: String,
    /// Raw template; placeholders are rendered at use.
    pub prompt: String,
    /// Selectors; the first usable one wins. Empty keeps the session model.
    pub model: Vec<String>,
    pub thinking: Option<String>,
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
    pub writes: Writes,
    /// Agent every `task` call uses while the mode is on.
    pub subagent: Option<String>,
    /// Offered as `/<name>` and as an ACP mode.
    pub command: bool,
    pub model_entry: ModelEntry,
    pub exit: ExitDef,
    /// `None` = built in.
    pub source: Option<PathBuf>,
}

fn default_label(e: ModeEffect) -> &'static str {
    match e {
        ModeEffect::Execute => "Approve and execute",
        ModeEffect::Compact => "Approve and compact context",
        ModeEffect::Keep => "Approve and keep context",
        ModeEffect::Park => "Park {n}",
        ModeEffect::Refine => "Refine {n}",
        ModeEffect::Exit => "Accept and leave {name} mode",
    }
}

impl ModeDef {
    pub fn file_uri(&self, slug: &str) -> String {
        format!("local://{slug}-{}.md", self.exit.document)
    }

    pub fn file_pattern(&self) -> String {
        self.file_uri("<slug>")
    }

    pub fn title(&self) -> String {
        capitalize(&self.name)
    }

    pub fn noun(&self) -> &str {
        &self.exit.document
    }

    pub fn read_only(&self) -> bool {
        self.writes == Writes::Local
    }

    /// An action label with `{n}` and `{name}` filled in.
    pub fn action_label(&self, label: &str) -> String {
        label.replace("{n}", &self.exit.document).replace("{name}", &self.name)
    }

    fn effect_desc(&self, e: ModeEffect) -> String {
        match e {
            ModeEffect::Execute => "fresh context (new session).".into(),
            ModeEffect::Compact => "compacts this conversation, then executes here.".into(),
            ModeEffect::Keep => "executes here with exploration history.".into(),
            ModeEffect::Refine => {
                format!("the user's feedback comes back as the tool result; stay in {} mode.", self.name)
            }
            ModeEffect::Park => "saved for later approval (/plans, mu plan approve); end the turn.".into(),
            ModeEffect::Exit => format!("accepted; {} mode ends and nothing is executed.", self.name),
        }
    }

    fn actions_text(&self) -> String {
        self.exit
            .actions
            .iter()
            .map(|(e, l)| format!("- **{}** — {}", self.action_label(l), self.effect_desc(*e)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fills `{{name}}`, `{{tool}}`, `{{file}}` and `{{actions}}`.
    pub fn render(&self, t: &str) -> String {
        t.replace("{{actions}}", &self.actions_text())
            .replace("{{name}}", &self.name)
            .replace("{{tool}}", &self.exit.tool)
            .replace("{{file}}", &self.file_pattern())
    }
}

/// `s` with its first letter uppercased.
pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The dialog for reviewing mode `m`'s document. `stored`: the document comes from the plan store, so
/// parking becomes dropping, leaving the mode is not offered, and Esc leaves the turn alone.
pub fn mode_request(
    m: &ModeDef,
    title: &str,
    uri: &str,
    file: PathBuf,
    content: String,
    stored: bool,
    note: Option<String>,
) -> ReviewRequest {
    let noun = m.noun();
    let mut actions: Vec<ActionSpec> = Vec::new();
    for (e, label) in &m.exit.actions {
        let (id, label) = match e {
            ModeEffect::Park if stored => ("drop".to_string(), format!("Drop {noun}")),
            ModeEffect::Exit if stored => continue,
            e => (e.as_str().to_string(), m.action_label(label)),
        };
        actions.push(ActionSpec {
            id,
            label,
            disabled: false,
            input: if *e == ModeEffect::Refine { ActionInput::Required } else { ActionInput::None },
            keeps_context: *e == ModeEffect::Keep,
            approves: matches!(e, ModeEffect::Execute | ModeEffect::Compact | ModeEffect::Keep | ModeEffect::Exit),
        });
    }
    if stored && !actions.iter().any(|a| a.id == "drop") {
        actions.push(ActionSpec {
            id: "drop".into(),
            label: format!("Drop {noun}"),
            disabled: false,
            input: ActionInput::None,
            keeps_context: false,
            approves: false,
        });
    }
    let default_id = match m.exit.default {
        ModeEffect::Park if stored => "drop",
        e => e.as_str(),
    };
    let default = actions.iter().position(|a| a.id == default_id).unwrap_or(0);
    ReviewRequest {
        header: format!("{} review", capitalize(noun)),
        title: title.to_string(),
        uri: Some(uri.to_string()),
        file: Some(file),
        content,
        note,
        actions,
        default,
        models: true,
        cancel_turn: !stored,
        cancel_notice: if stored {
            format!("{noun} left parked")
        } else {
            format!("{noun} review cancelled; {} mode stays on", m.name)
        },
    }
}

/// The action a dialog choice stands for. `None`: an unknown id, or a refinement without text.
pub fn choice_action(c: &ActionChoice) -> Option<ModeAction> {
    let model = || c.model.clone();
    Some(match c.id.as_str() {
        "execute" => ModeAction::Execute { model: model() },
        "compact" => ModeAction::Compact { model: model() },
        "keep" => ModeAction::Keep { model: model() },
        "refine" => ModeAction::Refine(c.text.clone().filter(|t| !t.trim().is_empty())?),
        "park" => ModeAction::Park,
        "drop" => ModeAction::Drop,
        "exit" => ModeAction::Exit,
        _ => return None,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ModeFront {
    name: Option<String>,
    description: Option<String>,
    extends: Option<String>,
    prompt: Option<String>,
    model: Option<Value>,
    thinking: Option<String>,
    tools: Option<ToolsFront>,
    writes: Option<String>,
    subagent: Option<String>,
    command: Option<bool>,
    model_entry: Option<String>,
    exit: Option<ExitFront>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ToolsFront {
    allow: Option<Value>,
    deny: Option<Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExitFront {
    tool: Option<String>,
    description: Option<String>,
    document: Option<String>,
    actions: Option<Vec<Value>>,
    default: Option<String>,
    approved: Option<String>,
    refine: Option<String>,
    compact_focus: Option<String>,
    headless: Option<String>,
}

fn warn(s: String) {
    WARNINGS.with(|w| w.borrow_mut().push(s));
}

fn blank() -> ModeDef {
    ModeDef {
        name: String::new(),
        description: String::new(),
        prompt: String::new(),
        model: Vec::new(),
        thinking: None,
        allow: None,
        deny: Vec::new(),
        writes: Writes::All,
        subagent: None,
        command: true,
        model_entry: ModelEntry::Never,
        exit: ExitDef {
            tool: String::new(),
            description: String::new(),
            document: String::new(),
            actions: Vec::new(),
            default: ModeEffect::Execute,
            approved: String::new(),
            refine: String::new(),
            compact_focus: String::new(),
            headless: String::new(),
        },
        source: None,
    }
}

/// The modes mu ships: `plan` first.
pub fn builtin_modes() -> Vec<ModeDef> {
    static BUILTIN: OnceLock<Vec<ModeDef>> = OnceLock::new();
    BUILTIN
        .get_or_init(|| {
            let path = Path::new("<built-in>/plan.md");
            let (front, body) = frontmatter_as::<ModeFront>(path, PLAN_MD);
            let mut def = merge(blank(), true, front, body, "plan").expect("the built-in plan mode is valid");
            def.source = None;
            vec![def]
        })
        .clone()
}

fn valid_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && n != "default"
        && n != "none"
}

fn parse_actions(items: &[Value]) -> Result<Vec<(ModeEffect, String)>, String> {
    let mut out: Vec<(ModeEffect, String)> = Vec::new();
    for item in items {
        let (name, label) = match item {
            Value::String(s) => (s.trim().to_string(), None),
            Value::Mapping(m) => {
                let get = |k: &str| m.get(k).and_then(Value::as_str).map(|s| s.trim().to_string());
                (get("effect").unwrap_or_default(), get("label").filter(|l| !l.is_empty()))
            }
            _ => return Err("exit.actions: each item is an effect name or {effect, label}".into()),
        };
        let effect = ModeEffect::parse(&name).ok_or_else(|| format!("exit.actions: unknown effect `{name}`"))?;
        if out.iter().any(|(e, _)| *e == effect) {
            return Err(format!("exit.actions: duplicate effect `{name}`"));
        }
        out.push((effect, label.unwrap_or_else(|| default_label(effect).to_string())));
    }
    if out.is_empty() {
        return Err("exit.actions: at least one effect is needed".into());
    }
    Ok(out)
}

fn merge(base: ModeDef, fresh: bool, f: ModeFront, body: &str, name: &str) -> Result<ModeDef, String> {
    let mut m = base;
    match f.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()) {
        Some(d) => m.description = d,
        None if fresh => return Err("missing `description`".into()),
        None => {}
    }
    if let Some(v) = &f.model {
        m.model = string_list(v).unwrap_or_default();
    }
    if let Some(t) = f.thinking {
        m.thinking = Some(t);
    }
    if let Some(t) = f.tools {
        if let Some(a) = t.allow.as_ref() {
            m.allow = string_list(a);
        }
        if let Some(d) = t.deny.as_ref() {
            m.deny = string_list(d).unwrap_or_default();
        }
    }
    if let Some(w) = f.writes {
        m.writes = match w.as_str() {
            "all" => Writes::All,
            "local" => Writes::Local,
            _ => return Err(format!("`writes` is `all` or `local`, not `{w}`")),
        };
    }
    if let Some(s) = f.subagent {
        m.subagent = Some(s);
    }
    if let Some(c) = f.command {
        m.command = c;
    }
    if let Some(e) = f.model_entry {
        m.model_entry = match e.as_str() {
            "never" => ModelEntry::Never,
            "ask" => ModelEntry::Ask,
            "allow" => ModelEntry::Allow,
            _ => return Err(format!("`model_entry` is `never`, `ask` or `allow`, not `{e}`")),
        };
    }
    if let Some(x) = f.exit {
        let e = &mut m.exit;
        let text = |s: String| s.trim_end().to_string();
        if let Some(v) = x.tool {
            e.tool = v.trim().to_string();
        }
        if let Some(v) = x.description {
            e.description = text(v);
        }
        if let Some(v) = x.document {
            e.document = v.trim().to_string();
        }
        if let Some(a) = x.actions {
            e.actions = parse_actions(&a)?;
        }
        if let Some(d) = x.default {
            e.default = ModeEffect::parse(d.trim()).ok_or_else(|| format!("exit.default: unknown effect `{d}`"))?;
        }
        if let Some(v) = x.approved {
            e.approved = text(v);
        }
        if let Some(v) = x.refine {
            e.refine = text(v);
        }
        if let Some(v) = x.compact_focus {
            e.compact_focus = text(v);
        }
        if let Some(v) = x.headless {
            e.headless = text(v);
        }
    }
    let body = body.trim();
    match f.prompt.as_deref() {
        None | Some("append") if !fresh => {
            if !body.is_empty() {
                m.prompt = if m.prompt.is_empty() { body.to_string() } else { format!("{}\n\n{body}", m.prompt) };
            }
        }
        None | Some("append") | Some("replace") => m.prompt = body.to_string(),
        Some(p) => return Err(format!("`prompt` is `append` or `replace`, not `{p}`")),
    }
    m.name = name.to_string();
    Ok(m)
}

struct Pending {
    path: PathBuf,
    name: String,
    front: ModeFront,
    body: String,
}

fn md_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> =
        rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
    files.sort();
    files
}

/// Applies one directory's files to the registry; files that extend a mode not defined yet wait for it.
fn apply_layer(reg: &mut Vec<ModeDef>, plan_exit: &ExitDef, files: Vec<PathBuf>) {
    let mut pending: Vec<Pending> = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let (front, body) = frontmatter_as::<ModeFront>(&path, &text);
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let name = front.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(stem);
        if !valid_name(&name) {
            warn(format!("{}: invalid mode name `{name}`", path.display()));
            continue;
        }
        pending.push(Pending { path, name, front, body: body.to_string() });
    }
    loop {
        let before = pending.len();
        let mut deferred = Vec::new();
        for p in pending {
            let base = match p.front.extends.as_deref().map(str::trim) {
                Some(x) => match reg.iter().find(|m| m.name == x) {
                    Some(b) => Some((b.clone(), false)),
                    None => {
                        deferred.push(p);
                        continue;
                    }
                },
                None => reg.iter().find(|m| m.name == p.name).map(|b| (b.clone(), false)),
            };
            let (base, fresh) = base.unwrap_or_else(|| (fresh_default(&p.name, plan_exit), true));
            match merge(base, fresh, p.front, &p.body, &p.name) {
                Ok(mut def) => {
                    def.source = Some(p.path);
                    match reg.iter().position(|m| m.name == p.name) {
                        Some(i) => reg[i] = def,
                        None => reg.push(def),
                    }
                }
                Err(e) => warn(format!("{}: {e}", p.path.display())),
            }
        }
        pending = deferred;
        if pending.is_empty() || pending.len() == before {
            break;
        }
    }
    for p in pending {
        let x = p.front.extends.unwrap_or_default();
        warn(format!("{}: extends unknown mode `{}`", p.path.display(), x.trim()));
    }
}

fn fresh_default(name: &str, plan_exit: &ExitDef) -> ModeDef {
    let mut m = blank();
    m.exit = plan_exit.clone();
    m.exit.tool = format!("propose_{}", name.replace('-', "_"));
    m.exit.document = name.to_string();
    m.exit.description = "Submit the finished {{name}} document for user approval. The file {{file}} must exist. Only available in {{name}} mode.".into();
    m
}

/// Built-in modes, then `user_dir/*.md`, then each project directory's `.agents/modes/*.md` (farthest first),
/// each file merged onto the mode of its name or the one it `extends`.
pub fn discover_modes(cwd: &Path, home: &Path, user_dir: &Path) -> Vec<ModeDef> {
    let mut reg = builtin_modes();
    let plan_exit = reg[0].exit.clone();
    apply_layer(&mut reg, &plan_exit, md_files(user_dir));
    for dir in walk(cwd, home).into_iter().rev() {
        apply_layer(&mut reg, &plan_exit, md_files(&dir.join(".agents/modes")));
    }
    reg.sort_by(|a, b| (a.name != "plan", &a.name).cmp(&(b.name != "plan", &b.name)));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp() -> PathBuf {
        let root = std::env::temp_dir().join(format!("mu-modes-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("home/p")).unwrap();
        root
    }

    fn warnings() -> Vec<String> {
        WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
    }

    #[test]
    fn builtin_plan_parses() {
        let m = &builtin_modes()[0];
        assert_eq!(m.name, "plan");
        assert_eq!(m.exit.tool, "propose_plan");
        let effects: Vec<_> = m.exit.actions.iter().map(|(e, _)| *e).collect();
        assert_eq!(
            effects,
            [ModeEffect::Execute, ModeEffect::Compact, ModeEffect::Keep, ModeEffect::Park, ModeEffect::Refine]
        );
        assert_eq!(m.writes, Writes::Local);
        assert_eq!(m.model, ["@plan"]);
        assert_eq!(m.model_entry, ModelEntry::Ask);
        assert!(m.source.is_none() && m.read_only());
        assert_eq!(m.file_uri("a"), "local://a-plan.md");
        assert_eq!(m.title(), "Plan");
    }

    #[test]
    fn plan_prompt_renders_fully() {
        let m = &builtin_modes()[0];
        let p = m.render(&m.prompt);
        assert!(!p.contains("{{"), "{p}");
        assert!(p.contains("propose_plan") && p.contains("local://<slug>-plan.md"));
        assert!(p.contains("**Approve and execute**") && p.contains("**Park plan**"));
        assert!(p.contains("Documentation is NOT cleanup"));
        assert!(!p.contains("doc updates, formatter"));
        for t in [&m.exit.description, &m.exit.refine, &m.exit.headless] {
            assert!(!m.render(t).contains("{{tool}}"));
        }
    }

    #[test]
    fn user_file_with_a_builtin_name_merges() {
        let root = temp();
        fs::create_dir_all(root.join("modes")).unwrap();
        fs::write(root.join("modes/plan.md"), "---\nmodel: \"@slow\"\n---\n").unwrap();
        warnings();
        let got = discover_modes(&root.join("home/p"), &root.join("home"), &root.join("modes"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].model, ["@slow"]);
        assert_eq!(got[0].prompt, builtin_modes()[0].prompt);
        assert!(got[0].source.is_some());
        assert!(warnings().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extends_replace_and_nearer_dirs() {
        let root = temp();
        let home = root.join("home");
        let proj = home.join("p/q");
        fs::create_dir_all(proj.join(".agents/modes")).unwrap();
        fs::create_dir_all(home.join("p/.agents/modes")).unwrap();
        let far = "---\ndescription: far\nextends: plan\nprompt: replace\nexit:\n  document: design\n  actions: [keep, exit]\n---\nX\n";
        let near = "---\nextends: plan\nprompt: replace\nexit:\n  document: design\n  actions: [keep, exit]\n---\nY\n";
        fs::write(home.join("p/.agents/modes/design.md"), far).unwrap();
        warnings();
        let got = discover_modes(&proj, &home, &root.join("nomodes"));
        assert_eq!(got.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["plan", "design"]);
        let d = &got[1];
        assert_eq!(d.prompt, "X");
        assert_eq!(d.description, "far");
        assert_eq!(d.exit.tool, "propose_plan");
        assert_eq!(d.file_uri("a"), "local://a-design.md");
        assert_eq!(d.exit.actions.len(), 2);
        assert_eq!(d.action_label(&d.exit.actions[1].1), "Accept and leave design mode");
        assert!(d.read_only(), "inherited from plan");
        fs::write(proj.join(".agents/modes/design.md"), near).unwrap();
        let got = discover_modes(&proj, &home, &root.join("nomodes"));
        assert_eq!(got[1].prompt, "Y");
        assert!(warnings().is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_new_mode_gets_defaults_and_its_own_tool() {
        let root = temp();
        let proj = root.join("home/p");
        fs::create_dir_all(proj.join(".agents/modes")).unwrap();
        fs::write(proj.join(".agents/modes/code-review.md"), "---\ndescription: d\n---\nBODY\n").unwrap();
        fs::write(proj.join(".agents/modes/nodesc.md"), "---\n---\nBODY\n").unwrap();
        warnings();
        let got = discover_modes(&proj, &root.join("home"), &root.join("nomodes"));
        let m = got.iter().find(|m| m.name == "code-review").unwrap();
        assert_eq!(m.exit.tool, "propose_code_review");
        assert_eq!(m.exit.document, "code-review");
        assert_eq!((m.writes, m.model_entry, m.command), (Writes::All, ModelEntry::Never, true));
        assert_eq!(m.prompt, "BODY");
        assert!(m.render(&m.exit.description).contains("local://<slug>-code-review.md"));
        let w = warnings();
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("nodesc.md") && w[0].contains("description"));
        assert!(got.iter().all(|m| m.name != "nodesc"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bad_names_and_references_warn() {
        let root = temp();
        let proj = root.join("home/p");
        let dir = proj.join(".agents/modes");
        fs::create_dir_all(&dir).unwrap();
        let ok = "---\ndescription: d\n---\nx";
        fs::write(dir.join("none.md"), ok).unwrap();
        fs::write(dir.join("Bad Name.md"), ok).unwrap();
        fs::write(dir.join("missing.md"), "---\ndescription: d\nextends: nope\n---\nx").unwrap();
        fs::write(dir.join("a.md"), "---\ndescription: d\nextends: b\n---\nx").unwrap();
        fs::write(dir.join("b.md"), "---\ndescription: d\nextends: a\n---\nx").unwrap();
        fs::write(dir.join("badeffect.md"), "---\ndescription: d\nexit:\n  actions: [nope]\n---\nx").unwrap();
        fs::write(dir.join("dupe.md"), "---\ndescription: d\nexit:\n  actions: [keep, keep]\n---\nx").unwrap();
        fs::write(dir.join("baddefault.md"), "---\ndescription: d\nexit:\n  default: nope\n---\nx").unwrap();
        warnings();
        let got = discover_modes(&proj, &root.join("home"), &root.join("nomodes"));
        assert_eq!(got.len(), 1, "{:?}", got.iter().map(|m| &m.name).collect::<Vec<_>>());
        let w = warnings().join("\n");
        for needle in [
            "none.md: invalid mode name `none`",
            "Bad Name.md: invalid mode name `Bad Name`",
            "missing.md: extends unknown mode `nope`",
            "a.md: extends unknown mode `b`",
            "b.md: extends unknown mode `a`",
            "unknown effect `nope`",
            "duplicate effect `keep`",
            "exit.default",
        ] {
            assert!(w.contains(needle), "{needle} not in\n{w}");
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn requests_follow_the_mode_and_whether_the_document_is_stored() {
        let m = &builtin_modes()[0];
        let live = mode_request(m, "T", "local://t-plan.md", "/f".into(), "c".into(), false, None);
        let ids: Vec<_> = live.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["execute", "compact", "keep", "park", "refine"]);
        assert_eq!(live.header, "Plan review");
        assert_eq!(live.default, 0);
        assert!(live.cancel_turn && live.cancel_notice == "plan review cancelled; plan mode stays on");
        let keep = &live.actions[2];
        assert!(keep.keeps_context && keep.approves && keep.label == "Approve and keep context");
        assert_eq!(live.actions[4].input, ActionInput::Required);
        assert!(!live.actions[3].approves);
        let stored = mode_request(m, "T", "u", "/f".into(), "c".into(), true, Some("n".into()));
        let ids: Vec<_> = stored.actions.iter().map(|a| (a.id.as_str(), a.label.as_str())).collect();
        assert_eq!(ids[3], ("drop", "Drop plan"));
        assert!(!stored.cancel_turn && stored.note.as_deref() == Some("n"));
    }

    #[test]
    fn choices_map_to_actions() {
        let c = |id: &str, text: Option<&str>| ActionChoice {
            id: id.into(),
            text: text.map(Into::into),
            model: Some("@m".into()),
        };
        assert!(matches!(choice_action(&c("keep", None)), Some(ModeAction::Keep { model: Some(m) }) if m == "@m"));
        assert!(matches!(choice_action(&c("refine", Some("x"))), Some(ModeAction::Refine(t)) if t == "x"));
        assert!(choice_action(&c("refine", Some("  "))).is_none() && choice_action(&c("refine", None)).is_none());
        assert!(matches!(choice_action(&c("exit", None)), Some(ModeAction::Exit)));
        assert!(matches!(choice_action(&c("drop", None)), Some(ModeAction::Drop)));
        assert!(choice_action(&c("nope", None)).is_none());
    }

    #[test]
    fn labels_follow_the_document() {
        let root = temp();
        let proj = root.join("home/p");
        fs::create_dir_all(proj.join(".agents/modes")).unwrap();
        fs::write(
            proj.join(".agents/modes/design.md"),
            "---\ndescription: d\nextends: plan\nexit:\n  document: design\n  actions:\n    - keep\n    - {effect: park, label: \"Shelve {n}\"}\n---\n",
        )
        .unwrap();
        let got = discover_modes(&proj, &root.join("home"), &root.join("nomodes"));
        let d = got.iter().find(|m| m.name == "design").unwrap();
        let r = d.render("{{actions}}");
        assert!(r.contains("**Approve and keep context**") && r.contains("**Shelve design**"), "{r}");
        fs::remove_dir_all(&root).ok();
    }
}
