//! `mu-predict bench`: replays the user's own prompt history and measures keystrokes saved.

use crate::corpus::load_corpus;
use crate::engine::State;
use mu_common::paths::{mu_home, omp_agent_dir};

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
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
        match a.as_str() {
            "--last" => last = v.parse().map_err(|_| "bad --last")?,
            "--threshold" => tau = v.parse().map_err(|_| "bad --threshold")?,
            _ => return Err(format!("unknown option {a}")),
        }
    }
    let rows = load_corpus(&omp_agent_dir(), &mu_home(), &crate::home());
    let dir = std::env::temp_dir().join(format!("mu-predict-bench-{}", std::process::id()));
    let mut st = State::open(&dir)?;
    let eval_from = rows.len().saturating_sub(last);
    let cfgs = [("ngram τ=0.15 (omp default)".to_string(), 0.15f32, false), (format!("ngram τ={tau}"), tau, false), (format!("mu τ={tau}"), tau, true)];
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
                    if *use_line {
                        if let Some(l) = &g.line {
                            return l.clone();
                        }
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
        println!("{name:<30} {:>7.2}% {:>20.2} {:>10}", ksr * 100.0, t.wrong as f64 * 100.0 / t.words.max(1) as f64, t.cost);
    }
    Ok(())
}
