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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub path: String,
    pub bytes: usize,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

fn d_chunk() -> usize {
    48
}
fn d_delay() -> u64 {
    2
}

/// Text step marker, found in the terminal output to time the first rendered token.
pub const MARKER_LEN: usize = "BMK00.00 ".len();

pub fn marker(turn: usize, step: usize) -> String {
    format!("BMK{turn:02}.{step:02} ")
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Scenario> {
        let s: Scenario = toml::from_str(text)?;
        s.validate()?;
        Ok(s)
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
    fn marker_has_the_declared_length() {
        assert_eq!(marker(3, 12).len(), MARKER_LEN);
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
