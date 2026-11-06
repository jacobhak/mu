//! Opens the authorization URL in the system browser.

use std::process::{Command, Stdio};

/// Starts the platform opener detached and returns whether it was started. The URL is one argument,
/// never run through a shell. A failure is not an error: the caller shows the URL anyway.
pub fn open_browser(url: &str) -> bool {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let spawned =
        Command::new(program).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}
