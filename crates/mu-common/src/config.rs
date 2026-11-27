//! mu's `config.toml` as one profile sees it: `$MU_HOME/config.toml` with the profile's own
//! `<profile home>/config.toml` merged over it. Tables merge key by key; any other value (and an array)
//! in the profile file replaces the shared one.

use crate::paths::profile_subdir;
use std::path::{Path, PathBuf};

/// The files that make up the config, shared first, the profile's overlay last (none without a profile).
pub fn files(mu_home: &Path, profile: Option<&str>) -> Vec<PathBuf> {
    let base = mu_home.join("config.toml");
    let own = profile_subdir(mu_home.to_path_buf(), profile).join("config.toml");
    if own == base { vec![base] } else { vec![base, own] }
}

/// Merges `over` into `base`.
pub fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Reads and merges `files`. A missing file is empty; one that cannot be read or parsed is skipped and
/// named in the second value.
pub fn load(files: &[PathBuf]) -> (toml::Table, Vec<String>) {
    let mut out = toml::Table::new();
    let mut warn = Vec::new();
    for p in files {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn.push(format!("{}: {e}; ignored", p.display()));
                continue;
            }
        };
        match text.parse::<toml::Table>() {
            Ok(t) => merge(&mut out, t),
            Err(e) => warn.push(format!("{}: {e}; ignored", p.display())),
        }
    }
    (out, warn)
}

/// `load` for the config of `profile` under `mu_home`.
pub fn load_in(mu_home: &Path, profile: Option<&str>) -> (toml::Table, Vec<String>) {
    load(&files(mu_home, profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> toml::Table {
        s.parse().unwrap()
    }

    #[test]
    fn tables_merge_and_other_values_are_replaced() {
        let mut base = table("a = 1\nl = [1, 2]\n[t]\nx = 1\ny = 1\n[t.u]\nk = 1\n");
        merge(&mut base, table("l = [3]\n[t]\ny = 2\n[t.u]\nj = 2\n[n]\nz = 1\n"));
        assert_eq!(base, table("a = 1\nl = [3]\n[t]\nx = 1\ny = 2\n[t.u]\nk = 1\nj = 2\n[n]\nz = 1\n"));
    }

    #[test]
    fn a_profile_adds_its_own_file_and_none_adds_nothing() {
        let home = Path::new("/m");
        assert_eq!(files(home, None), [PathBuf::from("/m/config.toml")]);
        assert_eq!(files(home, Some("../x")), [PathBuf::from("/m/config.toml")], "an unsafe name is no profile");
        assert_eq!(
            files(home, Some("work")),
            [PathBuf::from("/m/config.toml"), PathBuf::from("/m/profiles/work/config.toml")]
        );
    }

    #[test]
    fn a_bad_file_is_skipped_with_its_path() {
        let d = std::env::temp_dir().join(format!("mu-common-config-{}", std::process::id()));
        std::fs::create_dir_all(d.join("profiles/p")).unwrap();
        std::fs::write(d.join("config.toml"), "[a]\nx = 1\n").unwrap();
        std::fs::write(d.join("profiles/p/config.toml"), "= = =").unwrap();
        let (t, w) = load_in(&d, Some("p"));
        assert_eq!(t, table("[a]\nx = 1\n"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("profiles/p/config.toml") && w[0].ends_with("ignored"), "{w:?}");
        std::fs::write(d.join("profiles/p/config.toml"), "[a]\nx = 2\n").unwrap();
        assert_eq!(load_in(&d, Some("p")).0, table("[a]\nx = 2\n"));
        assert_eq!(load_in(&d, None).0, table("[a]\nx = 1\n"));
        std::fs::remove_dir_all(d).unwrap();
    }
}
