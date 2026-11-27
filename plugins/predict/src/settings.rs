//! The `[predict]` keys the plugin reads from `<mu_home>/config.toml` and the profile's. The host validates the same section
//! and warns; a section that fails to parse here is ignored here too, as it is there.

use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_THREADS: usize = 4;

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Section {
    engine: String,
    threads: usize,
    other_histories: bool,
}

impl Default for Section {
    fn default() -> Self {
        Self { engine: "ngram".into(), threads: DEFAULT_THREADS, other_histories: true }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// `engine = "smollm"`; any other value is the n-gram.
    pub smollm: bool,
    /// CPU threads for SmolLM; `None` means all cores (`threads = 0`).
    pub threads: Option<usize>,
    /// Learn from omp's `history.db` and Claude Code's `history.jsonl` as well as mu's own.
    pub other_histories: bool,
}

#[cfg(test)]
pub fn parse(text: &str) -> Settings {
    from_table(&text.parse::<toml::Table>().unwrap_or_default())
}

fn from_table(t: &toml::Table) -> Settings {
    let section = t.get("predict").cloned().and_then(|v| v.try_into::<Section>().ok()).unwrap_or_default();
    Settings {
        smollm: section.engine == "smollm",
        threads: (section.threads > 0).then_some(section.threads),
        other_histories: section.other_histories,
    }
}

/// The settings of the active profile: its `config.toml` over `<mu_home>/config.toml`.
pub fn read(mu_home: &Path) -> Settings {
    from_table(&mu_common::config::load_in(mu_home, mu_common::paths::profile().as_deref()).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_ngram_and_four_threads() {
        assert_eq!(parse(""), Settings { smollm: false, threads: Some(4), other_histories: true });
        assert_eq!(
            parse("[predict]\nenabled = true\n"),
            Settings { smollm: false, threads: Some(4), other_histories: true }
        );
    }

    #[test]
    fn other_tools_histories_can_be_turned_off() {
        assert!(!parse("[predict]\nother_histories = false\n").other_histories);
        assert!(parse("[predict]\nother_histories = true\n").other_histories);
    }

    #[test]
    fn smollm_and_threads() {
        let s = |smollm, threads| Settings { smollm, threads, other_histories: true };
        assert_eq!(parse("[predict]\nengine = \"smollm\"\nthreads = 2\n"), s(true, Some(2)));
        assert_eq!(parse("[predict]\nengine = \"smollm\"\nthreads = 0\n"), s(true, None));
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
            Settings { smollm: false, threads: Some(4), other_histories: true }
        );
    }
}
