//! System clipboard access: text out via tmux/OSC 52 plus local helpers, images in via local helpers
//! (`osascript` on macOS, `wl-paste`/`xclip` elsewhere).

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

/// Which local display server a clipboard helper can talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Display {
    #[cfg(target_os = "macos")]
    MacOs,
    Wayland,
    X11,
    None,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn detect_display(wayland: Option<&std::ffi::OsStr>, x11: Option<&std::ffi::OsStr>) -> Display {
    let set = |v: Option<&std::ffi::OsStr>| v.is_some_and(|v| !v.is_empty());
    if set(wayland) {
        Display::Wayland
    } else if set(x11) {
        Display::X11
    } else {
        Display::None
    }
}

/// Detected once: the environment does not change under a running session.
fn display() -> Display {
    static D: std::sync::OnceLock<Display> = std::sync::OnceLock::new();
    *D.get_or_init(|| {
        #[cfg(target_os = "macos")]
        return Display::MacOs;
        #[cfg(not(target_os = "macos"))]
        detect_display(std::env::var_os("WAYLAND_DISPLAY").as_deref(), std::env::var_os("DISPLAY").as_deref())
    })
}

fn pipe_to_local(text: &str) {
    let mut cmd = match display() {
        Display::Wayland => Command::new("wl-copy"),
        Display::X11 => {
            let mut c = Command::new("xclip");
            c.args(["-selection", "clipboard"]);
            c
        }
        #[cfg(target_os = "macos")]
        Display::MacOs => return,
        Display::None => return,
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
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { format!("{bin} not found") } else { format!("{bin}: {e}") }
        })?;
    // A non-zero exit means the clipboard is empty or lacks the target.
    Ok(if out.status.success() { out.stdout } else { Vec::new() })
}

type ReadArgs = fn(&str) -> Vec<String>;

/// Reads the general pasteboard's image via NSPasteboard. TIFF, which many apps put on the pasteboard
/// instead of PNG, is converted to PNG.
#[cfg(target_os = "macos")]
fn read_pasteboard_image() -> Result<Option<Vec<u8>>, String> {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeTIFF};
    use objc2_foundation::{NSDictionary, NSString};

    let pb = NSPasteboard::generalPasteboard();
    for t in ["public.png", "public.jpeg", "com.compuserve.gif", "org.webmproject.webp"] {
        if let Some(d) = pb.dataForType(&NSString::from_str(t)).filter(|d| !d.is_empty()) {
            return Ok(Some(d.to_vec()));
        }
    }
    let Some(tiff) = pb.dataForType(unsafe { NSPasteboardTypeTIFF }).filter(|d| !d.is_empty()) else {
        return Ok(None);
    };
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("clipboard image: unreadable TIFF")?;
    // SAFETY: an empty properties dictionary is valid for PNG output.
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
        .ok_or("clipboard image: PNG conversion failed")?;
    Ok(Some(png.to_vec()))
}

#[cfg(target_os = "macos")]
async fn read_image_macos() -> Result<Option<mu_base::message::Image>, String> {
    let bytes = tokio::task::spawn_blocking(read_pasteboard_image).await.map_err(|e| e.to_string())??;
    bytes.map(mu_base::image::from_bytes).transpose()
}

/// Reads an image from the local clipboard. `Ok(None)` when it holds none.
pub async fn read_image() -> Result<Option<mu_base::message::Image>, String> {
    let (bin, list_args, read_args): (&str, &[&str], ReadArgs) = match display() {
        #[cfg(target_os = "macos")]
        Display::MacOs => return read_image_macos().await,
        Display::Wayland => ("wl-paste", &["--list-types"], |m| vec!["--type".into(), m.into()]),
        Display::X11 => ("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"], |m| {
            ["-selection", "clipboard", "-t", m, "-o"].map(String::from).to_vec()
        }),
        Display::None => {
            return Err(
                "no local clipboard (DISPLAY/WAYLAND_DISPLAY unset); use the terminal's paste key or /image <path>"
                    .into(),
            );
        }
    };
    let types = String::from_utf8_lossy(&run(bin, list_args).await?).into_owned();
    let Some(mime) = IMAGE_TYPES.iter().find(|m| types.lines().any(|l| l.trim() == **m)) else { return Ok(None) };
    let args = read_args(mime);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run(bin, &args).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    mu_base::image::from_bytes(bytes).map(Some)
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn wayland_wins_and_empty_means_unset() {
        let o = |s: &'static str| Some(OsStr::new(s));
        assert_eq!(detect_display(o("wayland-0"), o(":0")), Display::Wayland);
        assert_eq!(detect_display(o(""), o(":0")), Display::X11);
        assert_eq!(detect_display(None, o("")), Display::None);
    }
}
