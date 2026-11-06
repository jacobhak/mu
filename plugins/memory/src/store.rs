//! The memory model: entry files (`user.md`, `memory.md`), notes, and the edit ops that change them.
//! Everything here is pure; `mem` does the file, lock and git work around it.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Separates entries in an entry file. It never occurs in normal prose.
pub const DELIM: &str = "§";
pub const MAX_NOTES: usize = 40;
const MAX_SLUG: usize = 60;
/// Entries listed when a call fails, so one reply is enough to retry.
const LIST_CAP: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum File {
    User,
    Project,
}

impl File {
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub user: usize,
    pub project: usize,
}

impl Caps {
    pub fn of(self, f: File) -> usize {
        match f {
            File::User => self.user,
            File::Project => self.project,
        }
    }
}

impl Default for Caps {
    fn default() -> Self {
        Self { user: 1500, project: 4000 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteType {
    #[default]
    Project,
    Reference,
    Feedback,
}

impl NoteType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Reference => "reference",
            Self::Feedback => "feedback",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim() {
            "reference" => Self::Reference,
            "feedback" => Self::Feedback,
            _ => Self::Project,
        }
    }
}

/// One edit. The same schema is used by the tool and by the background review.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Add {
        file: File,
        text: String,
    },
    Replace {
        file: File,
        #[serde(rename = "match")]
        pattern: String,
        text: String,
    },
    Remove {
        file: File,
        #[serde(rename = "match")]
        pattern: String,
    },
    WriteNote {
        note: String,
        #[serde(default)]
        description: String,
        #[serde(default, rename = "type")]
        kind: NoteType,
        text: String,
    },
    DeleteNote {
        note: String,
    },
}

impl Op {
    fn label(&self) -> String {
        match self {
            Self::Add { file, .. } => format!("add {}", file.name()),
            Self::Replace { file, .. } => format!("replace {}", file.name()),
            Self::Remove { file, .. } => format!("remove {}", file.name()),
            Self::WriteNote { note, .. } => format!("write_note {note}"),
            Self::DeleteNote { note } => format!("delete_note {note}"),
        }
    }

    /// Every text the op carries, for redaction.
    pub fn texts_mut(&mut self) -> Vec<&mut String> {
        match self {
            Self::Add { text, .. } | Self::Replace { text, .. } => vec![text],
            Self::WriteNote { description, text, .. } => vec![description, text],
            Self::Remove { .. } | Self::DeleteNote { .. } => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    pub description: String,
    pub kind: NoteType,
    pub body: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub user: Vec<String>,
    pub project: Vec<String>,
    pub notes: BTreeMap<String, Note>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Entry files whose content changed.
    pub files: BTreeSet<File>,
    /// Notes written or deleted.
    pub notes: BTreeSet<String>,
    /// `add project, write_note x`, for the commit message.
    pub summary: String,
}

impl Outcome {
    pub fn changed(&self) -> bool {
        !self.files.is_empty() || !self.notes.is_empty()
    }
}

impl State {
    pub fn entries(&self, f: File) -> &Vec<String> {
        match f {
            File::User => &self.user,
            File::Project => &self.project,
        }
    }

    fn entries_mut(&mut self, f: File) -> &mut Vec<String> {
        match f {
            File::User => &mut self.user,
            File::Project => &mut self.project,
        }
    }
}

/// Entries of an entry file. Text without delimiter lines is one entry, so a file written by hand parses.
pub fn parse_entries(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut flush = |cur: &mut Vec<&str>| {
        let e = cur.join("\n");
        let e = e.trim();
        if !e.is_empty() {
            out.push(e.to_string());
        }
        cur.clear();
    };
    for line in text.lines() {
        if line.trim() == DELIM {
            flush(&mut cur);
        } else {
            cur.push(line);
        }
    }
    flush(&mut cur);
    out
}

pub fn format_entries(entries: &[String]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    format!("{}\n", entries.join(&format!("\n{DELIM}\n")))
}

/// The size an entry file is capped on.
pub fn size_of(entries: &[String]) -> usize {
    format_entries(entries).chars().count()
}

/// Whitespace runs, quotes and dashes folded, so a match survives how a model re-types an entry.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.trim().chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            c => c,
        };
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SLUG
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn parse_note(text: &str) -> Note {
    let mut note = Note::default();
    let Some(rest) = text.strip_prefix("---\n") else {
        note.body = text.trim().to_string();
        return note;
    };
    let Some(end) = rest.find("\n---") else {
        note.body = text.trim().to_string();
        return note;
    };
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "description" => note.description = v.trim().to_string(),
                "type" => note.kind = NoteType::parse(v),
                _ => {}
            }
        }
    }
    let body = &rest[end + 4..];
    note.body = body.strip_prefix('\n').unwrap_or(body).trim().to_string();
    note
}

