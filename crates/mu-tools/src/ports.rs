//! The TCP ports a background job's processes listen on, so the UI can offer to open them. A job's
//! processes are its process group and everything those started (`ps` gives the parent links);
//! their listening sockets come from `/proc` on Linux and from `lsof` on macOS. Only a port that
//! `localhost` reaches counts: bound to a wildcard or a loopback address.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

/// The ports the processes of group `pgid` (and their descendants) listen on, sorted. A tool that
/// is missing or a process that cannot be inspected leaves its ports out; nothing here fails.
pub fn listening(pgid: u32) -> Vec<u16> {
    if pgid <= 1 {
        return Vec::new();
    }
    let Some(table) = run("ps", &["-A", "-o", "pid=,ppid=,pgid="]) else { return Vec::new() };
    let pids = process_set(&parse_ps(&table), pgid);
    if pids.is_empty() {
        return Vec::new();
    }
    let mut ports = if cfg!(target_os = "linux") { linux_listeners(&pids) } else { lsof_listeners(&pids) };
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    // lsof exits 1 when it finds nothing; its output is still what there is.
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `(pid, ppid, pgid)` of each row of `ps -A -o pid=,ppid=,pgid=`.
pub fn parse_ps(text: &str) -> Vec<(u32, u32, u32)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(str::parse::<u32>);
            Some((f.next()?.ok()?, f.next()?.ok()?, f.next()?.ok()?))
        })
        .collect()
}

/// Every process in group `pgid`, and every process descended from one of them.
pub fn process_set(rows: &[(u32, u32, u32)], pgid: u32) -> HashSet<u32> {
    let mut set: HashSet<u32> = rows.iter().filter(|r| r.2 == pgid).map(|r| r.0).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, ppid, _) in rows {
        children.entry(ppid).or_default().push(pid);
    }
    let mut todo: Vec<u32> = set.iter().copied().collect();
    while let Some(p) = todo.pop() {
        for &c in children.get(&p).into_iter().flatten() {
            if set.insert(c) {
                todo.push(c);
            }
        }
    }
    set
}

fn linux_listeners(pids: &HashSet<u32>) -> Vec<u16> {
    let mut inodes = HashSet::new();
    for pid in pids {
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in dir.flatten() {
            if let Some(n) = std::fs::read_link(fd.path()).ok().and_then(|l| socket_inode(&l.to_string_lossy())) {
                inodes.insert(n);
            }
        }
    }
    let mut ports = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            ports.extend(proc_net_listeners(&text).into_iter().filter(|l| inodes.contains(&l.inode)).map(|l| l.port));
        }
    }
    ports
}

/// `N` of a `socket:[N]` link target.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// A listening socket that `localhost` reaches.
#[derive(Debug, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub inode: u64,
}

/// The listening (`0A`) sockets in a `/proc/net/tcp` or `/proc/net/tcp6` listing that are bound to a
/// wildcard or loopback address.
pub fn proc_net_listeners(text: &str) -> Vec<Listener> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.get(3) != Some(&"0A") {
                return None;
            }
            let (addr, port) = f.get(1)?.split_once(':')?;
            if !proc_addr_reachable(addr) {
                return None;
            }
            Some(Listener { port: u16::from_str_radix(port, 16).ok()?, inode: f.get(9)?.parse().ok()? })
        })
        .collect()
}

/// Whether a `/proc/net/tcp*` address (hex, as the kernel prints it) is 0.0.0.0, 127.0.0.1, `::` or `::1`
/// (or `::1` / `127.0.0.1` in their IPv4-mapped form).
fn proc_addr_reachable(addr: &str) -> bool {
    matches!(
        addr.to_ascii_uppercase().as_str(),
        "00000000"
            | "0100007F"
            | "00000000000000000000000000000000"
            | "00000000000000000000000001000000"
            | "0000000000000000FFFF000000000000"
            | "0000000000000000FFFF00000100007F"
    )
}

fn lsof_listeners(pids: &HashSet<u32>) -> Vec<u16> {
    let Some(text) = run("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"]) else { return Vec::new() };
    lsof_ports(&text, pids)
}

