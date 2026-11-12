//! What the rules and the mode decide about the subjects of one call. Plain data: it knows nothing of
//! tools, so the same verdict serves the gate in front of a call and the check at a write.

use super::glob_match;
use mu_base::permissions::{Action, Capability, PermissionMode, Rule, RuleSet};
use std::path::{Path, PathBuf};

/// One thing a call touches, judged on its own.
pub(super) struct Subject {
    /// The tool whose rules decide it: the call's tool, or `write` for a bash output redirect.
    pub tool: String,
    pub text: String,
    /// The resolved path, as text, when the subject is a file. Widths and absolute rules use it.
    pub abs: Option<String>,
    /// Other spellings `deny` and `ask` rules also match; never `allow`.
    pub also: Vec<String>,
    /// What the mode has to grant for it to run unasked.
    pub needs: Capability,
    /// An `allow` rule can never cover it (leading assignments, a line that does not parse).
    pub no_allow: bool,
}

impl Subject {
    pub fn new(tool: &str, text: impl Into<String>, needs: Capability) -> Self {
        Self { tool: tool.to_string(), text: text.into(), abs: None, also: Vec::new(), needs, no_allow: false }
    }
}

#[derive(Clone)]
pub(super) struct Hit {
    pub tool: String,
    pub rule: Rule,
    pub source: &'static str,
    /// The first subject the rule matched.
    pub subject: usize,
}

pub(super) enum Outcome {
    Allow,
    Deny(Hit),
    Ask { rule: Option<Hit>, open: Vec<usize> },
}

/// The mode, the rules and the project's shape: everything `decide` needs.
#[derive(Clone)]
pub struct Gate {
    pub(super) mode: PermissionMode,
    pub(super) readonly_bash: bool,
    /// The project root, physical.
    pub(super) root: PathBuf,
    /// mu's home, physical.
    pub(super) mu_home: PathBuf,
    /// Global, project and session rules, with the source each is named by.
    pub(super) rules: [(RuleSet, &'static str); 3],
}

/// A rule matches a subject by its displayed text (relative inside the project) or by its absolute
/// path, so a grant can name the directory a path sits in either way.
pub(super) fn hit(rule: &Rule, s: &Subject) -> bool {
    glob_match(&rule.pattern, &s.text) || s.abs.as_deref().is_some_and(|a| glob_match(&rule.pattern, a))
}

impl Gate {
    pub(super) fn hits(&self, subjects: &[Subject]) -> Vec<Hit> {
        let mut out = Vec::new();
        for (set, source) in &self.rules {
            for (tool, rule) in &set.0 {
                let found = subjects.iter().position(|s| {
                    glob_match(tool, &s.tool)
                        && (hit(rule, s)
                            || (rule.action != Action::Allow && s.also.iter().any(|t| glob_match(&rule.pattern, t))))
                });
                if let Some(subject) = found {
                    out.push(Hit { tool: tool.clone(), rule: rule.clone(), source, subject });
                }
            }
        }
        out
    }

    /// An `allow` rule covers the subject.
    pub(super) fn covered(hits: &[Hit], s: &Subject) -> bool {
        !s.no_allow
            && hits.iter().any(|h| h.rule.action == Action::Allow && glob_match(&h.tool, &s.tool) && hit(&h.rule, s))
    }

    pub(super) fn decide(&self, subjects: &[Subject]) -> (Outcome, Vec<Hit>) {
        let hits = self.hits(subjects);
        if let Some(h) = hits.iter().find(|h| h.rule.action == Action::Deny) {
            return (Outcome::Deny(h.clone()), hits);
        }
        let rule = hits.iter().find(|h| h.rule.action == Action::Ask).cloned();
        let open: Vec<usize> = subjects
            .iter()
            .enumerate()
            .filter(|(_, s)| !(Self::covered(&hits, s) || self.mode.allows(s.needs)))
            .map(|(i, _)| i)
            .collect();
        if rule.is_none() && open.is_empty() {
            return (Outcome::Allow, hits);
        }
        (Outcome::Ask { rule, open }, hits)
    }

    pub(super) fn inside(&self, p: &Path) -> bool {
        p.starts_with(&self.root)
    }

    /// `.git` of the project and mu's own home are never edited by an `edits`-mode call.
    pub(super) fn editable(&self, p: &Path) -> bool {
        self.inside(p) && !p.starts_with(self.root.join(".git")) && !p.starts_with(&self.mu_home)
    }

    /// A path as a subject shows it: relative to the project root inside it, else absolute.
    pub(super) fn shown(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => p.to_string_lossy().into_owned(),
        }
    }

    /// A physical path a call writes.
    pub(super) fn write_subject(&self, tool: &str, p: &Path) -> Subject {
        let needs = if self.editable(p) { Capability::WriteProject } else { Capability::Exec };
        Subject { abs: Some(p.to_string_lossy().into_owned()), ..Subject::new(tool, self.shown(p), needs) }
    }

    /// A physical path a call reads.
    pub(super) fn read_subject(&self, tool: &str, p: &Path) -> Subject {
        let needs = if self.inside(p) { Capability::ReadProject } else { Capability::ReadOutside };
        Subject { abs: Some(p.to_string_lossy().into_owned()), ..Subject::new(tool, self.shown(p), needs) }
    }

    /// Whether a write of the physical path `p` by `tool` runs without asking.
    pub fn allows_write(&self, tool: &str, p: &Path) -> bool {
        matches!(self.decide(&[self.write_subject(tool, p)]).0, Outcome::Allow)
    }
}
