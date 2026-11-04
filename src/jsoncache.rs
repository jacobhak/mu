//! A small on-disk cache of a JSON document, shared by the model list and the price table.
//! Reads tolerate a missing or corrupt file; writes are atomic and never store an empty document.

use std::path::PathBuf;

pub struct JsonCache {
    path: PathBuf,
}

impl JsonCache {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The parsed contents, or `None` if the file is missing, unreadable or `parse` rejects it.
    pub fn load<T>(&self, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
        parse(&std::fs::read_to_string(&self.path).ok()?)
    }

    /// Writes `text` atomically (temp name per process). Returns `false` when `text` is blank
    /// or the write failed: an empty answer must never replace good cached data.
    pub fn save(&self, text: &str) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        mu_common::fs::write_atomic(&self.path, text.as_bytes()).is_ok()
    }

    /// As `save`, for data tied to an account: the file is written 0600 and its directory 0700.
    pub fn save_private(&self, text: &str) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        mu_common::fs::write_private(&self.path, text.as_bytes()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_text_is_not_saved_and_corrupt_files_read_as_none() {
        let p = std::env::temp_dir().join(format!("mu-jc-{}/c.json", uuid::Uuid::new_v4()));
        let c = JsonCache::new(p.clone());
        assert!(!c.save("  "));
        assert!(c.load(|s| Some(s.len())).is_none());
        assert!(c.save("[1]"));
        assert_eq!(c.load(|s| Some(s.len())), Some(3));
        assert!(c.load(|s| serde_json::from_str::<Vec<String>>(s).ok()).is_none());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
