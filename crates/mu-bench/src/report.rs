//! Aggregates repeated runs of one scenario and prints them.
use crate::driver::RunResult;
use serde::Serialize;

pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

/// `(min, max)`, or `None` for no values.
pub fn range(v: &[f64]) -> Option<(f64, f64)> {
    let lo = v.iter().copied().min_by(f64::total_cmp)?;
    let hi = v.iter().copied().max_by(f64::total_cmp)?;
    Some((lo, hi))
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub label: String,
    pub content_kb: f64,
    pub mu_anon_kb: f64,
    pub mu_pss_kb: f64,
    pub tree_pss_kb: f64,
    /// Spread of `mu_anon_kb` over the runs.
    pub mu_anon_range_kb: (f64, f64),
    pub first_token_ms: Option<f64>,
    pub turn_ms: Option<f64>,
    pub procs: usize,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub scenario: String,
    pub runs: usize,
    pub startup_first_byte_ms: f64,
    pub startup_prompt_ms: f64,
    pub rows: Vec<Row>,
    pub tool_errors: usize,
    pub overruns: usize,
    pub aux_requests: usize,
    pub unrouted: Vec<String>,
}

pub fn summarize(scenario: &str, runs: &[RunResult]) -> Summary {
    let col = |f: &dyn Fn(&RunResult) -> Option<f64>| runs.iter().filter_map(f).collect::<Vec<_>>();
    let n_rows = runs.iter().map(|r| r.samples.len()).min().unwrap_or(0);
    let rows = (0..n_rows)
        .map(|i| {
            let at = |f: &dyn Fn(&crate::driver::Sample) -> f64| col(&|r| Some(f(&r.samples[i])));
            let anon = at(&|s| s.mu.pss_anon_kb as f64);
            let tok = col(&|r| i.checked_sub(1).and_then(|t| r.first_token_ms.get(t).copied().flatten()));
            let turn = col(&|r| i.checked_sub(1).and_then(|t| r.turn_ms.get(t).copied()));
            Row {
                label: runs[0].samples[i].label.clone(),
                content_kb: median(&at(&|s| s.content_bytes as f64)) / 1024.0,
                mu_anon_kb: median(&anon),
                mu_pss_kb: median(&at(&|s| s.mu.pss_kb as f64)),
                tree_pss_kb: median(&at(&|s| s.tree.pss_kb as f64)),
                mu_anon_range_kb: range(&anon).unwrap_or((f64::NAN, f64::NAN)),
                first_token_ms: (!tok.is_empty()).then(|| median(&tok)),
                turn_ms: (!turn.is_empty()).then(|| median(&turn)),
                procs: runs[0].samples[i].procs.len(),
            }
        })
        .collect();
    Summary {
        scenario: scenario.into(),
        runs: runs.len(),
        startup_first_byte_ms: median(&col(&|r| r.startup_first_byte_ms)),
        startup_prompt_ms: median(&col(&|r| Some(r.startup_prompt_ms))),
        rows,
        tool_errors: runs.iter().map(|r| r.tool_errors).sum(),
        overruns: runs.iter().map(|r| r.overruns).sum(),
        aux_requests: runs.iter().map(|r| r.aux_requests).sum(),
        unrouted: {
            let mut u: Vec<String> = runs.iter().flat_map(|r| r.unrouted.clone()).collect();
            u.sort();
            u.dedup();
            u
        },
    }
}

fn mb(kb: f64) -> String {
    format!("{:.1}", kb / 1024.0)
}

pub fn print(s: &Summary) {
    println!("\n== {} ({} run{}, medians) ==", s.scenario, s.runs, if s.runs == 1 { "" } else { "s" });
    println!("startup: first output {:.0} ms, prompt drawn {:.0} ms", s.startup_first_byte_ms, s.startup_prompt_ms);
    println!(
        "{:<8} {:>11} {:>14} {:>16} {:>12} {:>13} {:>10} {:>6}",
        "", "content kB", "mu private MB", "(range over runs)", "mu total MB", "tree total MB", "1st token", "procs"
    );
    for r in &s.rows {
        let tok = r.first_token_ms.map_or("-".into(), |t| format!("{t:.0} ms"));
        println!(
            "{:<8} {:>11} {:>14} {:>16} {:>12} {:>13} {:>10} {:>6}",
            r.label,
            format!("{:.0}", r.content_kb),
            mb(r.mu_anon_kb),
            format!("{}-{}", mb(r.mu_anon_range_kb.0), mb(r.mu_anon_range_kb.1)),
            mb(r.mu_pss_kb),
            mb(r.tree_pss_kb),
            tok,
            r.procs
        );
    }
    if s.tool_errors > 0 || s.overruns > 0 {
        println!(
            "WARNING: {} tool error(s) and {} unscripted main request(s): mu did not follow the script",
            s.tool_errors, s.overruns
        );
    }
    if s.aux_requests > 0 {
        println!("note: {} request(s) without tools were answered with a canned reply", s.aux_requests);
    }
    if !s.unrouted.is_empty() {
        println!("note: mu also called routes the fake server does not serve: {}", s.unrouted.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Sample;
    use crate::smaps::Mem;

    fn run(anon: &[u64], tok: Option<f64>) -> RunResult {
        RunResult {
            startup_first_byte_ms: Some(10.0),
            startup_prompt_ms: 40.0,
            samples: anon
                .iter()
                .enumerate()
                .map(|(i, a)| Sample {
                    label: if i == 0 { "start".into() } else { format!("turn {i}") },
                    content_bytes: i as u64 * 2048,
                    mu: Mem { pss_kb: a + 100, pss_anon_kb: *a },
                    tree: Mem { pss_kb: a + 200, pss_anon_kb: *a },
                    procs: vec!["mu".into()],
                })
                .collect(),
            first_token_ms: vec![tok; anon.len().saturating_sub(1)],
            turn_ms: vec![500.0; anon.len().saturating_sub(1)],
            tool_errors: 0,
            overruns: 0,
            aux_requests: 0,
            unrouted: vec![],
        }
    }

    #[test]
    fn medians_and_ranges() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(median(&[]).is_nan());
        assert_eq!(range(&[3.0, 1.0, 2.0]), Some((1.0, 3.0)));
        assert_eq!(range(&[]), None);
    }

    #[test]
    fn summary_takes_medians_per_sample() {
        let runs = [run(&[1000, 2000], Some(30.0)), run(&[1200, 2400], Some(50.0)), run(&[1100, 2200], None)];
        let s = summarize("t", &runs);
        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.rows[0].mu_anon_kb, 1100.0);
        assert_eq!(s.rows[1].mu_anon_kb, 2200.0);
        assert_eq!(s.rows[1].mu_anon_range_kb, (2000.0, 2400.0));
        assert_eq!(s.rows[1].first_token_ms, Some(40.0));
        assert_eq!(s.rows[0].first_token_ms, None);
        assert_eq!(s.rows[1].content_kb, 2.0);
        assert_eq!(s.startup_prompt_ms, 40.0);
    }
}
