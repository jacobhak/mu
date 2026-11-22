//! Customizable key bindings. Every key the UI reacts to is a [`Bind`]: a context, an action name
//! and default chords. `[keys.<context>]` in config.toml replaces an action's chords (see
//! docs/keys.md). Code asks `BIND.hit(&key)`; it never matches a `KeyCode` for a bound action.
//!
//! A context is a screen or widget (`chat`, `editor`, `jobs`, ...); each lives in its own file
//! and declares its binds with [`binds!`]. Binds in different contexts may share a chord, since
//! only one context reads a key at a time; `chat`, `editor` and `app` all see the composer's keys,
//! so a rebinding that makes two of them collide is warned about.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use crossterm::event::{KeyEvent, KeyModifiers};
use mu_base::config::MuKeys;

/// One bindable action. Declared with [`binds!`], never built by hand.
pub struct Bind {
    pub ctx: &'static str,
    pub name: &'static str,
    /// Chord syntax: `ctrl+t`, `alt+enter`, `shift+tab`, `f5`, `up`, `?` (see docs/keys.md).
    pub keys: &'static [&'static str],
}

/// Declares a context's binds as statics, plus `ALL` listing them. Syntax:
/// `NAME = "context", "action", ["chord", ...];`
macro_rules! binds {
    ($( $(#[$m:meta])* $id:ident = $ctx:literal, $name:literal, [$($k:literal),* $(,)?]; )*) => {
        $( $(#[$m])* pub static $id: $crate::keymap::Bind =
            $crate::keymap::Bind { ctx: $ctx, name: $name, keys: &[$($k),*] }; )*
        pub(super) static ALL: &[&$crate::keymap::Bind] = &[$(&$id),*];
    };
}
mod app;
mod chat;
mod chord;
mod editor;
mod jobs;
mod misc;
mod reply;
mod review;
mod select;

pub use chord::Chord;

pub use app::*;
pub use chat::*;
pub use editor::*;
pub use jobs::*;
pub use misc::*;
pub use reply::*;
pub use review::*;
pub use select::*;

fn all() -> impl Iterator<Item = &'static Bind> {
    [app::ALL, chat::ALL, editor::ALL, jobs::ALL, misc::ALL, reply::ALL, review::ALL, select::ALL]
        .into_iter()
        .flat_map(|l| l.iter().copied())
}

impl Bind {
    /// Whether `k` is one of this action's chords. The caller has already dropped key releases.
    pub fn hit(&self, k: &KeyEvent) -> bool {
        current().hit(self, k)
    }

    /// The chords now bound, for hints.
    pub fn chords(&self) -> Vec<Chord> {
        current().chords_of(self)
    }

    /// The first chord as shown to the user (`Ctrl+T`), or `unbound`.
    pub fn label(&self) -> String {
        self.chords().first().map_or_else(|| "unbound".to_string(), Chord::label)
    }
}

/// Resolved chords per (context, action).
pub struct Keymap {
    map: HashMap<(&'static str, &'static str), Vec<Chord>>,
}

impl Keymap {
    fn defaults() -> Self {
        Self { map: all().map(|b| ((b.ctx, b.name), default_chords(b))).collect() }
    }

    fn chords_of(&self, b: &Bind) -> Vec<Chord> {
        self.map.get(&(b.ctx, b.name)).cloned().unwrap_or_else(|| default_chords(b))
    }

    fn hit(&self, b: &Bind, k: &KeyEvent) -> bool {
        let c = Chord::of_event(k);
        match self.map.get(&(b.ctx, b.name)) {
            Some(v) => v.contains(&c),
            None => default_chords(b).contains(&c),
        }
    }
}

fn default_chords(b: &Bind) -> Vec<Chord> {
    b.keys.iter().filter_map(|s| Chord::parse(s).ok()).collect()
}

static MAP: OnceLock<Arc<Keymap>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_MAP: std::cell::RefCell<Option<Arc<Keymap>>> = const { std::cell::RefCell::new(None) };
}

fn current() -> Arc<Keymap> {
    #[cfg(test)]
    if let Some(m) = TEST_MAP.with(|t| t.borrow().clone()) {
        return m;
    }
    MAP.get_or_init(|| Arc::new(Keymap::defaults())).clone()
}

/// Installs the configured bindings; returns warnings (an unknown action, a chord that does not
/// parse, two actions on one chord). A bad entry is skipped and keeps its default.
pub fn init(cfg: &MuKeys) -> Vec<String> {
    let (km, warnings) = build(cfg);
    if MAP.set(Arc::new(km)).is_err() {
        let mut w = warnings;
        w.push("keys: they were read before they were configured, so the configured keys are ignored".into());
        return w;
    }
    warnings
}

fn build(cfg: &MuKeys) -> (Keymap, Vec<String>) {
    let mut km = Keymap::defaults();
    let mut warnings = Vec::new();
    let mut overridden: Vec<&'static Bind> = Vec::new();
    for (ctx, table) in &cfg.0 {
        for (name, spec) in table {
            let Some(bind) = all().find(|b| b.ctx == ctx && b.name == name) else {
                warnings.push(format!("config.toml: [keys.{ctx}] {name}: no such action; ignored"));
                continue;
            };
            let mut chords = Vec::new();
            let mut ok = true;
            for s in spec.chords() {
                if s.eq_ignore_ascii_case("none") {
                    continue;
                }
                match Chord::parse(s) {
                    Ok(c) => chords.push(c),
                    Err(e) => {
                        warnings.push(format!("config.toml: [keys.{ctx}] {name}: {e}; keeping the default"));
                        ok = false;
                    }
                }
            }
            if ok {
                km.map.insert((bind.ctx, bind.name), chords);
                overridden.push(bind);
            }
        }
    }
    warnings.extend(conflicts(&km, &overridden));
    (km, warnings)
}

/// Contexts whose binds all see the same key: the composer's.
fn group(ctx: &str) -> &str {
    match ctx {
        "app" | "chat" | "editor" => "composer",
        c => c,
    }
}

/// Binds that now share a chord with another in their group, where the user moved at least one.
fn conflicts(km: &Keymap, overridden: &[&'static Bind]) -> Vec<String> {
    let mut by_chord: BTreeMap<(String, String), Vec<&'static Bind>> = BTreeMap::new();
    for b in all() {
        for c in km.chords_of(b) {
            by_chord.entry((group(b.ctx).to_string(), c.label())).or_default().push(b);
        }
    }
    let mut out = Vec::new();
    for ((_, chord), binds) in by_chord {
        if binds.len() > 1 && binds.iter().any(|b| overridden.iter().any(|o| std::ptr::eq(*o, *b))) {
            let names: Vec<String> = binds.iter().map(|b| format!("{}.{}", b.ctx, b.name)).collect();
            out.push(format!("config.toml: [keys] {chord} is bound to {}; the first one wins", names.join(", ")));
        }
    }
    out
}

/// Runs `f` with these bindings in place on this thread (tests only). Each entry is
/// `(context, action, chords)`.
#[cfg(test)]
pub fn with_keys<R>(entries: &[(&str, &str, &[&str])], f: impl FnOnce() -> R) -> R {
    let mut cfg = MuKeys::default();
    for (ctx, name, chords) in entries {
        cfg.0.entry((*ctx).to_string()).or_default().insert(
            (*name).to_string(),
            mu_base::config::KeySpec::Many(chords.iter().map(|s| (*s).to_string()).collect()),
        );
    }
    let (km, warnings) = build(&cfg);
    assert!(warnings.is_empty(), "{warnings:?}");
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_MAP.with(|t| *t.borrow_mut() = None);
        }
    }
    TEST_MAP.with(|t| *t.borrow_mut() = Some(Arc::new(km)));
    let _reset = Reset;
    f()
}

