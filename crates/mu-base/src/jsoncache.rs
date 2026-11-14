//! A small on-disk cache of a JSON document, shared by the model list and the price table.
//! Reads tolerate a missing or corrupt file; writes are atomic and never store an empty document.

use std::path::PathBuf;
use std::time::Duration;

/// A cache is refreshed when it is older than this.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// A failed fetch is retried after this, not after `MAX_AGE`.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3600);

/// Whether a cache last filled at `last_ok` (Unix ms) is due a refresh: never filled, older than
/// `MAX_AGE`, or, when the latest attempt (`failed_at`) failed, `RETRY_AFTER_FAILURE` old.
pub fn due(failed_at: Option<i64>, last_ok: Option<i64>) -> bool {
    let old = |at: i64, max: Duration| {
        let age = crate::message::now_ms().saturating_sub(at);
        age < 0 || age as u128 > max.as_millis()
    };
    match (failed_at, last_ok) {
        (Some(failed), ok) if ok.is_none_or(|t| failed > t) => old(failed, RETRY_AFTER_FAILURE),
        (_, Some(t)) => old(t, MAX_AGE),
        (_, None) => true,
    }
}

fn modified_ms(path: &std::path::Path) -> Option<i64> {
    let t = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

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

    fn failed_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".failed");
        p.into()
    }

    /// For a cache with no stamps of its own: whether the file is missing, over `MAX_AGE` old, or
    /// its last refresh failed less than `RETRY_AFTER_FAILURE` ago (see `mark_failed`).
    pub fn due(&self) -> bool {
        due(modified_ms(&self.failed_path()), modified_ms(&self.path))
    }

    /// Records a failed refresh as the mtime of a marker file beside the cache.
    pub fn mark_failed(&self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(self.failed_path(), b"");
    }

    pub fn clear_failed(&self) {
        let _ = std::fs::remove_file(self.failed_path());
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
    fn a_stale_check_follows_the_daily_and_hourly_rules() {
        assert!(due(None, None));
        let now = crate::message::now_ms();
        assert!(!due(None, Some(now)));
        assert!(due(None, Some(now - 25 * 3_600_000)));
        assert!(!due(Some(now), Some(now - 3_600_000)), "a fresh failure waits an hour");
        assert!(due(Some(now - 2 * 3_600_000), Some(now - 3 * 3_600_000)));
    }

    #[test]
    fn a_file_cache_is_due_until_filled_and_backs_off_after_a_failure() {
        let p = std::env::temp_dir().join(format!("mu-jc-{}/c.json", uuid::Uuid::new_v4()));
        let c = JsonCache::new(p.clone());
        assert!(c.due());
        c.mark_failed();
        assert!(!c.due(), "a failure with no cache waits an hour");
        assert!(c.save("[1]"));
        c.clear_failed();
        assert!(!c.due());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

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
