fn pin(toml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = \"");
    toml.lines()
        .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .and_then(|r| r.split('"').next())
        .map(str::to_string)
}

fn main() {
    println!("cargo:rerun-if-changed=../../upstream.toml");
    let toml = std::fs::read_to_string("../../upstream.toml").unwrap_or_default();
    let omp = pin(&toml, "omp").unwrap_or_else(|| "unknown".into());
    let pi = pin(&toml, "pi").map(|v| format!(", pi {v}")).unwrap_or_default();
    println!(
        "cargo:rustc-env=MU_GATEWAY_VERSION={} (omp {omp}{pi})",
        std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
    );
}