/// `KeyModifiers` that matter to a chord.
const CHORD_MODS: KeyModifiers =
    KeyModifiers::CONTROL.union(KeyModifiers::ALT).union(KeyModifiers::SHIFT).union(KeyModifiers::SUPER);

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use mu_testutil::key_mods;

    #[test]
    fn every_default_parses_and_every_bind_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in all() {
            assert!(seen.insert((b.ctx, b.name)), "{}.{} is declared twice", b.ctx, b.name);
            for k in b.keys {
                Chord::parse(k).unwrap_or_else(|e| panic!("{}.{} default {k:?}: {e}", b.ctx, b.name));
            }
        }
    }

    #[test]
    fn every_bind_is_documented() {
        let docs = include_str!("../../../../docs/keys.md");
        for b in all() {
            assert!(docs.contains(&format!("`{}`", b.name)), "docs/keys.md does not mention {}.{}", b.ctx, b.name);
        }
    }

    #[test]
    fn a_rebinding_replaces_the_defaults() {
        let ctrl_t = key_mods(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let f5 = key_mods(KeyCode::F(5), KeyModifiers::NONE);
        assert!(toggle_thinking_hit(&ctrl_t));
        with_keys(&[("chat", "toggle_thinking", &["f5"])], || {
            assert!(!toggle_thinking_hit(&ctrl_t));
            assert!(toggle_thinking_hit(&f5));
        });
        assert!(toggle_thinking_hit(&ctrl_t), "the override ends with the closure");
    }

    fn toggle_thinking_hit(k: &KeyEvent) -> bool {
        TOGGLE_THINKING.hit(k)
    }

    #[test]
    fn bad_config_warns_and_keeps_defaults() {
        let mut cfg = MuKeys::default();
        let t = cfg.0.entry("chat".into()).or_default();
        t.insert("nope".into(), mu_base::config::KeySpec::One("ctrl+x".into()));
        t.insert("reply".into(), mu_base::config::KeySpec::One("ctrl+bogus".into()));
        t.insert("jobs".into(), mu_base::config::KeySpec::One("ctrl+a".into()));
        let (km, w) = build(&cfg);
        assert!(w.iter().any(|w| w.contains("no such action")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("ctrl+bogus") || w.contains("bogus")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("Ctrl+A")), "ctrl+a is the editor's home: {w:?}");
        let ctrl_r = key_mods(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(km.hit(&REPLY, &ctrl_r), "a bad chord keeps the default");
    }

    #[test]
    fn none_unbinds() {
        let ctrl_o = key_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
        with_keys(&[("chat", "toggle_expanded", &[])], || assert!(!TOGGLE_EXPANDED.hit(&ctrl_o)));
    }
}