pub fn format_note(n: &Note) -> String {
    format!(
        "---\ndescription: {}\ntype: {}\n---\n\n{}\n",
        n.description.replace('\n', " "),
        n.kind.name(),
        n.body.trim()
    )
}

fn list(entries: &[String]) -> String {
    let mut s: String = entries.iter().take(LIST_CAP).map(|e| format!("- {e}\n")).collect();
    if entries.len() > LIST_CAP {
        s.push_str(&format!("- … {} more\n", entries.len() - LIST_CAP));
    }
    if s.is_empty() {
        s.push_str("(no entries)\n");
    }
    s
}

fn clean_entry(text: &str) -> Result<String, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("an entry cannot be empty".into());
    }
    if t.lines().any(|l| l.trim() == DELIM) {
        return Err(format!("an entry cannot contain a line holding only {DELIM}"));
    }
    Ok(t.to_string())
}

/// The one entry whose folded text contains `pattern`.
fn find(entries: &[String], file: File, pattern: &str) -> Result<usize, String> {
    let p = fold(pattern);
    if p.is_empty() {
        return Err("match cannot be empty".into());
    }
    let hits: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| fold(e).contains(&p)).map(|(i, _)| i).collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!("match {pattern:?} hits no entry in {}. Entries:\n{}", file.name(), list(entries))),
        many => {
            let c: Vec<String> = many.iter().map(|i| entries[*i].clone()).collect();
            Err(format!(
                "match {pattern:?} hits {} entries in {}; use more of the text:\n{}",
                many.len(),
                file.name(),
                list(&c)
            ))
        }
    }
}

