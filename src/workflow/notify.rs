//! `mu notify <session|id> <text>`: the CLI sender. The queue, the wake-up and the message layout
//! live in `mu_session::inbox`.
use mu_session::inbox::{MAX_TEXT, send};
use std::io::Read;
use std::path::Path;

pub fn run(arg: &Path, from: Option<&str>, text: &str, cwd: &Path) -> anyhow::Result<i32> {
    let text = if text == "-" {
        let mut s = String::new();
        if let Err(e) = std::io::stdin().take(MAX_TEXT as u64 + 1).read_to_string(&mut s) {
            eprintln!("mu notify: cannot read stdin: {e}");
            return Ok(1);
        }
        s
    } else {
        text.to_string()
    };
    match send(arg, from, &text, cwd) {
        Ok(line) => {
            println!("{line}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("mu notify: {e}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_session::inbox::has_pending;
    use mu_session::session;
    use mu_testutil::home;

    #[test]
    fn run_refuses_empty_and_oversized_text() {
        home();
        let d = std::env::temp_dir();
        assert_eq!(run(Path::new("no-such-session-zzz"), None, "x", &d).unwrap(), 1);
        let s = session::SessionFile::create(Path::new("/opt/notify-run"), None).unwrap();
        assert_eq!(run(&s.path, None, "  ", &d).unwrap(), 1);
        assert_eq!(run(&s.path, None, &"x".repeat(MAX_TEXT + 1), &d).unwrap(), 1);
        assert!(!has_pending(&session::id_of(&s.path)));
    }
}
