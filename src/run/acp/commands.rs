//! Slash commands over ACP: the ones that work without a terminal UI, advertised with
//! `available_commands_update` and run when a prompt starts with them.

use mu_tools::context::Context;
use serde_json::{Value, json};

/// Built-in commands that mean something here: name, description, input hint.
const BUILTIN: [(&str, &str, Option<&str>); 3] = [
    ("compact", "Compact the context", Some("what to keep")),
    ("review", "Review changes against the approved plan", Some("focus")),
    ("rename", "Rename this session", Some("title")),
];

/// What a prompt asks for.
#[derive(Debug, PartialEq)]
pub(super) enum Parsed {
    /// Not a command, or one this side does not know: sent to the model as typed.
    Plain,
    /// An expanded skill or prompt template, sent to the model in place of the command.
    Prompt(String),
    Compact(Option<String>),
    Review(Option<String>),
    /// Enters a mode; with a prompt it also runs the text in it, without one the turn ends.
    Mode {
        name: String,
        prompt: Option<String>,
    },
    Rename(String),
    /// Answered at once, without the model.
    Reply(String),
}

fn command(name: &str, description: &str, hint: Option<&str>) -> Value {
    let mut c = json!({"name": name, "description": description});
    if let Some(h) = hint {
        c["input"] = json!({"hint": h});
    }
    c
}

/// The commands a client can offer: the built-ins above, the modes, prompt templates and skills.
pub(super) fn available(ctx: &Context) -> Vec<Value> {
    let mut out: Vec<Value> = BUILTIN.iter().map(|(n, d, h)| command(n, d, *h)).collect();
    out.extend(ctx.modes.iter().filter(|m| m.command).map(|m| command(&m.name, &m.description, Some("prompt"))));
    out.extend(ctx.commands.iter().map(|c| command(&c.name, &c.description, Some("arguments"))));
    out.extend(ctx.skills.iter().map(|s| command(&format!("skill:{}", s.name), &s.description, Some("arguments"))));
    out
}

pub(super) fn parse(ctx: &Context, text: &str) -> Parsed {
    let Some(cmd) = text.trim().strip_prefix('/') else { return Parsed::Plain };
    let (name, arg) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(n, a)| (n, a.trim()));
    let some = |a: &str| (!a.is_empty()).then(|| a.to_string());
    match name {
        "compact" => Parsed::Compact(some(arg)),
        "review" => Parsed::Review(some(arg)),
        "rename" if arg.is_empty() => Parsed::Reply("usage: /rename <title>".into()),
        "rename" => Parsed::Rename(arg.into()),
        _ => {
            if ctx.mode(name).is_some_and(|m| m.command) {
                Parsed::Mode { name: name.into(), prompt: some(arg) }
            } else if let Some(sk) = name.strip_prefix("skill:") {
                match ctx.skill_prompt(sk, arg) {
                    Some(p) => Parsed::Prompt(p),
                    None => Parsed::Reply(format!("unknown skill {sk}")),
                }
            } else if let Some(c) = ctx.commands.iter().find(|c| c.name == name) {
                Parsed::Prompt(c.expand(arg))
            } else {
                Parsed::Plain
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_tools::context::SlashCommand;

    fn ctx() -> Context {
        Context {
            modes: mu_tools::context::builtin_modes(),
            commands: vec![SlashCommand {
                name: "fix".into(),
                body: "Fix $1 carefully".into(),
                description: "Fix a thing".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn built_ins_templates_and_skills_are_advertised_with_hints() {
        let a = available(&ctx());
        let names: Vec<&str> = a.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["compact", "review", "rename", "plan", "fix"]);
        assert_eq!(a[0]["input"]["hint"], "what to keep");
        assert_eq!(a[3]["input"]["hint"], "prompt");
        assert_eq!(a[4]["description"], "Fix a thing");
    }

    #[test]
    fn a_prompt_starting_with_a_command_runs_it() {
        let c = ctx();
        assert_eq!(parse(&c, "/compact keep the API"), Parsed::Compact(Some("keep the API".into())));
        assert_eq!(parse(&c, "  /compact  "), Parsed::Compact(None));
        assert_eq!(parse(&c, "/review"), Parsed::Review(None));
        assert_eq!(
            parse(&c, "/plan add a flag"),
            Parsed::Mode { name: "plan".into(), prompt: Some("add a flag".into()) }
        );
        assert_eq!(parse(&c, "/plan"), Parsed::Mode { name: "plan".into(), prompt: None });
        assert_eq!(parse(&c, "/rename New title"), Parsed::Rename("New title".into()));
        assert!(matches!(parse(&c, "/rename"), Parsed::Reply(m) if m.contains("usage")));
        assert_eq!(parse(&c, "/fix the bug"), Parsed::Prompt("Fix the carefully".into()));
    }

    #[test]
    fn anything_else_goes_to_the_model_as_typed() {
        let c = ctx();
        for t in ["hello", "/etc/hosts is odd", "/unknown thing", "a /compact b", ""] {
            assert_eq!(parse(&c, t), Parsed::Plain, "{t}");
        }
        assert!(matches!(parse(&c, "/skill:nope x"), Parsed::Reply(m) if m.contains("unknown skill nope")));
    }
}