/// Applies `ops` in order to a copy of `state`. Caps are checked on the result of the whole batch; any failure
/// leaves nothing applied. A file already over its cap (edited by hand) refuses edits.
pub fn apply(state: &State, ops: &[Op], caps: &Caps) -> Result<(State, Outcome), String> {
    let mut next = state.clone();
    let mut touched: BTreeSet<File> = BTreeSet::new();
    let mut notes: BTreeSet<String> = BTreeSet::new();
    for op in ops {
        let file = match op {
            Op::Add { file, .. } | Op::Replace { file, .. } | Op::Remove { file, .. } => Some(*file),
            _ => None,
        };
        if let Some(f) = file {
            let size = size_of(state.entries(f));
            if size > caps.of(f) {
                return Err(format!(
                    "{} memory is {size} characters, over its cap of {}: it was edited by hand. Shorten it by hand; \
                     automatic edits to it resume once it fits.",
                    f.name(),
                    caps.of(f)
                ));
            }
        }
        match op {
            Op::Add { file, text } => {
                let t = clean_entry(text)?;
                let es = next.entries_mut(*file);
                if !es.contains(&t) {
                    es.push(t);
                }
            }
            Op::Replace { file, pattern, text } => {
                let t = clean_entry(text)?;
                let i = find(next.entries(*file), *file, pattern)?;
                next.entries_mut(*file)[i] = t;
            }
            Op::Remove { file, pattern } => {
                let i = find(next.entries(*file), *file, pattern)?;
                next.entries_mut(*file).remove(i);
            }
            Op::WriteNote { note, description, kind, text } => {
                if !valid_slug(note) {
                    return Err(format!(
                        "note name {note:?} must be lowercase letters, digits and dashes (at most {MAX_SLUG})"
                    ));
                }
                if text.trim().is_empty() {
                    return Err(format!("note {note} has no text"));
                }
                if description.trim().is_empty() {
                    return Err(format!("note {note} needs a one-line description"));
                }
                if !next.notes.contains_key(note) && next.notes.len() >= MAX_NOTES {
                    let names: Vec<&str> = next.notes.keys().map(String::as_str).collect();
                    return Err(format!(
                        "at most {MAX_NOTES} notes per project; delete one first. Notes: {}",
                        names.join(", ")
                    ));
                }
                let n =
                    Note { description: description.trim().to_string(), kind: *kind, body: text.trim().to_string() };
                next.notes.insert(note.clone(), n);
                notes.insert(note.clone());
            }
            Op::DeleteNote { note } => {
                if next.notes.remove(note).is_none() {
                    let names: Vec<&str> = next.notes.keys().map(String::as_str).collect();
                    return Err(format!("no note {note:?}. Notes: {}", names.join(", ")));
                }
                notes.insert(note.clone());
            }
        }
        if let Some(f) = file {
            touched.insert(f);
        }
    }
    for f in [File::User, File::Project] {
        let size = size_of(next.entries(f));
        if touched.contains(&f) && size > caps.of(f) {
            return Err(format!(
                "{} memory would be {size} characters, {} over its cap of {}. Consolidate in the same call: replace \
                 or remove entries so the batch fits. Current entries:\n{}",
                f.name(),
                size - caps.of(f),
                caps.of(f),
                list(state.entries(f))
            ));
        }
    }
    let mut out = Outcome::default();
    for f in touched {
        if next.entries(f) != state.entries(f) {
            out.files.insert(f);
        }
    }
    // A note written with identical content, or deleted after being added in the same batch, is no change.
    for n in notes {
        if next.notes.get(&n) != state.notes.get(&n) {
            out.notes.insert(n);
        }
    }
    out.summary = ops.iter().map(Op::label).collect::<Vec<_>>().join(", ");
    Ok((next, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn add(file: File, text: &str) -> Op {
        Op::Add { file, text: text.into() }
    }

    fn state() -> State {
        State {
            user: s(&["Prefers terse replies", "Uses Neovim"]),
            project: s(&["Releases are cut from main"]),
            notes: Default::default(),
        }
    }

    #[test]
    fn parses_delimited_and_hand_written_files() {
        assert_eq!(parse_entries("a\n§\nb\nc\n§\n\n§\n d \n"), s(&["a", "b\nc", "d"]));
        assert_eq!(
            parse_entries("one\n\ntwo paragraphs\nwithout delimiters\n"),
            s(&["one\n\ntwo paragraphs\nwithout delimiters"])
        );
        assert!(parse_entries("  \n").is_empty());
        let es = s(&["a", "b\nc"]);
        assert_eq!(parse_entries(&format_entries(&es)), es);
        assert_eq!(format_entries(&[]), "");
    }

    #[test]
    fn fold_ignores_whitespace_quotes_and_dashes() {
        assert_eq!(fold("  it\u{2019}s  a\n \u{2014} \u{201c}x\u{201d} "), "it's a - \"x\"");
    }

    #[test]
    fn every_op_applies() {
        let ops = [
            add(File::User, "Likes tests"),
            Op::Replace { file: File::User, pattern: "neovim".into(), text: "x".into() },
        ];
        // Matching is case-sensitive: this one misses.
        assert!(apply(&state(), &ops, &Caps::default()).unwrap_err().contains("hits no entry"));
        let ops = [
            add(File::User, "Likes tests"),
            Op::Replace { file: File::User, pattern: "Uses  Neovim".into(), text: "Uses Helix".into() },
            Op::Remove { file: File::Project, pattern: "Releases".into() },
            Op::WriteNote {
                note: "release-process".into(),
                description: "How releases work".into(),
                kind: NoteType::Reference,
                text: "Tag main.".into(),
            },
        ];
        let (next, out) = apply(&state(), &ops, &Caps::default()).unwrap();
        assert_eq!(next.user, s(&["Prefers terse replies", "Uses Helix", "Likes tests"]));
        assert!(next.project.is_empty());
        assert_eq!(next.notes["release-process"].kind, NoteType::Reference);
        assert_eq!(out.files, BTreeSet::from([File::User, File::Project]));
        assert_eq!(out.notes, BTreeSet::from(["release-process".to_string()]));
        assert_eq!(out.summary, "add user, replace user, remove project, write_note release-process");
        let (after, out) =
            apply(&next, &[Op::DeleteNote { note: "release-process".into() }], &Caps::default()).unwrap();
        assert!(after.notes.is_empty());
        assert!(out.changed());
    }

    #[test]
    fn exact_duplicate_add_is_a_no_op() {
        let (next, out) = apply(&state(), &[add(File::User, "  Uses Neovim ")], &Caps::default()).unwrap();
        assert_eq!(next, state());
        assert!(!out.changed());
    }

    #[test]
    fn ambiguous_and_missing_matches_list_candidates() {
        let e = apply(&state(), &[Op::Remove { file: File::User, pattern: "s".into() }], &Caps::default()).unwrap_err();
        assert!(
            e.contains("hits 2 entries") && e.contains("- Uses Neovim") && e.contains("- Prefers terse replies"),
            "{e}"
        );
        let e =
            apply(&state(), &[Op::Remove { file: File::User, pattern: "emacs".into() }], &Caps::default()).unwrap_err();
        assert!(e.contains("hits no entry") && e.contains("- Uses Neovim"), "{e}");
    }

    #[test]
    fn caps_are_checked_on_the_batch_result() {
        let caps = Caps { user: 40, project: 4000 };
        let one = [add(File::User, "a very long new entry that cannot fit")];
        let e = apply(&state(), &one, &caps).unwrap_err();
        assert!(e.contains("over its cap of 40") && e.contains("- Uses Neovim"), "{e}");
        // The same batch fits once it also removes an entry: nothing fails midway.
        let both = [
            add(File::User, "a very long new entry that cannot fit"),
            Op::Remove { file: File::User, pattern: "Prefers".into() },
            Op::Remove { file: File::User, pattern: "Neovim".into() },
        ];
        let (next, _) = apply(&state(), &both, &Caps { user: 60, project: 4000 }).unwrap();
        assert_eq!(next.user.len(), 1);
    }

    #[test]
    fn a_file_over_its_cap_after_a_hand_edit_refuses_edits() {
        let big = State { user: vec!["x".repeat(100)], ..Default::default() };
        let caps = Caps { user: 50, project: 4000 };
        let e = apply(&big, &[Op::Remove { file: File::User, pattern: "x".into() }], &caps).unwrap_err();
        assert!(e.contains("edited by hand"), "{e}");
        // The other file and notes are still editable.
        apply(&big, &[add(File::Project, "fine")], &caps).unwrap();
    }

    #[test]
    fn rejects_bad_entries_and_notes() {
        let c = Caps::default();
        assert!(apply(&state(), &[add(File::User, "  ")], &c).is_err());
        assert!(apply(&state(), &[add(File::User, "a\n§\nb")], &c).is_err());
        let note = |name: &str| Op::WriteNote {
            note: name.into(),
            description: "d".into(),
            kind: NoteType::Project,
            text: "t".into(),
        };
        assert!(apply(&state(), &[note("Bad Name")], &c).is_err());
        assert!(apply(&state(), &[note("../x")], &c).is_err());
        assert!(apply(&state(), &[Op::DeleteNote { note: "nope".into() }], &c).is_err());
        let mut full = State::default();
        for i in 0..MAX_NOTES {
            full.notes.insert(format!("n{i}"), Note::default());
        }
        assert!(apply(&full, &[note("one-more")], &c).unwrap_err().contains("at most 40"));
        apply(&full, &[note("n3")], &c).unwrap();
    }

    #[test]
    fn notes_round_trip_through_front_matter() {
        let n = Note { description: "How".into(), kind: NoteType::Feedback, body: "Line one\n\nLine two".into() };
        assert_eq!(parse_note(&format_note(&n)), n);
        let plain = parse_note("just text\n");
        assert_eq!((plain.description.as_str(), plain.body.as_str()), ("", "just text"));
    }

    #[test]
    fn ops_deserialize_from_the_tool_schema() {
        let ops: Vec<Op> = serde_json::from_str(
            r#"[{"op":"add","file":"project","text":"x"},{"op":"replace","file":"user","match":"a","text":"b"},
                {"op":"write_note","note":"n","description":"d","type":"reference","text":"t"},{"op":"delete_note","note":"n"}]"#,
        )
        .unwrap();
        assert_eq!(ops.len(), 4);
        assert!(matches!(&ops[1], Op::Replace { pattern, .. } if pattern == "a"));
        assert!(serde_json::from_str::<Op>(r#"{"op":"add","file":"other","text":"x"}"#).is_err());
    }
}
