//! Image attachments: type sniffing, file loading and image-path detection in pasted text.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::message::Image;

const MAX_BYTES: usize = 5 * 1024 * 1024;

fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"\xFF\xD8\xFF") {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Validates size and type, then base64-encodes. No resizing.
pub fn from_bytes(bytes: Vec<u8>) -> Result<Image, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("image too large ({:.1} MB, max 5 MB)", bytes.len() as f64 / (1024.0 * 1024.0)));
    }
    let mime = sniff(&bytes).ok_or("not a png, jpeg, gif or webp image")?;
    Ok(Image { data: STANDARD.encode(&bytes), mime_type: mime.into() })
}

pub fn load(path: &Path) -> Result<Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    from_bytes(bytes)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn image_path(line: &str) -> Option<PathBuf> {
    let mut s = line.trim();
    for q in ['\'', '"'] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            s = inner;
            break;
        }
    }
    let mut p = match s.strip_prefix("file://") {
        Some(rest) => percent_decode(rest),
        None => s.to_string(),
    };
    p = p.replace("\\ ", " ");
    if let Some(rest) = p.strip_prefix("~/") {
        p = format!("{}/{rest}", std::env::var("HOME").ok()?.trim_end_matches('/'));
    }
    let path = PathBuf::from(p);
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    (path.is_absolute() && matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") && path.is_file()).then_some(path)
}

/// The image files a paste consists of, when every non-empty line is one.
pub fn paths_in_paste(text: &str) -> Option<Vec<PathBuf>> {
    let paths: Option<Vec<PathBuf>> = text.lines().filter(|l| !l.trim().is_empty()).map(image_path).collect();
    paths.filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn sniffs_types_and_rejects_others() {
        assert_eq!(from_bytes(PNG.to_vec()).unwrap().mime_type, "image/png");
        assert_eq!(from_bytes(b"GIF89a....".to_vec()).unwrap().mime_type, "image/gif");
        assert_eq!(from_bytes(b"RIFF\x01\x02\x03\x04WEBPVP8 ".to_vec()).unwrap().mime_type, "image/webp");
        assert_eq!(from_bytes(b"hello".to_vec()).unwrap_err(), "not a png, jpeg, gif or webp image");
        let mut big = PNG.to_vec();
        big.resize(MAX_BYTES + 1, 0);
        assert!(from_bytes(big).unwrap_err().starts_with("image too large (5.0 MB"));
    }

    #[test]
    fn paste_paths_accept_quotes_file_urls_and_escapes() {
        let dir = std::env::temp_dir().join(format!("mu-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a b.PNG");
        let b = dir.join("c.jpg");
        std::fs::write(&a, PNG).unwrap();
        std::fs::write(&b, PNG).unwrap();
        let (a_s, b_s) = (a.display().to_string(), b.display().to_string());
        let paste = format!("'{a_s}'\n\nfile://{}\n{}", b_s.replace(' ', "%20"), a_s.replace(' ', "\\ "));
        assert_eq!(paths_in_paste(&paste), Some(vec![a.clone(), b.clone(), a.clone()]));
        assert_eq!(paths_in_paste(&format!("{a_s}\nnot a path")), None);
        assert_eq!(paths_in_paste("relative.png"), None);
        assert_eq!(paths_in_paste(&dir.join("missing.png").display().to_string()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
