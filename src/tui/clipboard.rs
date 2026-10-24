//! System clipboard access: text out via tmux/OSC 52 plus local helpers, images in via local helpers.

use std::io::Write;
use std::process::{Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Copies `text`. Under tmux, `load-buffer -w` forwards it to the outer terminal's clipboard (OSC 52);
/// otherwise OSC 52 goes straight to the terminal. A local clipboard tool is also tried, best effort.
pub fn copy(text: &str) -> Result<(), String> {
    let in_tmux = std::env::var("TMUX").is_ok_and(|v| !v.is_empty());
    let sent = if in_tmux { tmux_load(text) } else { osc52(text) };
    pipe_to_local(text);
    sent
}

fn tmux_load(text: &str) -> Result<(), String> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tmux load-buffer failed: {e}"))?;
    // Dropped after the write so tmux sees EOF.
    let mut stdin = child.stdin.take().ok_or("tmux load-buffer failed")?;
    let _ = stdin.write_all(text.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().map_err(|e| format!("tmux load-buffer failed: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() { "tmux load-buffer failed".into() } else { err })
}

fn osc52(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    out.write_all(format!("\x1b]52;c;{}\x07", STANDARD.encode(text)).as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

fn pipe_to_local(text: &str) {
    let mut cmd = if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
        Command::new("wl-copy")
    } else if std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard"]);
        c
    } else {
        return;
    };
    let Ok(mut child) = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    // xclip and wl-copy daemonise to serve the selection; do not wait for them.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

async fn run(bin: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { format!("{bin} not found") } else { format!("{bin}: {e}") })?;
    // A non-zero exit means the clipboard is empty or lacks the target.
    Ok(if out.status.success() { out.stdout } else { Vec::new() })
}

/// Reads an image from the local clipboard (`wl-paste` or `xclip`). `Ok(None)` when it holds none.
pub async fn read_image() -> Result<Option<crate::message::Image>, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
    let (bin, list_args, read_args): (&str, &[&str], fn(&str) -> Vec<String>) = if wayland {
        ("wl-paste", &["--list-types"], |m| vec!["--type".into(), m.into()])
    } else if x11 {
        (
            "xclip",
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            |m| ["-selection", "clipboard", "-t", m, "-o"].map(String::from).to_vec(),
        )
    } else {
        return Err("no local clipboard (DISPLAY/WAYLAND_DISPLAY unset); use the terminal's paste key or /image <path>".into());
    };
    let types = String::from_utf8_lossy(&run(bin, list_args).await?).into_owned();
    let Some(mime) = IMAGE_TYPES.iter().find(|m| types.lines().any(|l| l.trim() == **m)) else { return Ok(None) };
    let args = read_args(mime);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run(bin, &args).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    crate::image::from_bytes(bytes).map(Some)
}
