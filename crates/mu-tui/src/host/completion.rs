//! The composer's completion list, skills and user commands, and the plugin list: what comes from the
//! host's context and plugins.

use super::local::{Local, Registry};
use super::{Answer, pending, ready};
use crate::app::{command_entries, custom_completion_ids};
use crate::widgets::complete::Entry;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::time::Duration;

pub const COMPLETIONS: &str = "completions";
pub const EXPAND: &str = "expand";
pub const SKILLS: &str = "skills";
pub const PLUGINS: &str = "plugins";
pub const OBSERVE: &str = "observe";
pub const GHOST: &str = "ghost";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Completions {
    pub entries: Vec<Entry>,
    /// `providers.toml` did not read: the custom endpoint names are missing from the list.
    pub error: Option<String>,
}

/// What `/name arg` stands for when it is not a built-in command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Expanded {
    Prompt(String),
    UnknownSkill,
    UnknownCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillRow {
    pub name: String,
    pub hidden: bool,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginRow {
    pub name: String,
    pub label: String,
    pub running: bool,
    pub extra: String,
}

pub(super) fn register(r: &mut Registry) {
    r.add(COMPLETIONS, completions);
    r.add(EXPAND, expand);
    r.add(SKILLS, skills);
    r.add(PLUGINS, plugins);
    r.add(OBSERVE, observe);
    r.add(GHOST, ghost);
}

/// `{text, assistant}`: the ghost text the prediction plugin suggests, empty when it has none or is
/// slow (it gets 300 ms).
fn ghost(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |(text, assistant): (String, String)| {
        let params = serde_json::json!({
            "text": text,
            "cwd": l.cwd,
            "assistant": assistant,
            "threshold": l.providers.cfg.mu.predict.threshold,
        });
        let plugins = l.plugins.clone();
        async move {
            Ok(plugins
                .request("predict", "predict/complete", params, Duration::from_millis(300))
                .await
                .ok()
                .and_then(|v| v["ghost"].as_str().map(String::from))
                .unwrap_or_default())
        }
    })
}

fn completions(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let (ids, error) = match custom_completion_ids(&l.providers) {
            Ok(ids) => (ids, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Ok(Completions { entries: command_entries(&l.plugins, &l.ctx, &l.providers.cfg, &ids), error })
    })
}

/// `{name, arg}`: a skill (`skill:<name>`) or a user command, expanded to the prompt text it sends.
fn expand(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(name, arg): (String, String)| {
        Ok(if let Some(sk) = name.strip_prefix("skill:") {
            match l.ctx.skill_prompt(sk, &arg) {
                Some(p) => Expanded::Prompt(p),
                None => Expanded::UnknownSkill,
            }
        } else if let Some(c) = l.ctx.commands.iter().find(|c| c.name == name) {
            Expanded::Prompt(c.expand(&arg))
        } else {
            Expanded::UnknownCommand
        })
    })
}

fn skills(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        Ok(l.ctx
            .skills
            .iter()
            .map(|s| SkillRow { name: s.name.clone(), hidden: s.hidden, description: s.description.clone() })
            .collect::<Vec<_>>())
    })
}

fn plugins(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        Ok(l.plugins
            .status()
            .into_iter()
            .map(|(name, label, running, extra)| PluginRow { name, label, running, extra })
            .collect::<Vec<_>>())
    })
}

/// Tells the prediction plugin about a prompt that was sent (when prediction is on).
fn observe(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |text: String| {
        if l.providers.cfg.mu.predict.enabled {
            let params = serde_json::json!({ "text": text, "cwd": l.cwd });
            let plugins = l.plugins.clone();
            tokio::task::spawn_local(async move {
                let _ = plugins.request("predict", "predict/observe", params, Duration::from_secs(2)).await;
            });
        }
        Ok(())
    })
}
