use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Makes text safe to print: C0/C1 controls other than `\n` become a visible glyph (`\x1b` → `␛`),
/// and tabs expand to the next 4-column stop. Nothing the terminal could act on gets through.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c != '\n' && (c.is_control())) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\u{7f}' => {
                out.push('\u{2421}');
                col += 1;
            }
            c if (c as u32) < 0x20 => {
                out.push(char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'));
                col += 1;
            }
            c if c.is_control() => {
                out.push('\u{fffd}');
                col += 1;
            }
            c => {
                out.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    Cow::Owned(out)
}

/// Display width of one grapheme cluster (a combining sequence or emoji counts as one cell run).
pub fn grapheme_width(g: &str) -> usize {
    UnicodeWidthStr::width(g)
}

/// `s` cut to `width` columns, ending with `…` when cut.
pub fn truncate_text(s: &str, width: usize) -> String {
    let s = sanitize(s);
    if s.graphemes(true).map(grapheme_width).sum::<usize>() <= width {
        return s.into_owned();
    }
    let mut out = String::new();
    let mut col = 0;
    for g in s.graphemes(true) {
        let w = grapheme_width(g);
        if col + w + 1 > width {
            break;
        }
        out.push_str(g);
        col += w;
    }
    out.push('…');
    out
}
