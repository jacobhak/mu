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
    mu_common::paths::profile_home().join("blobs")
}

/// Whether `data` is a blob reference rather than inline base64.
pub(crate) fn is_ref(data: &str) -> bool {
    data.strip_prefix(BLOB_PREFIX)
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The hash of a blob reference, `None` for inline base64.
pub fn blob_hash(data: &str) -> Option<&str> {
    data.strip_prefix(BLOB_PREFIX).filter(|_| is_ref(data))
}

fn hash_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn write_blob(hash: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = blob_dir().join(hash);
    if !std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
        std::fs::create_dir_all(blob_dir())?;
        mu_common::fs::write_atomic(&path, bytes)?;
    }
    Ok(())
}

/// Writes `bytes` to the content-addressed blob store (idempotent: the file is named by its hash,
/// so the same image is stored once) and returns its reference. Messages keep only the reference.
fn store(bytes: &[u8]) -> std::io::Result<String> {
    let hash = hash_of(bytes);
    write_blob(&hash, bytes)?;
    Ok(format!("{BLOB_PREFIX}{hash}"))
}

/// Stores bytes that arrived under `hash` (a relayed image). Refuses bytes that do not hash to it.
pub fn store_blob(hash: &str, bytes: &[u8]) -> std::io::Result<()> {
    if hash_of(bytes) != hash {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "blob bytes do not match their hash"));
    }
    write_blob(hash, bytes)
}

/// The base64 an API request needs for `data`: inline data as is, a reference read back from the
/// blob store (mu's own, then omp's, which writes the same references). `None` if the blob is gone.
pub fn resolve(data: &str) -> Option<String> {
    let Some(hash) = blob_hash(data) else { return Some(data.to_string()) };
    let dirs = [blob_dir(), mu_common::paths::omp_agent_dir().join("blobs")];
    let bytes = dirs.iter().find_map(|d| std::fs::read(d.join(hash)).ok())?;
    Some(STANDARD.encode(bytes))
}

const BLOB_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);
const BLOB_GC_EVERY: u64 = 86_400;
/// Where blob references live: session files and their run, inbox and delivery records.
const BLOB_REF_DIRS: &[&str] = &["sessions", "run", "inbox", "deliveries"];

/// Adds every blob hash mentioned in the files under `dir` to `out`. `false` if anything could not be read.
fn collect_refs(dir: &Path, out: &mut std::collections::HashSet<String>) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return e.kind() == std::io::ErrorKind::NotFound,
    };
    let mut ok = true;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let path = entry.path();
        let Ok(ty) = entry.file_type() else { return false };
        if ty.is_dir() {
            ok &= collect_refs(&path, out);
        } else if ty.is_file() {
            let Ok(text) = std::fs::read(&path) else { return false };
            let text = String::from_utf8_lossy(&text);
            for (i, _) in text.match_indices(BLOB_PREFIX) {
                let hash = text[i + BLOB_PREFIX.len()..].chars().take(64).collect::<String>();
                if is_ref(&format!("{BLOB_PREFIX}{hash}")) {
                    out.insert(hash);
                }
            }
        }
    }
    ok
}

/// Removes blobs in `<profile>/blobs` that no file in the profile references and that are older than 30
/// days, at most once a day (stamp `<profile>/blobs.gc`). Any unreadable file under the profile skips the
/// run, so a blob is never removed on partial knowledge. Returns how many were removed.
pub fn prune_blobs(profile: &Path, now: std::time::SystemTime) -> usize {
    let secs = |t: std::time::SystemTime| t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let stamp = profile.join("blobs.gc");
    let last = std::fs::read_to_string(&stamp).ok().and_then(|s| s.trim().parse::<u64>().ok());
    if last.is_some_and(|l| secs(now).saturating_sub(l) < BLOB_GC_EVERY) {
        return 0;
    }
    let Ok(blobs) = std::fs::read_dir(profile.join("blobs")) else { return 0 };
    let mut refs = std::collections::HashSet::new();
    if !BLOB_REF_DIRS.iter().all(|d| collect_refs(&profile.join(d), &mut refs)) {
        return 0;
    }
    let mut removed = 0;
    for entry in blobs.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| is_ref(&format!("{BLOB_PREFIX}{n}"))) else { continue };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| now.duration_since(m).is_ok_and(|age| age > BLOB_MAX_AGE));
        if old && !refs.contains(name) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    let _ = mu_common::fs::write_atomic(&stamp, secs(now).to_string().as_bytes());
    removed
}

