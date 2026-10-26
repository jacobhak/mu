//! Completes a truncated JSON prefix so streaming tool arguments can be previewed.
use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
enum ObjExpect {
    Key,
    Colon,
    Value,
    Next,
}

#[derive(Clone, Copy, PartialEq)]
enum ArrExpect {
    Value,
    Next,
}

enum Frame {
    Obj { expect: ObjExpect, last_complete: usize },
    Arr { expect: ArrExpect, last_complete: usize },
}

impl Frame {
    fn complete(&mut self, at: usize) {
        match self {
            Frame::Obj { expect, last_complete } => {
                *expect = ObjExpect::Next;
                *last_complete = at;
            }
            Frame::Arr { expect, last_complete } => {
                *expect = ArrExpect::Next;
                *last_complete = at;
            }
        }
    }

    fn last(&self) -> usize {
        match self {
            Frame::Obj { last_complete, .. } | Frame::Arr { last_complete, .. } => *last_complete,
        }
    }

    fn closer(&self) -> char {
        match self {
            Frame::Obj { .. } => '}',
            Frame::Arr { .. } => ']',
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Normal,
    /// `key`: the string started where an object key was expected.
    Str {
        key: bool,
        escape: Option<usize>,
        hex: u8,
    },
    Literal,
}

/// A JSON document arriving in pieces. `push` scans only the new bytes (the scanner's state is
/// kept between calls); `value` completes the prefix and parses it.
#[derive(Default)]
pub struct Partial {
    buf: String,
    frames: Vec<Frame>,
    st: State,
    /// Bytes of `buf` already scanned.
    i: usize,
}

/// Parses `s` as JSON, completing it if it is a truncated prefix. `None` when nothing usable yet.
#[cfg(test)]
pub fn parse(s: &str) -> Option<Value> {
    let mut p = Partial::default();
    p.push(s);
    p.value()
}

impl Partial {
    pub fn push(&mut self, delta: &str) {
        self.buf.push_str(delta);
        self.scan();
    }

    fn scan(&mut self) {
        let b = self.buf.as_bytes();
        let (frames, st) = (&mut self.frames, &mut self.st);
        let mut i = self.i;
        while i < b.len() {
            let c = b[i];
            match st {
                State::Str { key, escape, hex } => {
                    if *hex > 0 {
                        if c.is_ascii_hexdigit() {
                            *hex -= 1;
                            if *hex == 0 {
                                *escape = None;
                            }
                        } else {
                            // malformed escape; let serde reject it
                            *hex = 0;
                            *escape = None;
                        }
                    } else if escape.is_some() {
                        if c == b'u' {
                            *hex = 4;
                        } else {
                            *escape = None;
                        }
                    } else if c == b'\\' {
                        *escape = Some(i);
                    } else if c == b'"' {
                        let was_key = *key;
                        *st = State::Normal;
                        match frames.last_mut() {
                            Some(Frame::Obj { expect, .. }) if was_key => *expect = ObjExpect::Colon,
                            Some(f) => f.complete(i + 1),
                            None => {}
                        }
                    }
                }
                State::Literal => {
                    if matches!(c, b',' | b'}' | b']' | b':') || c.is_ascii_whitespace() {
                        *st = State::Normal;
                        if let Some(f) = frames.last_mut() {
                            f.complete(i);
                        }
                        continue; // reprocess the terminator
                    }
                }
                State::Normal => match c {
                    b'{' => frames.push(Frame::Obj { expect: ObjExpect::Key, last_complete: i + 1 }),
                    b'[' => frames.push(Frame::Arr { expect: ArrExpect::Value, last_complete: i + 1 }),
                    b'}' | b']' => {
                        frames.pop();
                        if let Some(f) = frames.last_mut() {
                            f.complete(i + 1);
                        }
                    }
                    b'"' => {
                        let key = matches!(frames.last(), Some(Frame::Obj { expect: ObjExpect::Key, .. }));
                        *st = State::Str { key, escape: None, hex: 0 };
                    }
                    b',' => match frames.last_mut() {
                        Some(Frame::Obj { expect, .. }) => *expect = ObjExpect::Key,
                        Some(Frame::Arr { expect, .. }) => *expect = ArrExpect::Value,
                        None => {}
                    },
                    b':' => {
                        if let Some(Frame::Obj { expect, .. }) = frames.last_mut() {
                            *expect = ObjExpect::Value;
                        }
                    }
                    c if c.is_ascii_whitespace() => {}
                    _ => *st = State::Literal,
                },
            }
            i += 1;
        }
        self.i = i;
    }

    /// Where the document is cut before closers are appended: the end of the last complete member
    /// of the innermost container, whenever the text ends in something unfinished (a key, a
    /// literal, a colon, or a comma awaiting its next member). `None` when no cut is needed.
    fn cut(&self) -> Option<Option<usize>> {
        match &self.st {
            State::Str { key: true, .. } | State::Literal => Some(self.frames.last().map(Frame::last)),
            State::Str { key: false, .. } => None,
            State::Normal => match self.frames.last() {
                Some(
                    f @ (Frame::Obj { expect: ObjExpect::Colon | ObjExpect::Value | ObjExpect::Key, .. }
                    | Frame::Arr { expect: ArrExpect::Value, .. }),
                ) => Some(Some(f.last())),
                _ => None,
            },
        }
    }

    /// The document so far, completed. `None` when nothing is usable yet.
    pub fn value(&self) -> Option<Value> {
        if self.buf.trim().is_empty() {
            return None;
        }
        let s = &self.buf;
        let mut out = match (&self.st, self.cut()) {
            // An open string value is kept (cut before a dangling escape) and closed.
            (State::Str { key: false, escape, .. }, _) => format!("{}\"", &s[..escape.unwrap_or(s.len())]),
            (_, Some(Some(at))) => s[..at].to_string(),
            (_, Some(None)) => return None,
            (_, None) => s.to_string(),
        };
        // `{"a":1,` and `{"a":` end at the same member boundary; trailing whitespace goes too.
        if out.ends_with(char::is_whitespace) {
            out.truncate(out.trim_end().len());
        }
        for f in self.frames.iter().rev() {
            out.push(f.closer());
        }
        serde_json::from_str(&out).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> Option<Value> {
        parse(s)
    }

    #[test]
    fn completes_prefixes() {
        assert_eq!(p(r#"{"pa"#), Some(json!({})));
        assert_eq!(p(r#"{"path":"src/ma"#), Some(json!({"path":"src/ma"})));
        assert_eq!(p(r#"{"command":"echo \"#), Some(json!({"command":"echo "})));
        assert_eq!(p(r#"{"a":"x\u00"#), Some(json!({"a":"x"})));
        assert_eq!(p(r#"{"a":1,"b":"#), Some(json!({"a":1})));
        assert_eq!(p(r#"{"a":1,"#), Some(json!({"a":1})));
        assert_eq!(p(r#"{"a":tr"#), Some(json!({})));
        assert_eq!(p(r#"{"a":12"#), Some(json!({})));
        assert_eq!(p(r#"{"tasks":[{"name":"x","task":"do"#), Some(json!({"tasks":[{"name":"x","task":"do"}]})));
        assert_eq!(p(r#"{"a":["#), Some(json!({"a":[]})));
        assert_eq!(p(""), None);
    }

    #[test]
    fn complete_round_trips() {
        let s = r#"{"a":[1,2,{"b":"c\n"}],"d":null,"e":true}"#;
        assert_eq!(p(s), serde_json::from_str(s).ok());
    }

    #[test]
    fn incremental_scanning_matches_a_fresh_parse_at_every_prefix() {
        let doc = r#"{"a":[1,2,{"b":"c\n\u00e9x"}],"d":null,"e":true,"f":"tail","g":{"h":[]}}"#;
        let mut inc = Partial::default();
        for (n, ch) in doc.char_indices() {
            inc.push(&doc[n..n + ch.len_utf8()]);
            assert_eq!(inc.value(), parse(&doc[..n + ch.len_utf8()]), "prefix {n}");
        }
        assert_eq!(inc.value(), serde_json::from_str(doc).ok());
    }
}
