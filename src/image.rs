//! Image attachments: type sniffing, file loading and image-path detection in pasted text.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::message::Image;

const MAX_BYTES: usize = 5 * 1024 * 1024;

/// MIME type of a png, jpeg, gif or webp, from its leading bytes.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
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

/// `data` of an image whose bytes live in a blob file, in omp's form: `blob:sha256:<hex>`.
const BLOB_PREFIX: &str = "blob:sha256:";

fn blob_dir() -> PathBuf {
    mu_common::paths::mu_home().join("blobs")
}

/// Whether `data` is a blob reference rather than inline base64.
pub fn is_ref(data: &str) -> bool {
    data.strip_prefix(BLOB_PREFIX)
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Writes `bytes` to the content-addressed blob store (idempotent: the file is named by its hash,
/// so the same image is stored once) and returns its reference. Messages keep only the reference.
fn store(bytes: &[u8]) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    let path = blob_dir().join(&hash);
    if !std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
        std::fs::create_dir_all(blob_dir())?;
        mu_common::fs::write_atomic(&path, bytes)?;
    }
    Ok(format!("{BLOB_PREFIX}{hash}"))
}

/// The base64 an API request needs for `data`: inline data as is, a reference read back from the
/// blob store (mu's own, then omp's, which writes the same references). `None` if the blob is gone.
pub fn resolve(data: &str) -> Option<String> {
    let Some(hash) = data.strip_prefix(BLOB_PREFIX).filter(|_| is_ref(data)) else { return Some(data.to_string()) };
    let dirs = [blob_dir(), mu_common::paths::omp_agent_dir().join("blobs")];
    let bytes = dirs.iter().find_map(|d| std::fs::read(d.join(hash)).ok())?;
    Some(STANDARD.encode(bytes))
}

/// Moves an image's inline base64 into the blob store, so what stays in memory (and in the session
/// file) is a short reference. An image that cannot be stored stays inline: attaching never fails for it.
pub fn externalize(img: Image) -> Image {
    if is_ref(&img.data) {
        return img;
    }
    match STANDARD.decode(&img.data).ok().and_then(|b| store(&b).ok()) {
        Some(data) => Image { data, ..img },
        None => img,
    }
}

/// Validates size and type, then stores the bytes. No resizing.
pub fn from_bytes(bytes: Vec<u8>) -> Result<Image, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("image too large ({:.1} MB, max 5 MB)", bytes.len() as f64 / (1024.0 * 1024.0)));
    }
    let mime = sniff(&bytes).ok_or("not a png, jpeg, gif or webp image")?;
    let data = store(&bytes).unwrap_or_else(|_| STANDARD.encode(&bytes));
    Ok(Image { data, mime_type: mime.into() })
}

pub fn load(path: &Path) -> Result<Image, String> {
    // Refuse before reading: a huge file must not be pulled into memory just to be rejected.
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len > MAX_BYTES as u64 {
        return Err(format!("image too large ({:.1} MB, max 5 MB)", len as f64 / (1024.0 * 1024.0)));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    from_bytes(bytes)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
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
    (path.is_absolute() && matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") && path.is_file())
        .then_some(path)
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

    #[test]
    fn oversized_files_are_refused_by_size_alone() {
        let f = std::env::temp_dir().join(format!("mu-big-{}.png", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&f).unwrap();
        file.set_len(MAX_BYTES as u64 + 1).unwrap(); // sparse: nothing is written
        let e = load(&f).unwrap_err();
        assert!(e.contains("too large"), "{e}");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn images_are_kept_as_references_and_read_back_for_requests() {
        crate::testutil::home();
        let mut png = PNG.to_vec();
        png.extend(std::iter::repeat_n(7u8, 200_000));
        let img = from_bytes(png.clone()).unwrap();
        assert!(is_ref(&img.data), "{}", img.data);
        assert!(img.data.len() < 100, "the message holds a reference, not 200 KB of base64");
        // The same bytes are stored once.
        assert_eq!(from_bytes(png.clone()).unwrap().data, img.data);
        // Requests read the bytes back.
        assert_eq!(resolve(&img.data).unwrap(), STANDARD.encode(&png));
        // A session file written with it stays small and still parses.
        let msg =
            crate::message::Message::User { content: vec![crate::message::Block::Image(img.clone())], timestamp: 0 };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.len() < 300, "{}", line.len());
        // Inline data from older sessions passes through, and a missing blob is reported, not sent.
        assert_eq!(resolve("QUJD").as_deref(), Some("QUJD"));
        assert_eq!(resolve(&format!("{BLOB_PREFIX}{}", "0".repeat(64))), None);
        // Inline data can be moved into the store.
        let moved = externalize(Image { data: STANDARD.encode(&png), mime_type: "image/png".into() });
        assert_eq!(moved.data, img.data);
    }
}
