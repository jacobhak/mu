//! mu-remote is built only on mu's documented contracts, never on mu's crates.

#[test]
fn uses_no_mu_crate() {
    let manifest: toml::Table = include_str!("../Cargo.toml").parse().unwrap();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(deps) = manifest.get(section).and_then(|d| d.as_table()) else { continue };
        for (name, spec) in deps {
            let is_path = spec.as_table().is_some_and(|t| t.contains_key("path"));
            assert!(
                !name.starts_with("mu-") && !name.starts_with("mu_") && !is_path,
                "mu-remote uses mu only through its documented contracts (docs/remote-contracts.md); {name} is a mu crate or a path dependency"
            );
        }
    }
}
