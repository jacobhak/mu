/// Incremental SSE frame splitter: frames end at a blank line; `data:` lines are joined by `\n`.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<(Option<String>, String)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((pos, sep)) = find_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..pos + sep).collect();
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
        out
    }
}

fn find_end(b: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\n' && b[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if b[i] == b'\r' && b[i + 1] == b'\n' && b.get(i + 2) == Some(&b'\r') && b.get(i + 3) == Some(&b'\n') {
            return Some((i, 4));
        }
        i += 1;
    }
    None
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
}
