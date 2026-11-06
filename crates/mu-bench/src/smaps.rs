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

/// Memory of every process under any of `roots`, counted once, from a single scan of `/proc`. This is what several
/// sessions hold together at one instant.
pub fn sample_roots(roots: &[i32]) -> Mem {
    let edges = all_edges();
    let mut seen: Vec<i32> = Vec::new();
    let mut t = Mem::default();
    for &root in roots {
        for pid in descendants(root, &edges) {
            if seen.contains(&pid) {
                continue;
            }
            seen.push(pid);
            if let Ok(text) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
                t += parse_rollup(&text);
            }
        }
    }
    t
}

pub fn total(procs: &[Proc]) -> Mem {
    let mut t = Mem::default();
    for p in procs {
        t += p.mem;
    }
    t
}

/// Where a process's memory sits. `pss_kb` counts every resident page; `anon_kb` only the pages that are
/// private to the process (heap, stack, copy-on-write data), as opposed to shared, file-backed ones.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Part {
    pub region: String,
    pub pss_kb: u64,
    pub anon_kb: u64,
}

pub const REGIONS: [&str; 8] = ["code", "read-only data", "data", "libraries", "heap", "stack", "anonymous", "other"];

/// The region a mapping belongs to. `exe` is the process's own binary: its executable mapping is code, its
/// writable one data, the rest read-only data (which includes relocated data, private once the loader has
/// patched it).
pub fn classify(path: &str, perms: &str, exe: &str) -> &'static str {
    match path {
        "[heap]" => "heap",
        "[stack]" => "stack",
        "" => "anonymous",
        p if p.starts_with('[') => "other",
        p if p.trim_end_matches(" (deleted)") == exe => {
            if perms.contains('x') {
                "code"
            } else if perms.contains('w') {
                "data"
            } else {
                "read-only data"
            }
        }
        _ => "libraries",
    }
}

/// `/proc/<pid>/smaps` summed per region, in `REGIONS` order.
pub fn parse_smaps(text: &str, exe: &str) -> Vec<Part> {
    let mut parts: Vec<Part> = REGIONS.iter().map(|r| Part { region: (*r).into(), pss_kb: 0, anon_kb: 0 }).collect();
    let mut cur = "other";
    for line in text.lines() {
        let mut tok = line.split_whitespace();
        let Some(first) = tok.next() else { continue };
        if let Some(key) = first.strip_suffix(':') {
            let kb: u64 = tok.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let i = REGIONS.iter().position(|r| *r == cur).unwrap_or(REGIONS.len() - 1);
            match key {
                "Pss" => parts[i].pss_kb += kb,
                "Anonymous" => parts[i].anon_kb += kb,
                _ => {}
            }
        } else if first.contains('-') {
            let perms = tok.next().unwrap_or("");
            let path = tok.skip(3).collect::<Vec<_>>().join(" ");
            cur = classify(&path, perms, exe);
        }
    }
    parts
}

/// `None` when the process is gone or unreadable.
pub fn breakdown(pid: i32) -> Option<Vec<Part>> {
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let text = fs::read_to_string(format!("/proc/{pid}/smaps")).ok()?;
    Some(parse_smaps(&text, &exe.to_string_lossy()))
}

#[cfg(test)]
mod region_tests {
    use super::*;

    const SMAPS: &str = "\
5000-6000 r--p 00000000 08:01 7 /opt/mu
Pss:                 300 kB
Anonymous:             0 kB
6000-9000 r-xp 00001000 08:01 7 /opt/mu
Pss:                1000 kB
Anonymous:             0 kB
9000-a000 r--p 00004000 08:01 7 /opt/mu
Pss:                 100 kB
Anonymous:           100 kB
a000-b000 rw-p 00005000 08:01 7 /opt/mu
Pss:                  20 kB
Anonymous:            20 kB
1000-2000 rw-p 00000000 00:00 0 [heap]
Pss:                  60 kB
Anonymous:            60 kB
7f00-7f10 r-xp 00000000 08:01 9 /usr/lib/libc.so.6
Pss:                  40 kB
Anonymous:             0 kB
7f20-7f30 rw-p 00000000 00:00 0 
Pss:                  16 kB
Anonymous:            16 kB
7ff0-8000 rw-p 00000000 00:00 0 [stack]
Pss:                  12 kB
Anonymous:            12 kB
7ffa-7ffb r-xp 00000000 00:00 0 [vdso]
Pss:                   4 kB
Anonymous:             0 kB
";

    #[test]
    fn classifies_by_path_and_permissions() {
        assert_eq!(classify("/opt/mu", "r-xp", "/opt/mu"), "code");
        assert_eq!(classify("/opt/mu (deleted)", "r-xp", "/opt/mu"), "code");
        assert_eq!(classify("/opt/mu", "rw-p", "/opt/mu"), "data");
        assert_eq!(classify("/opt/mu", "r--p", "/opt/mu"), "read-only data");
        assert_eq!(classify("/usr/lib/libc.so.6", "r-xp", "/opt/mu"), "libraries");
        assert_eq!(classify("", "rw-p", "/opt/mu"), "anonymous");
        assert_eq!(classify("[vdso]", "r-xp", "/opt/mu"), "other");
    }

    #[test]
    fn sums_pss_and_private_pages_per_region() {
        let p = parse_smaps(SMAPS, "/opt/mu");
        let get = |r: &str| p.iter().find(|x| x.region == r).map(|x| (x.pss_kb, x.anon_kb)).unwrap();
        assert_eq!(get("code"), (1000, 0));
        assert_eq!(get("read-only data"), (400, 100));
        assert_eq!(get("data"), (20, 20));
        assert_eq!(get("libraries"), (40, 0));
        assert_eq!(get("heap"), (60, 60));
        assert_eq!(get("anonymous"), (16, 16));
        assert_eq!(get("stack"), (12, 12));
        assert_eq!(get("other"), (4, 0));
        assert_eq!(p.iter().map(|x| x.pss_kb).sum::<u64>(), 1552);
        assert_eq!(p.iter().map(|x| x.region.as_str()).collect::<Vec<_>>(), REGIONS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_breaks_down() {
        let parts = breakdown(std::process::id() as i32).unwrap();
        let get = |r: &str| parts.iter().find(|x| x.region == r).unwrap().pss_kb;
        assert!(get("code") > 0 && get("heap") + get("anonymous") > 0);
    }
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

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_is_sampled() {
        let me = std::process::id() as i32;
        let procs = sample_tree(me);
        assert_eq!(procs[0].pid, me);
        assert!(procs[0].mem.pss_anon_kb > 0 && procs[0].mem.pss_kb >= procs[0].mem.pss_anon_kb);
        let one = sample_roots(&[me]);
        let twice = sample_roots(&[me, me]);
        assert!(one.pss_kb > 0 && twice.pss_kb < one.pss_kb * 3 / 2, "a repeated root is counted once");
        assert_eq!(sample_roots(&[]), Mem::default());
    }
}
