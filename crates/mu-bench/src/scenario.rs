//! Scenario files: what the fake model says, turn by turn, and the files the tools will find.
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    /// Bytes per streamed delta and the pause after each, so a reply streams like a real one.
    #[serde(default = "d_chunk")]
    pub chunk_bytes: usize,
    #[serde(default = "d_delay")]
    pub chunk_delay_ms: u64,
    /// Files generated in the working directory before mu starts.
    #[serde(default)]
    pub files: Vec<FileSpec>,
    #[serde(rename = "turn")]
    pub turns: Vec<Turn>,
}

/// A generated file. `path` may contain `{i}`; with `count` > 1 that many files are made, numbered from 0,
/// each `bytes` long or, when `sizes` is given, sized by cycling through it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub path: String,
    #[serde(default)]
    pub bytes: usize,
    #[serde(default)]
    pub sizes: Vec<usize>,
    #[serde(default = "one")]
    pub count: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub prompt: String,
    /// Model replies in order. All but the last call tools; the last ends the turn.
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub thinking_bytes: usize,
    #[serde(default)]
    pub text_bytes: usize,
    #[serde(default, rename = "tool")]
    pub tools: Vec<ToolCall>,
    /// This step this many times in a row; `{i}` in tool arguments becomes the repetition number.
    #[serde(default = "one")]
    pub repeat: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

fn one() -> usize {
    1
}
fn d_chunk() -> usize {
    48
}
fn d_delay() -> u64 {
    2
}

/// Each turn's text starts with one distinct character (①, ②, …), found in the terminal output to time the
/// first visible text. One character, because mu reveals streamed text a few characters per frame.
pub const MARKER_LEN: usize = 4;
pub const MAX_TURNS: usize = 50;

pub fn marker(turn: usize) -> String {
    let cp = match turn {
        0..20 => 0x2460 + turn,
        20..35 => 0x3251 + turn - 20,
        _ => 0x32B1 + turn.min(MAX_TURNS - 1) - 35,
    };
    format!("{} ", char::from_u32(cp as u32).unwrap())
}

impl Scenario {
    /// Streamed bytes per second, or `None` when replies are sent without pauses.
    pub fn stream_rate(&self) -> Option<u64> {
        (self.chunk_delay_ms > 0).then(|| self.chunk_bytes as u64 * 1000 / self.chunk_delay_ms)
    }

    pub fn parse(text: &str) -> Result<Scenario> {
        let mut s: Scenario = toml::from_str(text)?;
        s.expand()?;
        s.validate()?;
        Ok(s)
    }

    /// Turns `count`, `sizes` and `repeat` into plain files and steps.
    fn expand(&mut self) -> Result<()> {
        let mut files = Vec::new();
        for f in &self.files {
            if f.count == 0 || f.count > 10_000 {
                bail!("file `{}`: count must be 1 to 10000", f.path);
            }
            for i in 0..f.count {
                let bytes = if f.sizes.is_empty() { f.bytes } else { f.sizes[i % f.sizes.len()] };
                if bytes == 0 {
                    bail!("file `{}` needs bytes or sizes", f.path);
                }
                files.push(FileSpec { path: f.path.replace("{i}", &i.to_string()), bytes, sizes: vec![], count: 1 });
            }
        }
        self.files = files;
        for turn in &mut self.turns {
            let mut steps = Vec::new();
            for s in &turn.steps {
                if s.repeat == 0 || s.repeat > 1000 {
                    bail!("step repeat must be 1 to 1000");
                }
                for i in 0..s.repeat {
                    let tools =
                        s.tools.iter().map(|t| ToolCall { name: t.name.clone(), args: subst(&t.args, i) }).collect();
                    steps.push(Step { thinking_bytes: s.thinking_bytes, text_bytes: s.text_bytes, tools, repeat: 1 });
                }
            }
            turn.steps = steps;
        }
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> Result<Scenario> {
        Self::parse(&std::fs::read_to_string(path)?).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        if self.turns.is_empty() {
            bail!("scenario `{}` has no turns", self.name);
        }
        if self.chunk_bytes == 0 {
            bail!("chunk_bytes must be at least 1");
        }
        if self.turns.len() > MAX_TURNS {
            bail!("at most {MAX_TURNS} turns per scenario");
        }
        for (t, turn) in self.turns.iter().enumerate() {
            let Some((last, rest)) = turn.steps.split_last() else {
                bail!("turn {t} has no steps");
            };
            if !last.tools.is_empty() {
                bail!("turn {t}: the last step must not call tools, or the turn never ends");
            }
            if let Some(i) = rest.iter().position(|s| s.tools.is_empty()) {
                bail!("turn {t}, step {i}: only the last step may be without tool calls");
            }
            for (i, s) in turn.steps.iter().enumerate() {
                if s.text_bytes != 0 && s.text_bytes < MARKER_LEN {
                    bail!("turn {t}, step {i}: text_bytes must be 0 or at least {MARKER_LEN}");
                }
            }
        }
        Ok(())
    }
}

/// `v` with `{i}` replaced by `i` in every string.
fn subst(v: &Value, i: usize) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{i}", &i.to_string())),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst(x, i)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), subst(x, i))).collect()),
        other => other.clone(),
    }
}

