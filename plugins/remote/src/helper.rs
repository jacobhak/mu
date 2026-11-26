//! `mu-remote helper NAME ID VERB ARGS…`: what `MU_ATTACH_HELPER` runs for mu (`mu --attach` started by
//! `client::run`). mu does not know ssh or kubectl; this maps its four verbs (`forward PORT`, `unforward PORT
//! LOCAL`, `read PATH`, `write PATH`) onto the remote's adapter. Exit 64 means "not supported here".

use crate::adapter::{Adapter, Unsupported};
use crate::config::Config;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

const NOT_SUPPORTED: i32 = 64;
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);
const FORWARD_TRIES: usize = 3;

pub fn run(name: &str, id: &str, verb: &str, args: &[String]) -> Result<i32> {
    if !matches!(verb, "forward" | "unforward" | "read" | "write") {
        eprintln!("mu-remote: helper: unknown verb {verb}");
        return Ok(NOT_SUPPORTED);
    }
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(Some(name))?;
    let adapter = Adapter::from_config(name, remote)?;
    if !adapter.trusted() {
        bail!("remote {name} is not trusted");
    }
    match dispatch(&adapter, id, verb, args) {
        Err(e) => match e.downcast_ref::<Unsupported>() {
            Some(u) => {
                eprintln!("mu-remote: {u}");
                Ok(NOT_SUPPORTED)
            }
            None => Err(e),
        },
        Ok(()) => Ok(0),
    }
}

fn dispatch(adapter: &Adapter, id: &str, verb: &str, args: &[String]) -> Result<()> {
    match (verb, args) {
        ("forward", [port]) => {
            let local = forward(adapter, id, parse_port(port)?)?;
            println!("{{\"local\":{local}}}");
            Ok(())
        }
        ("unforward", [port, local]) => {
            adapter.unforward(id, parse_port(port)?, parse_port(local)?, Some(FORWARD_TIMEOUT))
        }
        ("read", [path]) => {
            let mut out = std::io::stdout();
            adapter.read(id, path, &mut out)?;
            out.flush().context("cannot write to stdout")
        }
        ("write", [path]) => adapter.write(id, path, &mut std::io::stdin()),
        _ => bail!("helper {verb}: wrong arguments"),
    }
}

fn parse_port(s: &str) -> Result<u16> {
    s.parse::<u16>().ok().filter(|p| *p != 0).with_context(|| format!("{s:?} is not a port"))
}

/// A free local port: `port` itself when free (so the URL means the same on both machines), else any.
fn pick_local(port: u16, prefer_same: bool) -> Result<u16> {
    if prefer_same && TcpListener::bind(("127.0.0.1", port)).is_ok() {
        return Ok(port);
    }
    let l = TcpListener::bind(("127.0.0.1", 0)).context("cannot find a free local port")?;
    Ok(l.local_addr()?.port())
}

fn address_in_use(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_ascii_lowercase();
    m.contains("in use") || m.contains("cannot listen") || m.contains("bind")
}

/// Forwards `port` of the agent's machine to a free local port and returns that port. The port is free when
/// picked; another process may take it before the adapter binds it, so a bind failure is retried.
fn forward(adapter: &Adapter, id: &str, port: u16) -> Result<u16> {
    let mut last = None;
    for attempt in 0..FORWARD_TRIES {
        let local = pick_local(port, attempt == 0)?;
        match adapter.forward(id, port, local, Some(FORWARD_TIMEOUT)) {
            Ok(()) => return Ok(local),
            Err(e) if address_in_use(&e) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("forward failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_numbers_above_zero() {
        assert_eq!(parse_port("3000").unwrap(), 3000);
        assert!(parse_port("0").is_err() && parse_port("x").is_err() && parse_port("70000").is_err());
    }

    #[test]
    fn the_same_port_is_taken_when_free_else_another() {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = l.local_addr().unwrap().port();
        assert_ne!(pick_local(busy, true).unwrap(), busy);
        drop(l);
        // Another test's process may take the freed port first, so try a few.
        let taken = (0..5).any(|_| pick_local(busy, true).unwrap() == busy);
        assert!(taken, "the freed port was never picked");
    }

    #[test]
    fn bind_failures_are_recognised() {
        assert!(address_in_use(&anyhow::anyhow!("bind [127.0.0.1]:3000: Address already in use")));
        assert!(!address_in_use(&anyhow::anyhow!("Permission denied")));
    }
}
