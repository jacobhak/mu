//! Views are functions from the theme, the data, their session and whether they have focus to an element.
//! They hold no state: the shell owns focus, scrolling is GPUI's per-element state (give a scrollable an `id`).
use crate::data::{Data, Session, SessionId};
use crate::theme::{MONO, SANS, Theme};
use gpui::{Div, Styled as _, div, px};

pub mod agents;
pub mod composer;
pub mod diff;
pub mod jobs;
pub mod markdown;
pub mod palette;
pub mod request;
pub mod review;
pub mod session;
pub mod transcript;

pub struct ViewCtx<'a> {
    pub t: &'a Theme,
    pub data: &'a Data,
    /// The session this view shows (a session tile), or follows (a review).
    pub session: SessionId,
    /// The view's tile (or bar) has focus: it takes the keyboard.
    pub focused: bool,
    /// The tile is wide enough for the table form of the agent list.
    pub wide: bool,
    /// A review pinned to its session.
    pub pinned: bool,
}

impl ViewCtx<'_> {
    pub fn s(&self) -> &Session {
        self.data.session(self.session)
    }
}

/// A box in interface type: Plex Sans at the UI size, in the palette's text colour.
pub fn ui(t: &Theme) -> Div {
    div().font_family(SANS).text_size(px(t.ty.ui)).text_color(t.text)
}

/// A box in code type: the mono face, for code, tool arguments, diffs and paths. The composer is interface type.
pub fn mono(t: &Theme) -> Div {
    div().font_family(MONO).text_size(px(t.ty.code)).text_color(t.text)
}

/// A box in prose type: Plex Sans at the prose size.
pub fn prose(t: &Theme) -> Div {
    div().font_family(SANS).text_size(px(t.ty.prose)).text_color(t.text)
}
