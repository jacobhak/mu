//! Opening the SmolLM2 word engine from verified weights: the CPU thread cap, the startup prompt history
//! and the startup plan (SmolLM, or the n-gram with a reason).

use crate::corpus::Row;
use crate::settings::Settings;
use crate::weights::{self, Pinned};
use pi_predict::{Config, Predictor};
use std::collections::HashSet;
use std::path::Path;

/// Prompt text fed at startup, in chars. The crate keeps 1500 to 2500 UTF-16 units of prompts.
const HISTORY_CHARS: usize = 2500;

/// The crate's persisted recent-prompt file in its state dir (`pi_predict::smollm`, `STATE_FILE`).
const MEMORY_FILE: &str = "memory.json";

pub enum Start {
    /// The n-gram only: SmolLM is not selected.
    Ngram,
    /// Open SmolLM in the background with this history already observed.
    Load { history: Vec<String> },
    /// SmolLM is selected but cannot be used; the n-gram runs and this is the notice.
    Fallback(String),
}

/// Chooses the engine at startup. Checks only file sizes here; the full hash is `mu-predict setup`'s job.
pub fn start(settings: &Settings, model_dir: &Path, files: &[Pinned], state_dir: &Path, rows: &[Row]) -> Start {
    if !settings.smollm {
        return Start::Ngram;
    }
    match weights::startup_check(model_dir, files) {
        Err(why) => Start::Fallback(fallback_notice(&why)),
        Ok(()) => Start::Load { history: history(rows, &persisted(state_dir)) },
    }
}

pub fn fallback_notice(reason: &str) -> String {
    format!("{reason}. Using the n-gram engine.")
}

/// Opens SmolLM from `model_dir`, keeping its state in `state_dir`, and observes `history` (oldest first).
pub fn open(
    model_dir: &Path,
    state_dir: &Path,
    threads: Option<usize>,
    history: &[String],
) -> Result<Box<dyn Predictor>, String> {
    if let Some(n) = threads {
        pin_cpus(n);
    }
    // The engine returns every suggestion and the caller applies the request's threshold; the crate's
    // own gate would hide suggestions below its default for any lower request.
    let cfg =
        Config { state_dir: state_dir.to_owned(), model_dir: Some(model_dir.to_owned()), show_threshold: Some(0.0) };
    let mut p = pi_predict::smollm::open(&cfg).map_err(|e| format!("{e:#}"))?;
    for prompt in history {
        p.observe(prompt);
    }
    Ok(p)
}

/// Prompts the crate restored from its own memory file, which must not be fed twice.
fn persisted(state_dir: &Path) -> HashSet<String> {
    let Ok(bytes) = std::fs::read(state_dir.join(MEMORY_FILE)) else { return HashSet::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return HashSet::new() };
    v["recent"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_owned)).collect()
}

/// The newest prompts that fit in [`HISTORY_CHARS`], oldest first, skipping those already persisted.
pub fn history(rows: &[Row], persisted: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = 0;
    for r in rows.iter().rev() {
        if chars >= HISTORY_CHARS {
            break;
        }
        if r.text.trim().is_empty() || persisted.contains(&r.text) {
            continue;
        }
        chars += r.text.chars().count();
        out.push(r.text.clone());
    }
    out.reverse();
    out
}

/// Limits SmolLM's CPU pool to `n` cores by narrowing this thread's affinity mask. The crate sizes its
/// rayon pool from `available_parallelism`, which reads that mask, and threads spawned later inherit it.
#[cfg(target_os = "linux")]
fn pin_cpus(n: usize) {
    // SAFETY: plain libc calls on zeroed `cpu_set_t` values; pid 0 is the calling thread.
    unsafe {
        let size = std::mem::size_of::<libc::cpu_set_t>();
        let mut cur: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size, &mut cur) != 0 {
            return;
        }
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let mut taken = 0;
        for cpu in 0..libc::CPU_SETSIZE as usize {
            if taken == n {
                break;
            }
            if libc::CPU_ISSET(cpu, &cur) {
                libc::CPU_SET(cpu, &mut set);
                taken += 1;
            }
        }
        let _ = libc::sched_setaffinity(0, size, &set);
    }
}

