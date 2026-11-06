//! `mu-predict bench`: replays the user's own prompt history and measures keystrokes saved.

use crate::corpus::{Row, load_corpus};
use crate::engine::{State, split_word};
use crate::settings::DEFAULT_THREADS;
use crate::smollm;
use mu_common::paths::{omp_agent_dir, profile_home};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Ghosts {
    /// Word ghost with its confidence.
    word: Option<(String, f32)>,
    line: Option<String>,
}

struct Tally {
    cost: usize,
    chars: usize,
    wrong: usize,
    words: usize,
}

/// Simulates a typist who accepts a ghost whenever it is right. Returns `(cost, wrong ghosts)`.
fn simulate(text: &str, ghost_at: &dyn Fn(usize) -> String) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let (mut i, mut cost, mut wrong) = (0, 0, 0);
    let mut wrong_word: Option<usize> = None;
    while i < chars.len() {
        let g: Vec<char> = ghost_at(i).chars().collect();
        let rest = &chars[i..];
        if !g.is_empty() && rest.starts_with(&g) {
            cost += 1;
            i += g.len();
            if chars.get(i) == Some(&' ') {
                i += 1;
            }
            continue;
        }
        let k_chars = partial_len(&g, rest);
        if k_chars.0 > 0 {
            cost += k_chars.0;
            i += k_chars.1;
            continue;
        }
        if !g.is_empty() {
            let w = chars[..i].iter().filter(|c| c.is_whitespace()).count();
            if wrong_word != Some(w) {
                wrong_word = Some(w);
                wrong += 1;
            }
        }
        cost += 1;
        i += 1;
    }
    (cost, wrong)
}

/// `(tokens, chars)` of the longest run of whole ghost tokens that `rest` starts with; needs at least two tokens in `g`.
fn partial_len(g: &[char], rest: &[char]) -> (usize, usize) {
    let mut segs = Vec::new();
    let mut start = 0;
    let mut in_tok = false;
    for (j, c) in g.iter().enumerate() {
        if c.is_whitespace() {
            if in_tok {
                segs.push(start..j);
                start = j;
                in_tok = false;
            }
        } else {
            in_tok = true;
        }
    }
    if in_tok {
        segs.push(start..g.len());
    }
    if segs.len() < 2 {
        return (0, 0);
    }
    let (mut k, mut n) = (0, 0);
    for s in &segs {
        if rest.len() >= s.end && rest[..s.end] == g[..s.end] {
            k += 1;
            n = s.end;
        } else {
            break;
        }
    }
    (k, n)
}

pub fn run(args: &[String]) -> Result<(), String> {
    let (mut last, mut tau) = (600usize, 0.3f32);
    let (mut model, mut threads, mut latency, mut warm) = (None::<PathBuf>, Some(DEFAULT_THREADS), None::<usize>, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
        match a.as_str() {
            "--last" => last = v.parse().map_err(|_| "bad --last")?,
            "--threshold" => tau = v.parse().map_err(|_| "bad --threshold")?,
            "--smollm" => model = Some(PathBuf::from(v)),
            "--threads" => threads = Some(v.parse::<usize>().map_err(|_| "bad --threads")?).filter(|n| *n > 0),
            "--latency" => latency = Some(v.parse().map_err(|_| "bad --latency")?),
            "--warm" => warm = v != "0",
            _ => return Err(format!("unknown option {a}")),
        }
    }
    let rows = load_corpus(&omp_agent_dir(), &profile_home(), &mu_common::paths::home());
    let dir = std::env::temp_dir().join(format!("mu-predict-bench-{}", std::process::id()));
    if let Some(n) = latency {
        let model = model.ok_or("--latency needs --smollm DIR")?;
        return latency_run(&rows, &model, threads, n, warm, &dir);
    }
    let mut st = State::open(&dir)?;
    if let Some(m) = &model {
        // Cold start like a fresh plugin: no history, then the replay observes every prompt, as production does.
        st.attach_smollm(smollm::open(m, &dir.join("smollm"), threads, &[])?, &[]);
    }
    let engine = if st.smollm_active() { "smollm" } else { "ngram" };
    let eval_from = rows.len().saturating_sub(last);
    let cfgs = [
        (format!("{engine} τ=0.15"), 0.15f32, false),
        (format!("{engine} τ={tau}"), tau, false),
        (format!("{engine} τ={tau} + lines"), tau, true),
    ];
    let mut tallies: Vec<Tally> = cfgs.iter().map(|_| Tally { cost: 0, chars: 0, wrong: 0, words: 0 }).collect();
    let mut evaluated = 0;
    for (n, r) in rows.iter().enumerate() {
        if n >= eval_from && !r.text.contains('\n') {
            evaluated += 1;
            let idx: Vec<usize> = r.text.char_indices().map(|(b, _)| b).collect();
            let ghosts: Vec<Ghosts> = idx
                .iter()
                .map(|&b| {
                    let buf = &r.text[..b];
                    Ghosts {
                        word: st.word_suggestion(buf).map(|s| (s.suffix, s.confidence)),
                        line: st.line_ghost(buf, &r.cwd),
                    }
                })
                .collect();
            let words = r.text.split_whitespace().count();
            for ((_, t, use_line), tally) in cfgs.iter().zip(&mut tallies) {
                let ghost_at = |i: usize| -> String {
                    let g = &ghosts[i];
                    if *use_line && let Some(l) = &g.line {
                        return l.clone();
                    }
                    g.word.as_ref().filter(|(_, c)| c >= t).map(|(s, _)| s.clone()).unwrap_or_default()
                };
                let (cost, wrong) = simulate(&r.text, &ghost_at);
                tally.cost += cost;
                tally.wrong += wrong;
                tally.chars += idx.len();
                tally.words += words;
            }
        }
        // Whole-prompt repeats count once per submission here (no omp use_count: that is future data).
        st.observe(&r.text, &r.cwd);
    }
    println!("evaluated {evaluated} prompts (replayed {} rows)", rows.len());
    println!("{:<30} {:>8} {:>20} {:>10}", "config", "KSR", "wrong/100 words", "keystrokes");
    for ((name, _, _), t) in cfgs.iter().zip(&tallies) {
        let ksr = 1.0 - t.cost as f64 / t.chars.max(1) as f64;
        println!(
            "{name:<30} {:>7.2}% {:>20.2} {:>10}",
            ksr * 100.0,
            t.wrong as f64 * 100.0 / t.words.max(1) as f64,
            t.cost
        );
    }
    Ok(())
}