/// Deterministic prose-like ASCII of exactly `bytes` bytes.
pub fn gen_text(seed: u64, bytes: usize) -> String {
    const WORDS: &[&str] = &[
        "the", "value", "returns", "parser", "buffer", "when", "stream", "state", "config", "input", "error", "handle",
        "update", "after", "before", "render", "token", "session", "tool", "result", "line", "file", "cache", "model",
    ];
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = String::with_capacity(bytes + 16);
    let mut col = 0;
    while out.len() < bytes {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let w = WORDS[(x % WORDS.len() as u64) as usize];
        out.push_str(w);
        col += w.len() + 1;
        if col > 70 {
            out.push('\n');
            col = 0;
        } else {
            out.push(' ');
        }
    }
    out.truncate(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
name = "t"
[[turn]]
prompt = "p"
[[turn.step]]
thinking_bytes = 10
[[turn.step.tool]]
name = "read"
args = { path = "a.txt" }
[[turn.step]]
text_bytes = 100
"#;

    #[test]
    fn parses_nested_tables() {
        let s = Scenario::parse(OK).unwrap();
        assert_eq!((s.chunk_bytes, s.chunk_delay_ms), (48, 2));
        assert_eq!(s.stream_rate(), Some(24_000));
        let unpaced = Scenario::parse(&OK.replace("name = \"t\"", "name = \"t\"\nchunk_delay_ms = 0")).unwrap();
        assert_eq!(unpaced.stream_rate(), None);
        assert_eq!(s.turns[0].steps[0].tools[0].args["path"], "a.txt");
        assert_eq!(s.turns[0].steps[1].text_bytes, 100);
    }

    #[test]
    fn rejects_turns_that_cannot_finish() {
        let tool_last = format!("{OK}[[turn.step.tool]]\nname = \"read\"\nargs = {{ path = \"a\" }}\n");
        assert!(Scenario::parse(&tool_last).unwrap_err().to_string().contains("last step"));
        let bare_first = OK
            .replace("thinking_bytes = 10", "text_bytes = 20")
            .replace("[[turn.step.tool]]\nname = \"read\"\nargs = { path = \"a.txt\" }\n", "");
        assert!(Scenario::parse(&bare_first).unwrap_err().to_string().contains("only the last step"));
        assert!(Scenario::parse("name = \"t\"\nturn = []").is_err());
        assert!(Scenario::parse(&OK.replace("text_bytes = 100", "text_bytes = 3")).is_err());
        assert!(Scenario::parse(&OK.replace("name = \"t\"", "name = \"t\"\nbogus = 1")).is_err());
    }

    #[test]
    fn generated_text_is_exact_and_stable() {
        let a = gen_text(7, 1000);
        assert_eq!(a.len(), 1000);
        assert_eq!(a, gen_text(7, 1000));
        assert_ne!(a, gen_text(8, 1000));
        assert!(a.is_ascii());
        assert_eq!(gen_text(1, 0), "");
    }

    #[test]
    fn markers_are_distinct_and_fixed_length() {
        let all: std::collections::HashSet<_> = (0..MAX_TURNS).map(marker).collect();
        assert_eq!(all.len(), MAX_TURNS);
        assert!(all.iter().all(|m| m.len() == MARKER_LEN));
        assert_eq!(marker(0), "① ");
    }

    #[test]
    fn shipped_scenarios_are_valid() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
        let found: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
        assert!(!found.is_empty());
        for p in found {
            Scenario::load(&p).unwrap();
        }
    }
}

#[cfg(test)]
mod expand_tests {
    use super::*;

    #[test]
    fn repeat_numbers_the_tool_arguments() {
        let s = Scenario::parse(
            r#"
name = "t"
[[turn]]
prompt = "p"
[[turn.step]]
repeat = 3
[[turn.step.tool]]
name = "read"
args = { path = "src/f{i}.rs", range = ["{i}", 2] }
[[turn.step]]
text_bytes = 50
"#,
        )
        .unwrap();
        let steps = &s.turns[0].steps;
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[2].tools[0].args["path"], "src/f2.rs");
        assert_eq!(steps[1].tools[0].args["range"][0], "1");
        assert!(steps.iter().all(|s| s.repeat == 1));
    }

    #[test]
    fn files_cycle_through_sizes() {
        let s = Scenario::parse(
            "name = \"t\"\n[[files]]\npath = \"a/f{i}.rs\"\ncount = 5\nsizes = [100, 200]\n[[files]]\npath = \"one.txt\"\nbytes = 7\n[[turn]]\nprompt = \"p\"\n[[turn.step]]\ntext_bytes = 20\n",
        )
        .unwrap();
        let got: Vec<_> = s.files.iter().map(|f| (f.path.as_str(), f.bytes)).collect();
        assert_eq!(
            got,
            [("a/f0.rs", 100), ("a/f1.rs", 200), ("a/f2.rs", 100), ("a/f3.rs", 200), ("a/f4.rs", 100), ("one.txt", 7)]
        );
    }

    #[test]
    fn bad_counts_are_refused() {
        let base = "name = \"t\"\n[[turn]]\nprompt = \"p\"\n[[turn.step]]\ntext_bytes = 20\n";
        assert!(Scenario::parse(&format!("name = \"t\"\n[[files]]\npath = \"a\"\n{}", &base[10..])).is_err());
        assert!(Scenario::parse(&format!("{base}repeat = 0\n")).is_err());
        assert!(Scenario::parse(&format!("{base}repeat = 5000\n")).is_err());
    }
}
