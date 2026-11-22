fn main() {
    println!("cargo:rerun-if-changed=../../upstream.toml");
    let toml = std::fs::read_to_string("../../upstream.toml").unwrap_or_default();
    let omp = toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("omp = \""))
        .and_then(|r| r.split('"').next())
        .unwrap_or("unknown");
    println!(
        "cargo:rustc-env=MU_GATEWAY_VERSION={} (omp {omp})",
        std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
    );
}
