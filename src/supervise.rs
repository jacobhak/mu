use crate::paths;
use anyhow::{Result, anyhow, bail};
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Kills the omp processes mu started.
pub struct Supervisor {
    children: Vec<Child>,
    pub gateway_url: String,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Spawns `omp <args>` on an ephemeral loopback port and returns the URL it reports.
fn spawn(args: &[&str], env: &[(&str, &str)]) -> Result<(Child, String)> {
    let mut cmd = Command::new("omp");
    cmd.args(args).envs(env.iter().copied()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0); // keep terminal Ctrl-C away from the children
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run `omp` ({e}); set MU_GATEWAY_URL to use an existing gateway"))?;
    let (tx, rx) = mpsc::channel();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    for r in [Box::new(out) as Box<dyn std::io::Read + Send>, Box::new(err)] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut seen = String::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => {
                if let Some(i) = line.find("http://") {
                    let url: String = line[i..].chars().take_while(|c| !c.is_whitespace() && *c != '"').collect();
                    return Ok((child, url));
                }
                seen.push_str(&line);
                seen.push('\n');
            }
            Err(_) => {
                let _ = child.kill();
                bail!("`omp {}` did not report a listening URL:\n{seen}", args.join(" "));
            }
        }
    }
}

/// Starts a private broker + gateway on ephemeral ports, unless MU_GATEWAY_URL is set.
pub fn ensure_gateway() -> Result<Option<Supervisor>> {
    if std::env::var_os("MU_GATEWAY_URL").is_some() {
        return Ok(None);
    }
    let mut sup = Supervisor { children: Vec::new(), gateway_url: String::new() };
    let (broker, broker_url) = spawn(&["auth-broker", "serve", "--bind=127.0.0.1:0"], &[])?;
    sup.children.push(broker);
    let _ = Command::new("omp").args(["auth-gateway", "token"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let token = std::fs::read_to_string(paths::omp_config_root().join("auth-broker.token")).unwrap_or_default();
    let (gw, gw_url) = spawn(
        &["auth-gateway", "serve", "--bind=127.0.0.1:0"],
        &[("OMP_AUTH_BROKER_URL", &broker_url), ("OMP_AUTH_BROKER_TOKEN", token.trim())],
    )?;
    sup.children.push(gw);
    sup.gateway_url = gw_url;
    Ok(Some(sup))
}
