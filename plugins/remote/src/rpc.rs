//! Lines of JSON from an async reader, one `Value` each (the framing the encrypted channel rides on).

use serde_json::Value;

/// Longest line kept in memory. A longer one is dropped and reported as `Line::Bad`.
pub const MAX_LINE: usize = 64 * 1024 * 1024;

/// Reads newline-delimited JSON from any async reader. Lines are read as bytes, so one invalid UTF-8
/// line is reported and skipped instead of ending the stream.
pub struct Framed<R> {
    reader: R,
    buf: Vec<u8>,
}

/// One line of input.
pub enum Line {
    Value(Value),
    /// A line that is not valid UTF-8 or not valid JSON; the stream goes on.
    Bad(String),
}

impl<R: tokio::io::AsyncBufRead + Unpin> Framed<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, buf: Vec::new() }
    }

    /// The next non-blank line, or `None` at end of input or on a read error. A line over `MAX_LINE`
    /// is consumed to its end and reported as `Line::Bad`, so the stream goes on.
    pub async fn next_line(&mut self) -> Option<Line> {
        use tokio::io::AsyncBufReadExt;
        loop {
            self.buf.clear();
            let mut dropped = false;
            let mut saw_bytes = false;
            loop {
                let chunk = self.reader.fill_buf().await.ok()?;
                if chunk.is_empty() {
                    break;
                }
                saw_bytes = true;
                let (end, used) = match chunk.iter().position(|b| *b == b'\n') {
                    Some(i) => (true, i + 1),
                    None => (false, chunk.len()),
                };
                if !dropped {
                    if self.buf.len() + used > MAX_LINE {
                        dropped = true;
                        self.buf = Vec::new();
                    } else {
                        self.buf.extend_from_slice(&chunk[..used]);
                    }
                }
                self.reader.consume(used);
                if end {
                    break;
                }
            }
            if !saw_bytes {
                return None;
            }
            if dropped {
                return Some(Line::Bad(format!("line longer than {} MiB dropped", MAX_LINE >> 20)));
            }
            if self.buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Some(match serde_json::from_slice::<Value>(&self.buf) {
                Ok(v) => Line::Value(v),
                Err(e) => Line::Bad(e.to_string()),
            });
        }
    }
}
