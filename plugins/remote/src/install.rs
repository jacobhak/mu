//! `install-adapter`: writes the shipped adapters into `<state>/bin/`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const POD_ADAPTER: &str = include_str!("../adapters/mu-remote-pod");
const POD_TEMPLATE: &str = include_str!("../adapters/mu-remote-pod.yaml");

/// Installs the adapter `name` into `dir` and returns the program's path. Running it again replaces the files.
fn install(name: &str, dir: &Path) -> Result<PathBuf> {
    if name != "pod" {
        bail!("unknown adapter `{name}`; mu-remote ships `pod` (`ssh` is built in and needs no install)");
    }
    crate::fs::create_private_dir(dir).with_context(|| dir.display().to_string())?;
    let program = dir.join("mu-remote-pod");
    let template = dir.join("mu-remote-pod.yaml");
    crate::fs::write_atomic(&template, POD_TEMPLATE.as_bytes(), 0o644)
        .with_context(|| template.display().to_string())?;
    crate::fs::write_atomic(&program, POD_ADAPTER.as_bytes(), 0o755).with_context(|| program.display().to_string())?;
    Ok(program)
}

pub fn run(name: &str) -> Result<i32> {
    let program = install(name, &crate::paths::state_dir().join("bin"))?;
    println!("installed {}; use `adapter = \"{name}\"` in a [remote.NAME] section", program.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_an_executable_script_and_its_template() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::tmp().join("bin");
        let program = install("pod", &dir).unwrap();
        assert_eq!(program, dir.join("mu-remote-pod"));
        assert_eq!(std::fs::metadata(&program).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_to_string(&program).unwrap(), POD_ADAPTER);
        let template = dir.join("mu-remote-pod.yaml");
        assert_eq!(std::fs::metadata(&template).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(std::fs::read_to_string(&template).unwrap(), POD_TEMPLATE);
        std::fs::write(&program, "stale").unwrap();
        install("pod", &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&program).unwrap(), POD_ADAPTER);
        let e = install("nope", &dir).unwrap_err().to_string();
        assert!(e.contains("unknown adapter") && e.contains("`pod`"), "{e}");
    }
}
