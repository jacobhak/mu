//! `mu-remote helper`: the four verbs `MU_ATTACH_HELPER` gives mu, over a fake adapter that reaches a "remote"
//! directory on this machine. No mu is needed.
#![allow(clippy::unwrap_used)]

mod common;

use common::tmp;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Far {
    dir: PathBuf,
}

impl Far {
    fn new(contract: u32, trusted: bool) -> Far {
        let dir = tmp();
        let script = format!(
            r#"#!/bin/sh
echo "$MU_REMOTE_NAME|$MU_REMOTE_TRUSTED|$*" >> '{dir}/log'
case "$1" in
  version) echo '{{"contract":{contract}}}' ;;
  read) cat -- "$3" ;;
  write) mkdir -p "$(dirname -- "$3")" && [ ! -e "$3" ] && cat > "$3" || {{ echo "cannot create $3" >&2; exit 1; }} ;;
  forward | unforward) ;;
  *) echo "unknown verb $1" >&2; exit 3 ;;
esac
"#,
            dir = dir.display(),
        );
        let adapter = dir.join("adapter");
        std::fs::write(&adapter, script).unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!("[remote.far]\nadapter = '{}'\ntrusted = {trusted}\n", adapter.display());
        std::fs::write(dir.join("config.toml"), config).unwrap();
        Far { dir }
    }

    fn helper(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mu-remote"))
            .args(["helper", "far", "abc123"])
            .args(args)
            .env("MU_REMOTE_CONFIG", self.dir.join("config.toml"))
            .env("MU_REMOTE_HOME", self.dir.join("state"))
            .env("HOME", &self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin).unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn a_file_written_through_the_helper_reads_back_and_is_never_replaced() {
    let far = Far::new(2, true);
    let path = far.dir.join("remote home/uploads/a.txt");
    let path = path.to_str().unwrap();
    let o = far.helper(&["write", path], b"hello");
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
    let o = far.helper(&["read", path], b"");
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "hello");
    let o = far.helper(&["write", path], b"again");
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("cannot create"), "{}", text(&o.stderr));
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
}

#[test]
fn forward_prints_the_local_port_and_asks_the_adapter_for_it() {
    let far = Far::new(2, true);
    let o = far.helper(&["forward", "3000"], b"");
    assert!(o.status.success(), "{}", text(&o.stderr));
    let reply: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let local = reply["local"].as_u64().expect("a local port");
    assert!(local > 0 && local < 65536, "{reply}");
    assert!(far.log().contains(&format!("far|1|forward abc123 3000 {local}")), "{:?}", far.log());
    let o = far.helper(&["unforward", "3000", &local.to_string()], b"");
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(far.log().contains(&format!("far|1|unforward abc123 3000 {local}")), "{:?}", far.log());
}

#[test]
fn a_contract_one_adapter_answers_every_verb_with_64() {
    let far = Far::new(1, true);
    let path = far.dir.join("x");
    let path = path.to_str().unwrap();
    for args in [vec!["forward", "3000"], vec!["unforward", "3000", "4001"], vec!["read", path], vec!["write", path]] {
        let o = far.helper(&args, b"data");
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", text(&o.stderr));
        assert_eq!(text(&o.stderr).trim().lines().count(), 1, "{args:?}: {}", text(&o.stderr));
        assert!(o.stdout.is_empty());
    }
    assert!(!far.dir.join("x").exists());
}

#[test]
fn an_untrusted_remote_gets_no_helper_and_an_unknown_verb_is_64() {
    let far = Far::new(2, false);
    let o = far.helper(&["read", "/etc/hostname"], b"");
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("remote far is not trusted"), "{}", text(&o.stderr));
    assert!(far.log().is_empty(), "the adapter was never called: {:?}", far.log());
    let o = far.helper(&["frobnicate"], b"");
    assert_eq!(o.status.code(), Some(64));
}