/// `prune_blobs` for the current profile.
pub fn prune_blobs_now() -> usize {
    prune_blobs(&mu_common::paths::profile_home(), std::time::SystemTime::now())
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

/// The path a pasted line names: quotes, `file://`, `\ ` and `~/` are undone; the file's type is not checked.
fn pasted_path(line: &str) -> Option<PathBuf> {
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
    let mut path = PathBuf::from(&p);
    if p.starts_with("~/") {
        path = mu_common::paths::expand_tilde(&p, Path::new(&std::env::var_os("HOME")?))?;
    }
    Some(path)
}

fn image_path(line: &str) -> Option<PathBuf> {
    let path = pasted_path(line)?;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    (path.is_absolute() && matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") && path.is_file())
        .then_some(path)
}

/// The existing regular file a paste of one line names, whatever its type.
pub fn file_in_paste(text: &str) -> Option<PathBuf> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let path = pasted_path(lines.next()?)?;
    (lines.next().is_none() && path.is_absolute() && path.is_file()).then_some(path)
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
        mu_testutil::home();
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
        let msg = crate::message::Message::User {
            content: vec![crate::message::Block::Image(img.clone())],
            timestamp: 0,
            synthetic: false,
            command: None,
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.len() < 300, "{}", line.len());
        // Inline data from older sessions passes through, and a missing blob is reported, not sent.
        assert_eq!(resolve("QUJD").as_deref(), Some("QUJD"));
        assert_eq!(resolve(&format!("{BLOB_PREFIX}{}", "0".repeat(64))), None);
        // A relayed blob is stored under its hash, and only if the bytes match.
        let hash = blob_hash(&img.data).unwrap().to_string();
        assert!(store_blob(&"1".repeat(64), &png).is_err());
        assert!(store_blob(&hash, b"other").is_err());
        store_blob(&hash, &png).unwrap();
        store_blob(&hash, &png).unwrap();
        assert_eq!(resolve(&img.data).unwrap(), STANDARD.encode(&png));
        assert_eq!(blob_hash("QUJD"), None);
        // Inline data can be moved into the store.
        let moved = externalize(Image { data: STANDARD.encode(&png), mime_type: "image/png".into() });
        assert_eq!(moved.data, img.data);
    }

    fn set_age(p: &Path, days: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn profile_with_blobs() -> (PathBuf, [PathBuf; 3]) {
        let p = std::env::temp_dir().join(format!("mu-gc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(p.join("blobs")).unwrap();
        std::fs::create_dir_all(p.join("sessions/x")).unwrap();
        let [kept, orphan, fresh] = ["a", "b", "c"].map(|c| p.join("blobs").join(c.repeat(64)));
        for (f, d) in [(&kept, 40), (&orphan, 40), (&fresh, 1)] {
            std::fs::write(f, b"x").unwrap();
            set_age(f, d);
        }
        std::fs::write(p.join("sessions/x/s.jsonl"), format!("{{\"data\":\"{BLOB_PREFIX}{}\"}}\n", "a".repeat(64)))
            .unwrap();
        (p, [kept, orphan, fresh])
    }

    #[test]
    fn prune_removes_only_old_unreferenced_blobs_once_a_day() {
        let (p, [kept, orphan, fresh]) = profile_with_blobs();
        let now = std::time::SystemTime::now();
        assert_eq!(prune_blobs(&p, now), 1);
        assert!(kept.exists() && fresh.exists() && !orphan.exists());
        // A second run within a day does nothing, even for a newly orphaned blob.
        std::fs::remove_file(p.join("sessions/x/s.jsonl")).unwrap();
        assert_eq!(prune_blobs(&p, now + std::time::Duration::from_secs(3600)), 0);
        assert!(kept.exists());
        assert_eq!(prune_blobs(&p, now + std::time::Duration::from_secs(2 * 86_400)), 1);
        assert!(!kept.exists() && fresh.exists());
        let _ = std::fs::remove_dir_all(&p);
    }

    #[cfg(unix)]
    #[test]
    fn prune_skips_when_a_session_file_cannot_be_read() {
        use std::os::unix::fs::PermissionsExt;
        let (p, [_, orphan, _]) = profile_with_blobs();
        let bad = p.join("sessions/x/bad.jsonl");
        std::fs::write(&bad, "{}").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&bad).is_ok() {
            return; // running as root: permissions do not apply
        }
        assert_eq!(prune_blobs(&p, std::time::SystemTime::now()), 0);
        assert!(orphan.exists());
        let _ = std::fs::remove_dir_all(&p);
    }
}
