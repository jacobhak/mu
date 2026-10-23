pub fn private_bytes() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/smaps_rollup") else { return 0 };
    s.lines()
        .find_map(|l| l.strip_prefix("Pss_Anon:"))
        .and_then(|r| r.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

pub fn tok(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

pub fn report(content_bytes: u64) -> (f64, f64, f64) {
    let private = private_bytes();
    (mb(private), mb(content_bytes), mb(private.saturating_sub(content_bytes)))
}
