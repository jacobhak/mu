//! Memory sampling from `/proc`: `Pss` and `Pss_Anon` per process, summed over a process tree.
use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Mem {
    pub pss_kb: u64,
    pub pss_anon_kb: u64,
}

impl std::ops::AddAssign for Mem {
    fn add_assign(&mut self, o: Mem) {
        self.pss_kb += o.pss_kb;
        self.pss_anon_kb += o.pss_anon_kb;
    }
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub comm: String,
    pub mem: Mem,
}

pub fn parse_rollup(text: &str) -> Mem {
    let mut m = Mem::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let kb = v.trim().strip_suffix("kB").and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        match k {
            "Pss" => m.pss_kb = kb,
            "Pss_Anon" => m.pss_anon_kb = kb,
            _ => {}
        }
    }
    m
}

/// The parent pid from `/proc/<pid>/stat`. The command name may contain spaces and parentheses, so
/// fields are counted after the last `)`.
pub fn parse_ppid(stat: &str) -> Option<i32> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

pub fn parse_comm(stat: &str) -> Option<&str> {
    Some(&stat[stat.find('(')? + 1..stat.rfind(')')?])
}

/// `root` and everything below it, given `(pid, ppid)` edges. Parents come before children.
pub fn descendants(root: i32, edges: &[(i32, i32)]) -> Vec<i32> {
    let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
    for &(pid, ppid) in edges {
        kids.entry(ppid).or_default().push(pid);
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(k) = kids.get(&out[i]) {
            let fresh: Vec<i32> = k.iter().copied().filter(|p| !out.contains(p)).collect();
            out.extend(fresh);
        }
        i += 1;
    }
    out
}

fn all_edges() -> Vec<(i32, i32)> {
    let Ok(rd) = fs::read_dir("/proc") else { return Vec::new() };
    rd.filter_map(|e| {
        let pid: i32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        Some((pid, parse_ppid(&stat)?))
    })
    .collect()
}

/// Memory of `root` and its descendants. A process that exits mid-scan is skipped.
pub fn sample_tree(root: i32) -> Vec<Proc> {
    descendants(root, &all_edges())
        .into_iter()
        .filter_map(|pid| {
            let mem = parse_rollup(&fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?);
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            Some(Proc { pid, comm: parse_comm(&stat).unwrap_or("?").into(), mem })
        })
        .collect()
}

pub fn total(procs: &[Proc]) -> Mem {
    let mut t = Mem::default();
    for p in procs {
        t += p.mem;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_fields() {
        let text = "55d0-7ffc ---p 00000000 00:00 0   [rollup]\nRss:    9000 kB\nPss:    7168 kB\nPss_Anon:    1024 kB\nPss_File:  6000 kB\n";
        assert_eq!(parse_rollup(text), Mem { pss_kb: 7168, pss_anon_kb: 1024 });
        assert_eq!(parse_rollup(""), Mem::default());
    }

    #[test]
    fn stat_with_awkward_command_name() {
        let stat = "123 (we ird) name) S 77 123 123 0 -1 4194560";
        assert_eq!(parse_ppid(stat), Some(77));
        assert_eq!(parse_comm(stat), Some("we ird) name"));
        assert_eq!(parse_ppid("garbage"), None);
    }

    #[test]
    fn tree_walk_follows_parents_and_ignores_strangers() {
        let edges = [(10, 1), (11, 10), (12, 11), (13, 10), (20, 1), (21, 20)];
        assert_eq!(descendants(10, &edges), [10, 11, 13, 12]);
        assert_eq!(descendants(99, &edges), [99]);
    }

    #[test]
    fn own_process_is_sampled() {
        let me = std::process::id() as i32;
        let procs = sample_tree(me);
        assert_eq!(procs[0].pid, me);
        assert!(procs[0].mem.pss_anon_kb > 0 && procs[0].mem.pss_kb >= procs[0].mem.pss_anon_kb);
    }
}