/// Per-query latency of `State::complete` with SmolLM attached, over the last prompts of the history: the
/// production path, with the startup history fed first and each prompt observed after its queries.
fn latency_run(
    rows: &[Row],
    model: &Path,
    threads: Option<usize>,
    n: usize,
    warm: bool,
    dir: &Path,
) -> Result<(), String> {
    let start = rows.len().saturating_sub(600);
    let rss_before = rss_mb();
    let t = Instant::now();
    let history = smollm::history(&rows[..start], &HashSet::new());
    let p = smollm::open(model, &dir.join("smollm"), threads, &history)?;
    let open = t.elapsed();
    let mut st = State::open(dir)?;
    let t = Instant::now();
    st.attach_smollm(p, &[]);
    if warm {
        st.warm();
    }
    let attach_warm = t.elapsed();
    let rss_open = rss_mb();
    let mut times: Vec<Duration> = Vec::with_capacity(n);
    let mut slowest: Vec<(Duration, String)> = Vec::new();
    'rows: for r in &rows[start..] {
        for (b, _) in r.text.char_indices() {
            let buf = &r.text[..b];
            if !split_word(buf).is_some_and(|(_, p)| p.chars().count() >= 2) {
                continue;
            }
            let t = Instant::now();
            let _ = st.complete(buf, &r.cwd, "", 0.3);
            times.push(t.elapsed());
            let tail: String = buf.chars().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect();
            slowest.push((t.elapsed(), tail));
            slowest.sort_by_key(|a| std::cmp::Reverse(a.0));
            slowest.truncate(4);
            if times.len() == n {
                break 'rows;
            }
        }
        st.observe(&r.text, &r.cwd);
        if warm {
            // Production warms while the queue is idle, between prompts; this is outside the timed queries.
            st.warm();
        }
    }
    let rss_after = rss_mb();
    times.sort();
    let pct = |p: f64| -> f64 {
        let i = ((p * times.len() as f64).ceil() as usize).clamp(1, times.len().max(1)) - 1;
        times.get(i).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    let mb = |v: Option<f64>| v.map_or("n/a".to_string(), |v| format!("{v:.0} MB"));
    println!(
        "threads {}: open {:.0} ms, attach+warm {:.0} ms; RSS before {}, after open {}, after queries {}",
        threads.map_or("all".to_string(), |n| n.to_string()),
        open.as_secs_f64() * 1000.0,
        attach_warm.as_secs_f64() * 1000.0,
        mb(rss_before),
        mb(rss_open),
        mb(rss_after)
    );
    println!(
        "{} queries: p50 {:.1} ms  p90 {:.1} ms  p99 {:.1} ms  max {:.1} ms",
        times.len(),
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0)
    );
    for (d, q) in &slowest {
        println!("  slowest {:>8.1} ms after {q:?}", d.as_secs_f64() * 1000.0);
    }
    Ok(())
}

/// Resident set size of this process in MB (Linux only).
fn rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.split_whitespace().next()?.parse().ok()?;
    Some(kb / 1024.0)
}