/// The ports in `lsof -F pn` output that belong to one of `pids` and that `localhost` reaches.
pub fn lsof_ports(text: &str, pids: &HashSet<u32>) -> Vec<u16> {
    let mut ports = Vec::new();
    let mut mine = false;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            mine = p.parse().is_ok_and(|p| pids.contains(&p));
        } else if let Some(name) = line.strip_prefix('n').filter(|_| mine) {
            let name = name.trim_end_matches(" (LISTEN)");
            let Some((host, port)) = name.rsplit_once(':') else { continue };
            if matches!(host, "*" | "0.0.0.0" | "127.0.0.1" | "[::]" | "[::1]" | "localhost")
                && let Ok(p) = port.parse()
            {
                ports.push(p);
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0000000000000000 100 0 0 10 0\n\
   2: 0A00A8C0:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0000000000000000 100 0 0 10 0\n\
   3: 0100007F:C350 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 44444 1 0000000000000000 100 0 0 10 0\n";

    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 55555 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000000000000000000001000000:0BB9 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 66666 1 0000000000000000 100 0 0 10 0\n\
   2: 0000000000000000FFFF0000C0A8000A:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 77777 1 0000000000000000 100 0 0 10 0\n";

    #[test]
    fn proc_net_keeps_listening_sockets_that_localhost_reaches() {
        assert_eq!(
            proc_net_listeners(TCP),
            [Listener { port: 8080, inode: 12345 }, Listener { port: 3000, inode: 22222 }],
            "a LAN-only address and an established connection are left out"
        );
        assert_eq!(
            proc_net_listeners(TCP6),
            [Listener { port: 8080, inode: 55555 }, Listener { port: 3001, inode: 66666 }],
            "a mapped LAN address is left out"
        );
    }

    #[test]
    fn socket_links_name_their_inode() {
        assert_eq!(socket_inode("socket:[12345]"), Some(12345));
        assert_eq!(socket_inode("pipe:[12345]"), None);
        assert_eq!(socket_inode("/dev/null"), None);
    }

    #[test]
    fn a_process_set_is_the_group_and_its_descendants() {
        // 10 leads group 10; 11 is in it; 12 is a child of 11 that left the group; 13 belongs to someone else.
        let rows =
            parse_ps("   10     1    10\n   11    10    10\n   12    11    12\n   13     1    13\n   14    12    14\n");
        let set = process_set(&rows, 10);
        assert_eq!(set, HashSet::from([10, 11, 12, 14]));
        assert!(process_set(&rows, 99).is_empty());
        assert!(parse_ps("garbage\n 1 2\n").is_empty());
    }

    #[test]
    fn lsof_output_gives_the_ports_of_the_right_processes() {
        let text = "p100\nn*:3000\nn127.0.0.1:8080\nn[::1]:3001\np200\nn*:9999\np300\nn192.168.1.5:7000\nn[::]:4000\n";
        let mine = HashSet::from([100, 300]);
        assert_eq!(lsof_ports(text, &mine), [3000, 8080, 3001, 4000]);
        assert!(lsof_ports(text, &HashSet::new()).is_empty());
    }

    /// Run as a child by `a_real_listener_is_found`: listens, says where in `MU_PORTS_FILE`, and waits
    /// to be killed. The port goes through a file because the test harness captures stdout.
    #[test]
    fn listener_child() {
        let Some(file) = std::env::var_os("MU_PORTS_FILE") else { return };
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(file, l.local_addr().unwrap().port().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_real_listener_is_found() {
        use std::os::unix::process::CommandExt;
        let dir = mu_testutil::tmp();
        let file = dir.path().join("port");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ports::tests::listener_child", "--test-threads=1"])
            .env("MU_PORTS_FILE", &file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut waited = 0;
        while !file.exists() && waited < 200 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        let port: u16 = std::fs::read_to_string(&file).expect("the child never listened").trim().parse().unwrap();
        let found = listening(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(found.contains(&port), "{port} not in {found:?}");
        assert!(listening(child.id()).is_empty(), "nothing is left once it is gone");
    }
}
