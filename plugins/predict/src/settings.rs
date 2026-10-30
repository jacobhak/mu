//! The `[predict]` keys the plugin reads from `<mu_home>/config.toml`. The host validates the same section
//! and warns; a section that fails to parse here is ignored here too, as it is there.

use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_THREADS: usize = 4;

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Section {
    engine: String,
    threads: usize,
}

impl Default for Section {
    fn default() -> Self {
        Self { engine: "ngram".into(), threads: DEFAULT_THREADS }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// `engine = "smollm"`; any other value is the n-gram.
    pub smollm: bool,
    /// CPU threads for SmolLM; `None` means all cores (`threads = 0`).
    pub threads: Option<usize>,
}

pub fn parse(text: &str) -> Settings {
    let section = text
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("predict").cloned())
        .and_then(|v| v.try_into::<Section>().ok())
        .unwrap_or_default();
    Settings { smollm: section.engine == "smollm", threads: (section.threads > 0).then_some(section.threads) }
}

pub fn read(mu_home: &Path) -> Settings {
    parse(&std::fs::read_to_string(mu_home.join("config.toml")).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_ngram_and_four_threads() {
        assert_eq!(parse(""), Settings { smollm: false, threads: Some(4) });
        assert_eq!(parse("[predict]\nenabled = true\n"), Settings { smollm: false, threads: Some(4) });
    }

    #[test]
    fn smollm_and_threads() {
        assert_eq!(parse("[predict]\nengine = \"smollm\"\nthreads = 2\n"), Settings { smollm: true, threads: Some(2) });
        assert_eq!(parse("[predict]\nengine = \"smollm\"\nthreads = 0\n"), Settings { smollm: true, threads: None });
    }

    #[test]
    fn unknown_engine_is_ngram() {
        assert!(!parse("[predict]\nengine = \"bogus\"\n").smollm);
        assert!(!parse("[predict]\nengine = \"SmolLM\"\n").smollm);
    }

    #[test]
    fn a_bad_section_is_ignored_as_a_whole() {
        assert_eq!(
            parse("[predict]\nengine = \"smollm\"\nthreads = -1\n"),
            Settings { smollm: false, threads: Some(4) }
        );
    }
}
