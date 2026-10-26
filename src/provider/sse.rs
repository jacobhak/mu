/// A frame larger than this is a protocol error, not data.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Incremental SSE frame splitter. Line endings (`\n`, `\r\n`, bare `\r`) are normalized to `\n`
/// first, so a frame ends at a blank line whatever the server sends. `data:` lines are joined by `\n`.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    /// The previous chunk ended in `\r`; a `\n` first in this one belongs to it.
    pending_cr: bool,
    /// Set when a frame outgrew `MAX_FRAME`.
    pub overflow: bool,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<(Option<String>, String)> {
        for &b in chunk {
            match b {
                b'\r' => {
                    self.buf.push(b'\n');
                    self.pending_cr = true;
                }
                b'\n' if self.pending_cr => self.pending_cr = false,
                _ => {
                    self.pending_cr = false;
                    self.buf.push(b);
                }
            }
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&frame[..pos]);
            let mut event = None;
            let mut data: Vec<&str> = Vec::new();
            for l in text.lines() {
                if let Some(e) = l.strip_prefix("event:") {
                    event = Some(e.trim().to_string());
                } else if let Some(d) = l.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d));
                }
            }
            if !data.is_empty() {
                out.push((event, data.join("\n")));
            }
        }
        if self.buf.len() > MAX_FRAME {
            self.overflow = true;
            self.buf.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_across_chunks_and_crlf() {
        let mut d = SseDecoder::default();
        let all = b"event: a\ndata: 1\ndata: 2\n\ndata: x\r\n\r\n";
        let mut got = Vec::new();
        for c in all.chunks(1) {
            got.extend(d.push(c));
        }
        assert_eq!(got, [(Some("a".into()), "1\n2".into()), (None, "x".into())]);
    }

    #[test]
    fn cr_only_mixed_and_split_endings() {
        for all in [&b"data: a\r\rdata: b\r\r"[..], b"data: a\n\ndata: b\r\n\r\n", b"data: a\r\n\ndata: b\n\r\n"] {
            let mut d = SseDecoder::default();
            let got: Vec<_> = all.chunks(1).flat_map(|c| d.push(c)).collect();
            assert_eq!(got, [(None, "a".into()), (None, "b".into())], "{all:?}");
        }
    }

    #[test]
    fn oversized_frame_is_flagged() {
        let mut d = SseDecoder::default();
        d.push(&vec![b'x'; MAX_FRAME + 1]);
        assert!(d.overflow);
    }
}
