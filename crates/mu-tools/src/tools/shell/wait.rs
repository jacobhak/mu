//! Commands that wait. While a background job runs, the model waits for it by ending its turn (the
//! job's result starts the next one), so the agent refuses these instead of running them.

#[derive(Clone, Copy, PartialEq)]
enum Sep {
    /// `;`, `&&`, `||`, a newline or the end.
    Seq,
    Pipe,
    Background,
}

/// One simple command of a command line.
struct Segment {
    words: Vec<String>,
    sep: Sep,
    /// No redirection, subshell, group or command substitution.
    plain: bool,
    /// A `$` or backtick outside single quotes.
    expands: bool,
}

impl Segment {
    fn new() -> Self {
        Self { words: Vec::new(), sep: Sep::Seq, plain: true, expands: false }
    }

    fn first(&self) -> &str {
        self.words.first().map_or("", String::as_str)
    }

    fn sequential(&self) -> bool {
        self.plain && self.sep == Sep::Seq
    }
}

/// Splits a command line into simple commands. A rough shell lexer: quotes, escapes, comments and
/// the control operators; anything more is marked not `plain`.
fn segments(cmd: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut seg = Segment::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut prev = ' ';
    let mut chars = cmd.chars().peekable();
    let end_word = |seg: &mut Segment, word: &mut String, in_word: &mut bool| {
        if *in_word {
            seg.words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    let end_seg = |out: &mut Vec<Segment>, seg: &mut Segment, sep: Sep| {
        let mut done = std::mem::replace(seg, Segment::new());
        if !done.words.is_empty() {
            done.sep = sep;
            out.push(done);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for d in chars.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    word.push(d);
                }
            }
            '"' => {
                in_word = true;
                while let Some(d) = chars.next() {
                    match d {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        '$' | '`' => {
                            seg.expands = true;
                            word.push(d);
                        }
                        _ => word.push(d),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some('\n') | None => {}
                    Some(d) => word.push(d),
                }
            }
            '#' if !in_word => while chars.next_if(|&d| d != '\n').is_some() {},
            ' ' | '\t' => end_word(&mut seg, &mut word, &mut in_word),
            ';' | '\n' => {
                end_word(&mut seg, &mut word, &mut in_word);
                end_seg(&mut out, &mut seg, Sep::Seq);
            }
            '&' if matches!(prev, '>' | '<') || chars.peek() == Some(&'>') => {
                seg.plain = false;
                in_word = true;
                word.push(c);
            }
            '&' | '|' => {
                end_word(&mut seg, &mut word, &mut in_word);
                let sep = if chars.next_if_eq(&c).is_some() {
                    Sep::Seq
                } else if c == '&' {
                    Sep::Background
                } else {
                    Sep::Pipe
                };
                end_seg(&mut out, &mut seg, sep);
            }
            '$' | '`' => {
                seg.expands = true;
                in_word = true;
                word.push(c);
            }
            '<' | '>' | '(' | ')' | '{' | '}' => {
                seg.plain = false;
                in_word = true;
                word.push(c);
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
        prev = c;
    }
    end_word(&mut seg, &mut word, &mut in_word);
    end_seg(&mut out, &mut seg, Sep::Seq);
    out
}

/// Seconds in `sleep`'s operands (`90`, `1.5`, `2m`, `1h`, `1d`, summed as GNU `sleep` does).
fn sleep_secs(args: &[String]) -> Option<f64> {
    if args.is_empty() {
        return None;
    }
    args.iter().try_fold(0.0, |sum, a| {
        let (num, unit) = match a.strip_suffix(['s', 'm', 'h', 'd']) {
            Some(n) => (n, &a[n.len()..]),
            None => (a.as_str(), "s"),
        };
        let n: f64 = num.parse().ok().filter(|n: &f64| *n >= 0.0)?;
        let mult = match unit {
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            _ => 1.0,
        };
        Some(sum + n * mult)
    })
}

/// Does nothing but pass time or print a fixed text.
fn filler(s: &Segment) -> bool {
    match s.first() {
        "sleep" | "true" | ":" | "wait" | "cd" => true,
        "echo" | "printf" => !s.expands,
        _ => false,
    }
}

/// A `while`, `until` or `for` loop whose body sleeps: a poll.
fn polls(segs: &[Segment]) -> bool {
    segs.iter().any(|s| matches!(s.first(), "while" | "until" | "for"))
        && segs
            .iter()
            .any(|s| s.first() == "sleep" || (s.first() == "do" && s.words.get(1).is_some_and(|w| w == "sleep")))
}

/// The command only waits: it is made of sleeps, fixed echoes and no-ops, starts (after any `cd`)
/// with a sleep of at least `min_sleep_secs` (0 turns that check off), or is a loop that sleeps.
pub fn waits(command: &str, min_sleep_secs: f64) -> bool {
    let segs = segments(command);
    let start = segs.iter().take_while(|s| s.sequential() && s.first() == "cd").count();
    let body = &segs[start..];
    let Some(first) = body.first() else { return false };
    if min_sleep_secs > 0.0
        && first.sequential()
        && first.first() == "sleep"
        && sleep_secs(&first.words[1..]).is_some_and(|s| s >= min_sleep_secs)
    {
        return true;
    }
    polls(body) || body.iter().all(|s| s.sequential() && filler(s))
}

#[cfg(test)]
mod tests {
    use super::waits;

    #[test]
    fn commands_that_only_wait() {
        for c in [
            "sleep 60; echo",
            "sleep 1; echo waiting",
            "echo waiting",
            "sleep 3",
            "cd /home/jacob/code/private/mu && sleep 60; echo",
            "sleep 2; echo \"still waiting\"",
            "true",
            "sleep 0.5 && printf 'tick\\n'",
            "wait",
        ] {
            assert!(waits(c, 10.0), "{c}");
        }
    }

    #[test]
    fn a_leading_sleep_at_or_over_the_threshold_waits() {
        for c in [
            "sleep 120; ls /tmp/scan/out/before 2>&1 | head",
            "sleep 240; grep -h \"^test result\" /tmp/par-*.log",
            "cd ~/x && sleep 10; tail -3 /tmp/build.log",
            "sleep 1m; gh run list",
            "sleep 5 5; ls",
        ] {
            assert!(waits(c, 10.0), "{c}");
        }
        assert!(!waits("sleep 120; ls out", 0.0), "0 turns the threshold off");
    }

    #[test]
    fn loops_that_sleep_are_polls() {
        for c in [
            "while ! test -f /tmp/done; do sleep 5; done",
            "until grep -q finished log; do\n  sleep 2\ndone; cat log",
            "for i in $(seq 40); do sleep 3; grep -q x f && break; done",
        ] {
            assert!(waits(c, 10.0), "{c}");
        }
    }

    #[test]
    fn real_work_is_not_a_wait() {
        for c in [
            "sleep 5; tmux capture-pane -pt m4r | tail -2",
            "tmux send-keys -t t Enter; sleep 3; tmux capture-pane -p -t t",
            "cargo test 2>&1 | tail -25",
            "echo $PATH",
            "echo \"$HOME\"",
            "echo done > /tmp/flag",
            "sleep 30 &",
            "sleep 14 | script -qfc mu /dev/null",
            "cd /tmp",
            "git status; echo ---; git log -1",
            "printf '%s' `date`",
            "for f in *.rs; do wc -l $f; done",
            "git commit -qm 'wait; sleep 60'",
            "",
        ] {
            assert!(!waits(c, 10.0), "{c}");
        }
    }
}
