//! `mu-config`: mu's `config.toml` as the active profile sees it, for an adapter that has to send it to a
//! machine that runs no profile. mu links no code here, so this repeats its rule: `$MU_HOME/config.toml`
//! (default `~/.mu`) with `profiles/<name>/config.toml` merged over it; tables merge key by key, any other
//! value (an array too) in the profile's file replaces the shared one.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn mu_home() -> PathBuf {
    match std::env::var_os("MU_HOME").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => crate::paths::home().join(".mu"),
    }
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// The shared file, then the profile's own over it. A missing file is empty; one that does not parse is an error.
fn merged(mu_home: &Path, profile: Option<&str>) -> Result<toml::Table> {
    let mut files = vec![mu_home.join("config.toml")];
    if let Some(p) = profile {
        files.push(mu_home.join("profiles").join(p).join("config.toml"));
    }
    let mut out = toml::Table::new();
    for f in files {
        let text = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| f.display().to_string()),
        };
        merge(&mut out, text.parse().with_context(|| f.display().to_string())?);
    }
    Ok(out)
}

pub fn run() -> Result<i32> {
    let table = merged(&mu_home(), crate::paths::profile().as_deref())?;
    print!("{}", toml::to_string(&table)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_file_is_merged_over_the_shared_one() {
        let home = crate::testutil::tmp();
        std::fs::create_dir_all(home.join("profiles/work")).unwrap();
        std::fs::write(home.join("config.toml"), "a = 1\nl = [1, 2]\n[t]\nx = 1\ny = 1\n").unwrap();
        std::fs::write(home.join("profiles/work/config.toml"), "l = [3]\n[t]\ny = 2\n[n]\nz = 1\n").unwrap();
        let want = |s: &str| s.parse::<toml::Table>().unwrap();
        assert_eq!(merged(&home, None).unwrap(), want("a = 1\nl = [1, 2]\n[t]\nx = 1\ny = 1\n"));
        assert_eq!(merged(&home, Some("work")).unwrap(), want("a = 1\nl = [3]\n[t]\nx = 1\ny = 2\n[n]\nz = 1\n"));
        assert_eq!(merged(&home, Some("other")).unwrap(), merged(&home, None).unwrap());
        std::fs::write(home.join("profiles/work/config.toml"), "= broken").unwrap();
        assert!(merged(&home, Some("work")).is_err());
    }
}
