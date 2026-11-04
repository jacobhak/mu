/// Unified diff with 3 lines of context; empty when the texts are equal.
pub fn unified(old: &str, new: &str) -> String {
    const CTX: usize = 3;
    let (ot, nt) = (pi_diff::line_tokens_str(old), pi_diff::line_tokens_str(new));
    let runs = pi_diff::line_runs_str(old, new);
    // (marker, line, old line no, new line no)
    let mut rows: Vec<(char, &str, usize, usize)> = Vec::new();
    let (mut o, mut n) = (0usize, 0usize);
    for r in &runs {
        for _ in 0..r.count {
            if r.added {
                rows.push(('+', nt[n], o, n));
                n += 1;
            } else if r.removed {
                rows.push(('-', ot[o], o, n));
                o += 1;
            } else {
                rows.push((' ', ot[o], o, n));
                o += 1;
                n += 1;
            }
        }
    }
    let changed: Vec<usize> = rows.iter().enumerate().filter(|(_, r)| r.0 != ' ').map(|(i, _)| i).collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CTX);
        let mut j = i;
        while j + 1 < changed.len() && changed[j + 1] <= changed[j] + 2 * CTX + 1 {
            j += 1;
        }
        let end = (changed[j] + CTX + 1).min(rows.len());
        let seg = &rows[start..end];
        let (os, ns) = (seg[0].2 + 1, seg[0].3 + 1);
        let oc = seg.iter().filter(|r| r.0 != '+').count();
        let nc = seg.iter().filter(|r| r.0 != '-').count();
        out.push_str(&format!("@@ -{os},{oc} +{ns},{nc} @@\n"));
        for (m, line, ..) in seg {
            out.push(*m);
            out.push_str(line.strip_suffix('\n').unwrap_or(line));
            out.push('\n');
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_with_context() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "X\n");
        let d = unified(&old, &new);
        assert_eq!(d, "@@ -7,7 +7,7 @@\n l7\n l8\n l9\n-l10\n+X\n l11\n l12\n l13\n");
        assert_eq!(unified(&old, &old), "");
        let d = unified("", "a\nb\n");
        assert_eq!(d, "@@ -1,0 +1,2 @@\n+a\n+b\n");
    }
}
