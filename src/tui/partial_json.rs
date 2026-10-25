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

enum State {
    Normal,
    /// `key`: the string started where an object key was expected.
    Str { key: bool, escape: Option<usize>, hex: u8 },
    Literal,
}

/// Parses `s` as JSON, completing it if it is a truncated prefix. `None` when nothing usable yet.
pub fn parse(s: &str) -> Option<Value> {
    if s.trim().is_empty() {
        return None;
    }
    let b = s.as_bytes();
    let mut frames: Vec<Frame> = Vec::new();
    let mut st = State::Normal;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match &mut st {
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
                    st = State::Normal;
                    match frames.last_mut() {
                        Some(Frame::Obj { expect, .. }) if was_key => *expect = ObjExpect::Colon,
                        Some(f) => f.complete(i + 1),
                        None => {}
                    }
                }
            }
            State::Literal => {
                if matches!(c, b',' | b'}' | b']' | b':') || c.is_ascii_whitespace() {
                    st = State::Normal;
                    if let Some(f) = frames.last_mut() {
                        f.complete(i);
                    }
                    continue; // reprocess the terminator
                }
            }
            State::Normal => match c {
                b'{' => {
                    frames.push(Frame::Obj { expect: ObjExpect::Key, last_complete: i + 1 });
                }
                b'[' => {
                    frames.push(Frame::Arr { expect: ArrExpect::Value, last_complete: i + 1 });
                }
                b'}' | b']' => {
                    frames.pop();
                    if let Some(f) = frames.last_mut() {
                        f.complete(i + 1);
                    }
                }
                b'"' => {
                    let key = matches!(frames.last(), Some(Frame::Obj { expect: ObjExpect::Key, .. }));
                    st = State::Str { key, escape: None, hex: 0 };
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
                _ => st = State::Literal,
            },
        }
        i += 1;
    }

    let mut out = match &st {
        State::Str { key: false, escape, .. } => {
            let cut = escape.unwrap_or(b.len());
            format!("{}\"", &s[..cut])
        }
        State::Str { key: true, .. } | State::Literal => match frames.last() {
            Some(f) => s[..f.last()].to_string(),
            None => return None,
        },
        State::Normal => match frames.last() {
            Some(Frame::Obj { expect: ObjExpect::Colon | ObjExpect::Value, last_complete })
            | Some(Frame::Arr { expect: ArrExpect::Value, last_complete }) => s[..*last_complete].to_string(),
            _ => s.to_string(),
        },
    };
    // A trailing comma after the last complete member (`{"a":1,`) is dropped by the truncation above
    // only for states that truncate; handle the remaining `Key`/`Value` expectation after a comma here.
    if matches!(st, State::Normal) {
        if let Some(f) = frames.last() {
            let after_comma = matches!(f, Frame::Obj { expect: ObjExpect::Key, .. } | Frame::Arr { expect: ArrExpect::Value, .. });
            if after_comma {
                out.truncate(f.last());
            }
        }
    }
    for f in frames.iter().rev() {
        out.push(f.closer());
    }
    serde_json::from_str(&out).ok()
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
}
