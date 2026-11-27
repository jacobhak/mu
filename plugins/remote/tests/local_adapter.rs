//! The reference adapter (`adapter = "local"`) passes `mu-remote check --full`: a synthetic worker is started
//! on this machine, connected to over the encrypted channel, fetched, stopped and gone.
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::process::Command;

#[test]
fn the_local_adapter_passes_the_full_check() {
    let mu = common::mu_bin();
    let mr = Path::new(env!("CARGO_BIN_EXE_mu-remote"));
    let home = common::tmp();
    let config = home.join("config.toml");
    std::fs::write(&config, "[remote.here]\nadapter = \"local\"\ntrusted = true\n").unwrap();
    let path = std::env::join_paths(
        [mr.parent().unwrap(), mu.parent().unwrap()]
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(mr)
            .args(args)
            .env("PATH", &path)
            .env("MU_REMOTE_HOME", home.join("state"))
            .env("MU_REMOTE_CONFIG", &config)
            .env("MU_REMOTE_MU", &mu)
            .env("MU_HOME", home.join("mu"))
            .env("MU_TEST_HOME", home.join("mu"))
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    let o = run(&["install-adapter", "local"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&["check", "here", "--full"]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    for step in [
        "version: ok",
        "list: ok",
        "worker start: ok",
        "worker connect: ok",
        "worker fetch: ok",
        "worker stop: ok",
        "worker gone: ok",
    ] {
        assert!(out.contains(step), "{step} is missing from:\n{out}");
    }
    assert!(out.contains("files: skipped"), "{out}");
}

#[test]
fn the_local_adapter_stops_cancels_and_messages_an_agent() {
    let mr = Path::new(env!("CARGO_BIN_EXE_mu-remote"));
    let mu = common::mu_bin();
    let home = common::home_with_key(&common::serve(vec![common::sse(common::ANSWER)]));
    let cwd = common::tmp();
    let config = home.join("remote.toml");
    std::fs::write(&config, "[remote.here]\nadapter = \"local\"\ntrusted = true\n").unwrap();
    let path = std::env::join_paths(
        [mr.parent().unwrap(), mu.parent().unwrap()]
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(mr)
            .args(args)
            .env("PATH", &path)
            .env("MU_REMOTE_HOME", home.join("state"))
            .env("MU_REMOTE_CONFIG", &config)
            .env("MU_REMOTE_MU", &mu)
            .env("MU_HOME", &home)
            .env("MU_TEST_HOME", &home)
            .env("HOME", &home)
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let o = run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    ok(&["install-adapter", "local"]);

    let o = common::mu(&home, &cwd)
        .args(["agent", "--detach", "--model", common::MODEL, "--permissions", "yolo"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let started: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let _kill = common::pid_of(&home, &id);

    let listed = |id: &str| {
        ok(&["agents", "here", "--json"])
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .any(|v| v["id"] == id && v["remote"] == "here")
    };
    assert!(listed(&id), "the agent is not listed");
    let out = ok(&["notify", "here", &id, "hello"]);
    assert!(out.contains("message to"), "{out}");
    ok(&["cancel", "here", &id]);
    let out = ok(&["stop", "here", &id]);
    assert!(out.contains("stopped"), "{out}");
    assert!(!listed(&id), "the agent is still listed after stop");
}
