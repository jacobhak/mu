//! The profile's `providers.toml`: reads and the edits that keep the file's comments and other entries.

use super::CustomProvider;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    #[serde(default)]
    providers: BTreeMap<String, CustomProvider>,
}

/// The profile's custom-provider definitions (`providers.toml`), read fresh on every call.
#[derive(Debug, Clone)]
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn new() -> Self {
        Self { path: mu_common::paths::profile_home().join("providers.toml") }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        let mut n = self.path.file_name().unwrap_or_default().to_os_string();
        n.push(".lock");
        self.path.with_file_name(n)
    }

    fn read_text(&self) -> Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn parse(&self, text: &str) -> Result<BTreeMap<String, CustomProvider>> {
        let doc: FileDoc = toml::from_str(text).with_context(|| {
            format!("{} is not valid custom-provider configuration; fix the file", self.path.display())
        })?;
        doc.providers
            .into_iter()
            .map(|(id, p)| {
                let p = p
                    .validated(&id)
                    .with_context(|| format!("{}: [providers.{id}] is invalid; fix the file", self.path.display()))?;
                Ok((id, p))
            })
            .collect()
    }

    /// Every definition as the file holds it now. A missing file is empty; one that cannot be read
    /// or does not validate is an error naming the file and the entry.
    pub fn snapshot(&self) -> Result<BTreeMap<String, CustomProvider>> {
        self.parse(&self.read_text()?)
    }

    pub fn get(&self, id: &str) -> Result<Option<CustomProvider>> {
        Ok(self.snapshot()?.remove(id))
    }

    /// Adds a definition, keeping the file's comments and other entries. Refuses an existing id
    /// and a file that does not parse.
    pub fn add(&self, id: &str, provider: CustomProvider) -> Result<()> {
        self.write_entry(id, provider, false)
    }

    /// Replaces a definition's entry as an edit, keeping the file's comments, the other entries and
    /// the position of this one. Refuses an id that is not there and a file that does not parse.
    pub fn set(&self, id: &str, provider: CustomProvider) -> Result<()> {
        self.write_entry(id, provider, true)
    }

    fn write_entry(&self, id: &str, provider: CustomProvider, replace: bool) -> Result<()> {
        let provider = provider.validated(id)?;
        let _lock = mu_common::fs::FileLock::exclusive(&self.lock_path())
            .with_context(|| format!("locking {}", self.lock_path().display()))?;
        let text = self.read_text()?;
        let exists = self.parse(&text)?.contains_key(id);
        match (replace, exists) {
            (false, true) => bail!("custom provider `{id}` already exists in {}", self.path.display()),
            (true, false) => bail!("custom provider `{id}` is not in {}", self.path.display()),
            _ => {}
        }
        let mut doc: toml_edit::DocumentMut =
            text.parse().with_context(|| format!("{} is not valid TOML", self.path.display()))?;
        let mut table = toml::to_string(&provider)
            .context("serializing the provider")?
            .parse::<toml_edit::DocumentMut>()
            .context("serializing the provider")?
            .as_table()
            .clone();
        clear_positions(&mut table);
        let providers = doc
            .as_table_mut()
            .entry("providers")
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_mut()
            .ok_or_else(|| anyhow!("`providers` in {} is not a table", self.path.display()))?;
        // An entry being replaced keeps its place in the file and the comment above it.
        let old = providers.remove(id);
        let mut item = toml_edit::Item::Table(table);
        if let Some(toml_edit::Item::Table(old)) = old
            && let Some(new) = item.as_table_mut()
        {
            new.set_position(old.position());
            *new.decor_mut() = old.decor().clone();
        }
        providers.insert(id, item);
        self.write(&doc.to_string())
    }

    /// Removes a definition; `false` when there was none. Other content is kept.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let _lock = mu_common::fs::FileLock::exclusive(&self.lock_path())
            .with_context(|| format!("locking {}", self.lock_path().display()))?;
        let text = self.read_text()?;
        if !self.parse(&text)?.contains_key(id) {
            return Ok(false);
        }
        let mut doc: toml_edit::DocumentMut =
            text.parse().with_context(|| format!("{} is not valid TOML", self.path.display()))?;
        let removed =
            doc.get_mut("providers").and_then(|p| p.as_table_like_mut()).is_some_and(|p| p.remove(id).is_some());
        if removed {
            self.write(&doc.to_string())?;
        }
        Ok(removed)
    }

    fn write(&self, text: &str) -> Result<()> {
        mu_common::fs::write_private(&self.path, text.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Tables built in another document carry its positions; without them they follow the file's own.
fn clear_positions(t: &mut toml_edit::Table) {
    t.set_position(None);
    for (_, item) in t.iter_mut() {
        if let Some(sub) = item.as_table_mut() {
            clear_positions(sub);
        }
    }
}
