//! Slash-command candidates and fuzzy ranking for the composer menu.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub desc: String,
    pub needs_arg: bool,
    pub args: Vec<String>,
}

/// Match quality of `cand` for `query` (higher is better); `None` when it does not match.
pub fn score(query: &str, cand: &str) -> Option<u32> {
    let q = query.to_lowercase();
    let c = cand.to_lowercase();
    if q.is_empty() {
        return Some(1);
    }
    let len = c.chars().count() as u32;
    if c.starts_with(&q) {
        return Some(3000 - len.min(999));
    }
    if c.split([':', '-', '_']).any(|seg| seg.starts_with(&q)) {
        return Some(2000 - len.min(999));
    }
    let mut chars = c.chars().enumerate();
    let mut gaps = 0u32;
    let mut last: Option<usize> = None;
    for qc in q.chars() {
        let (i, _) = chars.find(|(_, ch)| *ch == qc)?;
        if let Some(l) = last {
            gaps += (i - l - 1) as u32;
        }
        last = Some(i);
    }
    Some(1000u32.saturating_sub(gaps).max(1))
}

/// Indices of matching entries, best first.
pub fn rank(entries: &[Entry], query: &str, usage: &HashMap<String, u32>) -> Vec<usize> {
    let mut hits: Vec<(usize, u32)> =
        entries.iter().enumerate().filter_map(|(i, e)| score(query, &e.name).map(|s| (i, s))).collect();
    let used = |i: usize| usage.get(&entries[i].name).copied().unwrap_or(0);
    hits.sort_by(|&(a, sa), &(b, sb)| {
        (sb / 1000)
            .cmp(&(sa / 1000))
            .then(used(b).cmp(&used(a)))
            .then(sb.cmp(&sa))
            .then(entries[a].name.cmp(&entries[b].name))
    });
    hits.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str) -> Entry {
        Entry { name: name.into(), desc: String::new(), needs_arg: false, args: vec![] }
    }

    fn names(es: &[Entry], idx: Vec<usize>) -> Vec<&str> {
        idx.into_iter().map(|i| es[i].name.as_str()).collect()
    }

    #[test]
    fn ranking() {
        let es: Vec<Entry> = ["model", "switch", "skill:modal", "compact"].map(e).to_vec();
        let none = HashMap::new();
        assert_eq!(names(&es, rank(&es, "mo", &none)), ["model", "skill:modal"]);
        assert_eq!(names(&es, rank(&es, "cmp", &none)), ["compact"]);
        assert_eq!(rank(&es, "", &none).len(), 4);
        assert!(rank(&es, "zzz", &none).is_empty());
    }

    #[test]
    fn usage_breaks_ties_in_bucket() {
        let es: Vec<Entry> = ["plan", "plugins"].map(e).to_vec();
        let mut u = HashMap::new();
        assert_eq!(names(&es, rank(&es, "pl", &u)), ["plan", "plugins"]);
        u.insert("plugins".to_string(), 3);
        assert_eq!(names(&es, rank(&es, "pl", &u)), ["plugins", "plan"]);
    }
}