/// macOS runs SmolLM on Metal, where the CPU count does not matter, and there is no affinity API.
#[cfg(not(target_os = "linux"))]
fn pin_cpus(_: usize) {}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_predict::Query;
    use std::path::PathBuf;

    fn row(text: &str) -> Row {
        Row { text: text.into(), cwd: String::new(), ts: 0, count: 1 }
    }

    #[test]
    fn history_takes_the_newest_that_fit_oldest_first() {
        let long = "x".repeat(1000);
        let rows: Vec<Row> = ["old", &long, &long, &long, "newest"].iter().map(|t| row(t)).collect();
        let h = history(&rows, &HashSet::new());
        assert_eq!(h.len(), 4, "newest 6 + three 1000s reach the budget before 'old'");
        assert_eq!(h.last().map(String::as_str), Some("newest"));
        assert!(!h.contains(&"old".to_string()));
    }

    #[test]
    fn history_skips_blank_and_already_persisted_prompts() {
        let rows: Vec<Row> = ["a", "  ", "b", "c"].iter().map(|t| row(t)).collect();
        let persisted: HashSet<String> = ["b".to_string()].into();
        assert_eq!(history(&rows, &persisted), ["a", "c"]);
    }

    #[test]
    fn startup_plan_falls_back_with_a_reason_when_weights_are_missing() {
        let dir = std::env::temp_dir().join(format!("mu-predict-plan-{}", std::process::id()));
        let files = [Pinned { name: "w.bin", repo: "r", revision: "v", size: 3, sha256: "" }];
        let on = Settings { smollm: true, threads: Some(4) };
        let Start::Fallback(note) = start(&on, &dir, &files, &dir, &[]) else { panic!("expected a fallback") };
        assert!(
            note.contains("w.bin") && note.contains("mu predict setup smollm") && note.contains("n-gram"),
            "{note}"
        );
        assert!(matches!(start(&Settings { smollm: false, threads: None }, &dir, &files, &dir, &[]), Start::Ngram));
    }

    #[test]
    fn startup_plan_loads_when_sizes_match() {
        let dir = std::env::temp_dir().join(format!("mu-predict-plan-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w.bin"), b"abc").unwrap();
        let files = [Pinned { name: "w.bin", repo: "r", revision: "v", size: 3, sha256: "" }];
        let rows = [row("hello there")];
        let on = Settings { smollm: true, threads: None };
        let Start::Load { history } = start(&on, &dir, &files, &dir.join("state"), &rows) else {
            panic!("expected load")
        };
        assert_eq!(history, ["hello there"]);
    }

    /// Needs the real weights: `MU_SMOLLM_TEST_DIR=<dir with the three pinned files> cargo test -- --ignored`.
    #[test]
    #[ignore = "needs the real SmolLM2-135M weights in MU_SMOLLM_TEST_DIR"]
    fn real_weights_complete_a_word_and_persist() {
        let dir = PathBuf::from(std::env::var("MU_SMOLLM_TEST_DIR").expect("set MU_SMOLLM_TEST_DIR"));
        for f in &weights::FILES {
            assert_eq!(weights::check_file(&dir.join(f.name), f, true), weights::Check::Ok, "{}", f.name);
        }
        let state = std::env::temp_dir().join(format!("mu-predict-real-{}", std::process::id()));
        let mut p = open(&dir, &state, Some(2), &["please refactor the parser module".to_string()]).unwrap();
        let q = Query { before: "Can you please refactor the pars", prefix: "pars" };
        let s = p.complete(&q).expect("a suggestion for a common word");
        assert!(!s.suffix.is_empty() && s.confidence > 0.0 && s.confidence <= 1.0, "{s:?}");
        p.persist().unwrap();
        assert!(state.join("memory.json").is_file());
    }
}
