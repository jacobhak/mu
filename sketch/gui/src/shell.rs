//! The window: tabs of tiles, the two bars, the status bar, focus, zoom, and the overlays (palettes, the
//! floating panel). Keys are GPUI actions bound in `bind_keys` with the defaults of
//! `docs/design/gui-wireframes.md` section 6.
use crate::data::{self, AgentState, Data, SessionId, sid};
use crate::theme::{Theme, metrics, palette_names};
use crate::views::{ViewCtx, agents, jobs, palette, review, session, ui};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, KeyBinding, Render, Window, actions, div, px, transparent_black,
};

actions!(
    sketch,
    [
        AgentPalette,
        FullPalette,
        JumpRequest,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        GoTab1,
        GoTab2,
        GoTab3,
        GoTab4,
        GoTab5,
        GoTab6,
        GoTab7,
        GoTab8,
        GoTab9,
        RenameTab,
        Split,
        SplitRight,
        SplitDown,
        CloseTile,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        SwapLeft,
        SwapRight,
        SwapUp,
        SwapDown,
        Narrower,
        Wider,
        Shorter,
        Taller,
        Zoom,
        ToggleLeftBar,
        ToggleRightBar,
        OpenView,
        Pin,
        SaveLayout,
        SwitchLayout,
        CyclePalette,
        ToggleScale,
        Quit,
        CloseOverlay,
        PaletteUp,
        PaletteDown,
        PaletteRun,
        OpenInTile,
    ]
);

/// Key bindings: frequent actions on a platform modifier, the rest after the leader `ctrl-x`.
pub fn bind_keys(cx: &mut App) {
    let w = Some("window");
    let mut keys = vec![
        KeyBinding::new("ctrl-x a", JumpRequest, None),
        KeyBinding::new("ctrl-x q", Quit, None),
        KeyBinding::new("ctrl-x c", CyclePalette, None),
        KeyBinding::new("ctrl-x f", ToggleScale, None),
        KeyBinding::new("ctrl-x h", FocusLeft, w),
        KeyBinding::new("ctrl-x j", FocusDown, w),
        KeyBinding::new("ctrl-x k", FocusUp, w),
        KeyBinding::new("ctrl-x l", FocusRight, w),
        KeyBinding::new("ctrl-x shift-h", SwapLeft, w),
        KeyBinding::new("ctrl-x shift-j", SwapDown, w),
        KeyBinding::new("ctrl-x shift-k", SwapUp, w),
        KeyBinding::new("ctrl-x shift-l", SwapRight, w),
        KeyBinding::new("ctrl-x enter", Split, w),
        KeyBinding::new("ctrl-x v", SplitRight, w),
        KeyBinding::new("ctrl-x s", SplitDown, w),
        KeyBinding::new("ctrl-x x", CloseTile, w),
        KeyBinding::new("ctrl-x z", Zoom, w),
        KeyBinding::new("ctrl-x <", Narrower, w),
        KeyBinding::new("ctrl-x >", Wider, w),
        KeyBinding::new("ctrl-x -", Shorter, w),
        KeyBinding::new("ctrl-x +", Taller, w),
        KeyBinding::new("ctrl-x shift-=", Taller, w),
        KeyBinding::new("ctrl-x [", ToggleLeftBar, w),
        KeyBinding::new("ctrl-x ]", ToggleRightBar, w),
        KeyBinding::new("ctrl-x o", OpenView, w),
        KeyBinding::new("ctrl-x p", Pin, w),
        KeyBinding::new("ctrl-x t", NewTab, w),
        KeyBinding::new("ctrl-x ,", RenameTab, w),
        KeyBinding::new("ctrl-x w", SaveLayout, w),
        KeyBinding::new("ctrl-x '", SwitchLayout, w),
        KeyBinding::new("ctrl-x 1", GoTab1, w),
        KeyBinding::new("ctrl-x 2", GoTab2, w),
        KeyBinding::new("ctrl-x 3", GoTab3, w),
        KeyBinding::new("ctrl-x 4", GoTab4, w),
        KeyBinding::new("ctrl-x 5", GoTab5, w),
        KeyBinding::new("ctrl-x 6", GoTab6, w),
        KeyBinding::new("ctrl-x 7", GoTab7, w),
        KeyBinding::new("ctrl-x 8", GoTab8, w),
        KeyBinding::new("ctrl-x 9", GoTab9, w),
        KeyBinding::new("escape", CloseOverlay, Some("palette")),
        KeyBinding::new("up", PaletteUp, Some("palette")),
        KeyBinding::new("down", PaletteDown, Some("palette")),
        KeyBinding::new("ctrl-p", PaletteUp, Some("palette")),
        KeyBinding::new("ctrl-n", PaletteDown, Some("palette")),
        KeyBinding::new("enter", PaletteRun, Some("palette")),
        KeyBinding::new("o", OpenInTile, Some("floating")),
    ];
    #[cfg(target_os = "macos")]
    keys.extend([
        KeyBinding::new("cmd-p", AgentPalette, None),
        KeyBinding::new("cmd-k", FullPalette, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-t", NewTab, w),
        KeyBinding::new("cmd-1", GoTab1, w),
        KeyBinding::new("cmd-2", GoTab2, w),
        KeyBinding::new("cmd-3", GoTab3, w),
        KeyBinding::new("cmd-4", GoTab4, w),
        KeyBinding::new("cmd-5", GoTab5, w),
        KeyBinding::new("cmd-6", GoTab6, w),
        KeyBinding::new("cmd-7", GoTab7, w),
        KeyBinding::new("cmd-8", GoTab8, w),
        KeyBinding::new("cmd-9", GoTab9, w),
        KeyBinding::new("cmd-shift-]", NextTab, w),
        KeyBinding::new("cmd-shift-[", PrevTab, w),
        KeyBinding::new("cmd-alt-left", FocusLeft, w),
        KeyBinding::new("cmd-alt-down", FocusDown, w),
        KeyBinding::new("cmd-alt-up", FocusUp, w),
        KeyBinding::new("cmd-alt-right", FocusRight, w),
        KeyBinding::new("cmd-w", CloseTile, w),
        KeyBinding::new("cmd-b", ToggleLeftBar, w),
    ]);
    // Ctrl+Shift+K is both "full palette" and "focus up" in the wireframes' Linux column. The sketch gives it to
    // the palette; focus up stays on the leader (ctrl-x k). See FINDINGS.md.
    #[cfg(not(target_os = "macos"))]
    keys.extend([
        KeyBinding::new("ctrl-shift-p", AgentPalette, None),
        KeyBinding::new("ctrl-shift-k", FullPalette, None),
        KeyBinding::new("ctrl-shift-t", NewTab, w),
        KeyBinding::new("ctrl-shift-1", GoTab1, w),
        KeyBinding::new("ctrl-shift-2", GoTab2, w),
        KeyBinding::new("ctrl-shift-3", GoTab3, w),
        KeyBinding::new("ctrl-shift-4", GoTab4, w),
        KeyBinding::new("ctrl-shift-5", GoTab5, w),
        KeyBinding::new("ctrl-shift-6", GoTab6, w),
        KeyBinding::new("ctrl-shift-7", GoTab7, w),
        KeyBinding::new("ctrl-shift-8", GoTab8, w),
        KeyBinding::new("ctrl-shift-9", GoTab9, w),
        KeyBinding::new("ctrl-pagedown", NextTab, w),
        KeyBinding::new("ctrl-pageup", PrevTab, w),
        KeyBinding::new("ctrl-shift-h", FocusLeft, w),
        KeyBinding::new("ctrl-shift-j", FocusDown, w),
        KeyBinding::new("ctrl-shift-l", FocusRight, w),
        KeyBinding::new("ctrl-shift-w", CloseTile, w),
        KeyBinding::new("ctrl-shift-b", ToggleLeftBar, w),
    ]);
    cx.bind_keys(keys);
}

// ---------------------------------------------------------------- tiling

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Axis {
    /// Side by side.
    Row,
    /// Stacked.
    Col,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct R {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

const EPS: f32 = 0.001;

const HOST_OS: gpui::OperatingSystem =
    if cfg!(target_os = "macos") { gpui::OperatingSystem::Mac } else { gpui::OperatingSystem::Linux };

#[derive(Clone, Debug)]
enum Node {
    Leaf(usize),
    Split { axis: Axis, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    fn contains(&self, id: usize) -> bool {
        match self {
            Node::Leaf(i) => *i == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn depth_of(&self, id: usize, d: usize) -> Option<usize> {
        match self {
            Node::Leaf(i) => (*i == id).then_some(d),
            Node::Split { a, b, .. } => a.depth_of(id, d + 1).or_else(|| b.depth_of(id, d + 1)),
        }
    }

    fn first_leaf(&self) -> usize {
        match self {
            Node::Leaf(i) => *i,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }

    fn split_leaf(&mut self, id: usize, axis: Axis, new: usize) -> bool {
        if matches!(self, Node::Leaf(i) if *i == id) {
            *self = Node::Split { axis, ratio: 0.5, a: Box::new(Node::Leaf(id)), b: Box::new(Node::Leaf(new)) };
            return true;
        }
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split_leaf(id, axis, new) || b.split_leaf(id, axis, new),
        }
    }

    /// The tree without `id`; `None` when `id` was all of it.
    fn remove(self, id: usize) -> Option<Node> {
        match self {
            Node::Leaf(i) => (i != id).then_some(Node::Leaf(i)),
            Node::Split { axis, ratio, a, b } => match ((*a).remove(id), (*b).remove(id)) {
                (Some(a), Some(b)) => Some(Node::Split { axis, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
        }
    }

    fn rects(&self, r: R, out: &mut Vec<(usize, R)>) {
        match self {
            Node::Leaf(i) => out.push((*i, r)),
            Node::Split { axis, ratio, a, b } => match axis {
                Axis::Row => {
                    a.rects(R { w: r.w * ratio, ..r }, out);
                    b.rects(R { x: r.x + r.w * ratio, w: r.w * (1.0 - ratio), ..r }, out);
                }
                Axis::Col => {
                    a.rects(R { h: r.h * ratio, ..r }, out);
                    b.rects(R { y: r.y + r.h * ratio, h: r.h * (1.0 - ratio), ..r }, out);
                }
            },
        }
    }

    /// Moves the edge nearest above `id` along `axis`; `delta` > 0 grows `id`'s side.
    fn resize(&mut self, id: usize, axis: Axis, delta: f32) -> bool {
        let Node::Split { axis: ax, ratio, a, b } = self else { return false };
        let in_a = a.contains(id);
        if !in_a && !b.contains(id) {
            return false;
        }
        let child = if in_a { a } else { b };
        if child.resize(id, axis, delta) {
            return true;
        }
        if *ax == axis {
            *ratio = (*ratio + if in_a { delta } else { -delta }).clamp(0.15, 0.85);
            return true;
        }
        false
    }
}

fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// The tile next to `from` in `dir`: the nearest edge, then the longest shared side.
fn neighbor(rects: &[(usize, R)], from: usize, dir: Dir) -> Option<usize> {
    let cur = rects.iter().find(|(i, _)| *i == from)?.1;
    rects
        .iter()
        .filter(|(i, _)| *i != from)
        .filter_map(|(i, r)| {
            let (gap, shared) = match dir {
                Dir::Left => (cur.x - (r.x + r.w), overlap(cur.y, cur.y + cur.h, r.y, r.y + r.h)),
                Dir::Right => (r.x - (cur.x + cur.w), overlap(cur.y, cur.y + cur.h, r.y, r.y + r.h)),
                Dir::Up => (cur.y - (r.y + r.h), overlap(cur.x, cur.x + cur.w, r.x, r.x + r.w)),
                Dir::Down => (r.y - (cur.y + cur.h), overlap(cur.x, cur.x + cur.w, r.x, r.x + r.w)),
            };
            (gap > -EPS && shared > EPS).then_some((*i, gap, shared))
        })
        .min_by(|a, b| (a.1, -a.2).partial_cmp(&(b.1, -b.2)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _, _)| i)
}

// ---------------------------------------------------------------- tabs

#[derive(Clone, Copy, PartialEq, Debug)]
enum View {
    Session(SessionId),
    Agents,
    Jobs,
    /// Follows the last focused session, or is pinned to `follow`.
    Review { follow: SessionId, pinned: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Tile(usize),
    Bar(Side),
}

struct Tab {
    title: &'static str,
    /// Views by tile id; a closed tile's entry stays so ids stay stable.
    tiles: Vec<View>,
    root: Node,
    focus: Focus,
    zoom: Option<usize>,
    left: Option<View>,
    right: Option<View>,
    left_on: bool,
    right_on: bool,
    /// The session the review view follows: the one most recently focused.
    last_session: SessionId,
}

impl Tab {
    fn new(title: &'static str, tiles: Vec<View>, root: Node) -> Self {
        let first = root.first_leaf();
        let last_session = tiles
            .iter()
            .find_map(|v| if let View::Session(s) = v { Some(*s) } else { None })
            .unwrap_or(sid::DESIGN);
        Self {
            title,
            tiles,
            root,
            focus: Focus::Tile(first),
            zoom: None,
            left: None,
            right: None,
            left_on: false,
            right_on: false,
            last_session,
        }
    }

    fn single(title: &'static str, v: View) -> Self {
        Self::new(title, vec![v], Node::Leaf(0))
    }

    fn pair(title: &'static str, a: View, b: View) -> Self {
        let root = Node::Split { axis: Axis::Row, ratio: 0.5, a: Box::new(Node::Leaf(0)), b: Box::new(Node::Leaf(1)) };
        Self::new(title, vec![a, b], root)
    }

    fn rects(&self) -> Vec<(usize, R)> {
        let mut out = vec![];
        match self.zoom {
            Some(z) => out.push((z, R { x: 0.0, y: 0.0, w: 1.0, h: 1.0 })),
            None => self.root.rects(R { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, &mut out),
        }
        out
    }

    fn focused_tile(&self) -> Option<usize> {
        if let Focus::Tile(i) = self.focus { Some(i) } else { None }
    }

    fn set_focus(&mut self, f: Focus) {
        self.focus = f;
        if let Focus::Tile(i) = f
            && let View::Session(s) = self.tiles[i]
        {
            self.last_session = s;
        }
    }

    fn focus_dir(&mut self, dir: Dir) {
        let rects = self.rects();
        match self.focus {
            Focus::Tile(i) => match neighbor(&rects, i, dir) {
                Some(n) => self.set_focus(Focus::Tile(n)),
                None if dir == Dir::Left && self.left_on && self.left.is_some() => self.focus = Focus::Bar(Side::Left),
                None if dir == Dir::Right && self.right_on && self.right.is_some() => {
                    self.focus = Focus::Bar(Side::Right)
                }
                None => {}
            },
            Focus::Bar(Side::Left) if dir == Dir::Right => {
                let first = rects.iter().min_by(|a, b| (a.1.x, a.1.y).partial_cmp(&(b.1.x, b.1.y)).unwrap());
                if let Some((i, _)) = first {
                    self.set_focus(Focus::Tile(*i));
                }
            }
            Focus::Bar(Side::Right) if dir == Dir::Left => {
                let last = rects.iter().max_by(|a, b| (a.1.x + a.1.w, -a.1.y).partial_cmp(&(b.1.x + b.1.w, -b.1.y)).unwrap());
                if let Some((i, _)) = last {
                    self.set_focus(Focus::Tile(*i));
                }
            }
            Focus::Bar(_) => {}
        }
    }

    fn view_not_shown(&self) -> View {
        let cands = [
            View::Session(sid::REST),
            View::Session(sid::DEPLOY),
            View::Session(sid::STREAM),
            View::Agents,
            View::Jobs,
        ];
        cands.into_iter().find(|v| !self.tiles.contains(v)).unwrap_or(View::Agents)
    }

    fn split(&mut self, axis: Option<Axis>) -> Option<&'static str> {
        let id = self.focused_tile()?;
        self.zoom = None;
        let depth = self.root.depth_of(id, 0).unwrap_or(0);
        // The spiral: the first split goes right, the next down, and so on.
        let axis = axis.unwrap_or(if depth % 2 == 0 { Axis::Row } else { Axis::Col });
        let new = self.tiles.len();
        self.tiles.push(self.view_not_shown());
        self.root.split_leaf(id, axis, new);
        self.set_focus(Focus::Tile(new));
        None
    }

    fn close_tile(&mut self) -> Option<&'static str> {
        let id = self.focused_tile()?;
        self.zoom = None;
        match self.root.clone().remove(id) {
            Some(root) => {
                self.root = root;
                let next = self.root.first_leaf();
                self.set_focus(Focus::Tile(next));
                None
            }
            None => Some("the last tile stays; close the tab instead"),
        }
    }

    fn swap(&mut self, dir: Dir) {
        let Some(id) = self.focused_tile() else { return };
        if let Some(n) = neighbor(&self.rects(), id, dir) {
            self.tiles.swap(id, n);
            self.set_focus(Focus::Tile(n));
        }
    }

    fn resize(&mut self, axis: Axis, delta: f32) {
        if let Some(id) = self.focused_tile() {
            self.root.resize(id, axis, delta);
        }
    }

    fn open_view(&mut self) {
        let Some(id) = self.focused_tile() else { return };
        let follow = self.last_session;
        self.tiles[id] = match self.tiles[id] {
            View::Session(_) => View::Agents,
            View::Agents => View::Jobs,
            View::Jobs => View::Review { follow, pinned: false },
            View::Review { .. } => View::Session(follow),
        };
    }

    fn pin(&mut self) -> Option<&'static str> {
        let id = self.focused_tile()?;
        let last = self.last_session;
        if let View::Review { follow, pinned } = &mut self.tiles[id] {
            *pinned = !*pinned;
            *follow = last;
            None
        } else {
            Some("only a review, jobs, plan or files view can be pinned")
        }
    }

    /// The sessions the tab shows.
    fn sessions(&self) -> Vec<SessionId> {
        let mut out = vec![];
        let mut seen = vec![];
        self.root.rects(R { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, &mut seen);
        for (i, _) in seen {
            if let View::Session(s) = self.tiles[i] {
                out.push(s);
            }
        }
        out
    }
}

fn build_tabs() -> Vec<Tab> {
    use View::*;
    let cockpit = {
        let root = Node::Split {
            axis: Axis::Row,
            ratio: 0.5,
            a: Box::new(Node::Leaf(0)),
            b: Box::new(Node::Split { axis: Axis::Col, ratio: 0.5, a: Box::new(Node::Leaf(1)), b: Box::new(Node::Leaf(2)) }),
        };
        let mut t = Tab::new(
            "Cockpit",
            vec![Session(sid::DESIGN), Session(sid::FLAKY), Review { follow: sid::DESIGN, pinned: true }],
            root,
        );
        t.left = Some(Agents);
        t.left_on = true;
        t
    };
    vec![
        cockpit,
        Tab::pair("Tiles only", Session(sid::DESIGN), Session(sid::DEPLOY)),
        Tab::single("Turn at rest", Session(sid::REST)),
        Tab::single("Turn streaming", Session(sid::STREAM)),
        Tab::pair("Approval", Session(sid::APPROVAL), Session(sid::APPROVAL_SCOPE)),
        Tab::pair("Questions", Session(sid::QUESTIONS), Session(sid::QUESTIONS_REVIEW)),
        Tab::single("Plan review", Session(sid::PLAN)),
        Tab::single("Composer", Session(sid::COMPOSER)),
        Tab::single("Agents wide", Agents),
        Tab::single("Jobs", Jobs),
    ]
}

// ---------------------------------------------------------------- shell

#[derive(Clone, Copy, PartialEq, Debug)]
enum Overlay {
    None,
    Agent(usize),
    Full(usize),
    Floating(SessionId),
}

pub struct Shell {
    data: Data,
    theme: Theme,
    palette_ix: usize,
    scale_ix: usize,
    tabs: Vec<Tab>,
    active: usize,
    overlay: Overlay,
    focus: FocusHandle,
    note: Option<String>,
    jump: usize,
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Shell {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
        let (palette_ix, scale_ix) = (env("MU_SKETCH_PALETTE").unwrap_or(0), env("MU_SKETCH_SCALE").unwrap_or(0));
        let tabs = build_tabs();
        let active = env("MU_SKETCH_TAB").unwrap_or(0).min(tabs.len() - 1);
        Self {
            data: data::build(),
            theme: Theme::build(palette_ix, scale_ix),
            palette_ix,
            scale_ix,
            tabs,
            active,
            overlay: Overlay::None,
            focus: cx.focus_handle(),
            note: None,
            jump: 0,
        }
    }

    fn tab(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn go_tab(&mut self, n: usize) {
        if n < self.tabs.len() {
            self.active = n;
        }
    }

    fn open_palette_ctx(&self) -> &'static str {
        match self.overlay {
            Overlay::None => "window",
            Overlay::Agent(_) | Overlay::Full(_) => "palette",
            Overlay::Floating(_) => "palette floating",
        }
    }

    fn palette_len(&self) -> usize {
        match self.overlay {
            Overlay::Agent(_) => self.data.palette.agents.len(),
            Overlay::Full(_) => self.data.palette.entries.len(),
            _ => 0,
        }
    }

    fn move_selection(&mut self, down: bool) {
        let n = self.palette_len().max(1);
        let step = |s: usize| if down { (s + 1) % n } else { (s + n - 1) % n };
        self.overlay = match self.overlay {
            Overlay::Agent(s) => Overlay::Agent(step(s)),
            Overlay::Full(s) => Overlay::Full(step(s)),
            o => o,
        };
    }

    fn run_palette(&mut self) {
        let (label, is_agent) = match self.overlay {
            Overlay::Agent(s) => (self.data.palette.agents.get(s).map(|e| e.label), true),
            Overlay::Full(s) => (self.data.palette.entries.get(s).map(|e| e.label), false),
            _ => return,
        };
        self.overlay = Overlay::None;
        let Some(label) = label else { return };
        let title = label.strip_prefix("Agent: ").unwrap_or(label);
        let session = self.data.sessions.iter().find(|s| s.title == title).map(|s| s.id);
        match session {
            Some(id) if is_agent || label.starts_with("Agent: ") => {
                let tab = self.tab();
                if let Some(t) = tab.focused_tile() {
                    tab.tiles[t] = View::Session(id);
                    tab.last_session = id;
                }
            }
            _ => self.note = Some(format!("would run: {label}")),
        }
    }

    fn jump_request(&mut self) {
        let waiting = self.data.waiting();
        if waiting.is_empty() {
            self.note = Some("nothing waits on you".into());
            return;
        }
        if matches!(self.overlay, Overlay::Floating(_)) {
            self.jump += 1;
        }
        self.overlay = Overlay::Floating(waiting[self.jump % waiting.len()]);
    }

    fn rebuild_theme(&mut self) {
        self.theme = Theme::build(self.palette_ix, self.scale_ix);
    }

    fn tab_title(&self, tab: &Tab) -> String {
        let waiting = tab
            .sessions()
            .iter()
            .filter(|s| self.data.session(**s).state == AgentState::NeedsYou)
            .count();
        match waiting {
            0 => tab.title.to_string(),
            n => format!("⚑ {} · {n}", tab.title),
        }
    }

    fn render_view(&self, v: View, focused: bool, wide: bool, last: SessionId) -> AnyElement {
        let mk = |session, pinned| ViewCtx { t: &self.theme, data: &self.data, session, focused, wide, pinned };
        match v {
            View::Session(s) => session::view(&mk(s, false)),
            View::Agents => agents::view(&mk(last, false)),
            View::Jobs => jobs::view(&mk(last, false)),
            View::Review { follow, pinned } => review::view(&mk(if pinned { follow } else { last }, pinned)),
        }
    }

    fn render_node(&self, node: &Node, tab: &Tab, vw: f32, rects: &[(usize, R)]) -> AnyElement {
        let t = &self.theme;
        match node {
            Node::Leaf(id) => {
                let focused = tab.focus == Focus::Tile(*id);
                let wide = rects.iter().find(|(i, _)| i == id).is_some_and(|(_, r)| r.w * vw > 700.0);
                div()
                    .id(("tile", *id))
                    .size_full()
                    .overflow_hidden()
                    .bg(t.window)
                    .border_l_2()
                    .border_color(if focused { t.focus_ring } else { transparent_black() })
                    .child(self.render_view(tab.tiles[*id], focused, wide, tab.last_session))
                    .into_any_element()
            }
            Node::Split { axis, ratio, a, b } => {
                let child = |n: &Node, grow: f32| {
                    let mut d = div().flex_basis(px(0.)).flex_grow(grow).flex_shrink(1.0).min_w_0().min_h_0().overflow_hidden();
                    d = match axis {
                        Axis::Row => d.h_full(),
                        Axis::Col => d.w_full(),
                    };
                    d.child(self.render_node(n, tab, vw, rects))
                };
                let d = div().flex().size_full().bg(t.line).gap(px(metrics::TILE_GAP));
                let d = match axis {
                    Axis::Row => d.flex_row(),
                    Axis::Col => d.flex_col(),
                };
                d.child(child(a, *ratio)).child(child(b, 1.0 - *ratio)).into_any_element()
            }
        }
    }

    fn render_bar(&self, tab: &Tab, side: Side) -> Option<AnyElement> {
        let (view, on) = match side {
            Side::Left => (tab.left, tab.left_on),
            Side::Right => (tab.right, tab.right_on),
        };
        let view = view.filter(|_| on)?;
        let t = &self.theme;
        let focused = tab.focus == Focus::Bar(side);
        let (w, edge) = match side {
            Side::Left => (metrics::LEFT_BAR_W, div().border_r_1()),
            Side::Right => (metrics::RIGHT_BAR_W, div().border_l_1()),
        };
        Some(
            edge.w(px(w))
                .h_full()
                .flex_none()
                .overflow_hidden()
                .bg(t.bar)
                .border_color(if focused { t.focus_ring } else { t.line })
                .child(self.render_view(view, focused, false, tab.last_session))
                .into_any_element(),
        )
    }

    fn render_tab_bar(&self) -> AnyElement {
        let t = &self.theme;
        let mut bar = ui(t)
            .flex()
            .flex_none()
            .items_center()
            .h(px(metrics::TAB_BAR_H))
            .px(px(8.))
            .gap_1()
            .bg(t.bar)
            .border_b_1()
            .border_color(t.line);
        for (n, tab) in self.tabs.iter().enumerate() {
            let active = n == self.active;
            bar = bar.child(
                div()
                    .px(px(10.))
                    .py(px(3.))
                    .rounded(px(metrics::RADIUS))
                    .when(active, |d| d.bg(t.selected).text_color(t.strong))
                    .when(!active, |d| d.text_color(t.muted))
                    .child(self.tab_title(tab)),
            );
        }
        bar.child(div().text_color(t.faint).child("+"))
            .child(div().flex_1())
            .child(div().text_color(t.faint).child(format!(
                "{} · {} · {}",
                t.name,
                t.ty.name,
                data::plat("⌘P", "Ctrl+Shift+P")
            )))
            .into_any_element()
    }

    fn render_status_bar(&self, window: &Window) -> AnyElement {
        let t = &self.theme;
        let s = &self.data.status;
        let pending = window
            .pending_input_keystrokes()
            .map(|k| k.iter().map(|k| k.unparse(HOST_OS)).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let mut remotes = ui(t).flex().gap_3().text_color(t.muted);
        for (name, ok) in &s.remotes {
            remotes = remotes.child(div().child(format!("{name} {}", if *ok { "✓" } else { "✗" })).text_color(if *ok { t.muted } else { t.err }));
        }
        ui(t)
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .h(px(metrics::STATUS_BAR_H))
            .px(px(10.))
            .bg(t.bar)
            .border_t_1()
            .border_color(t.line)
            .text_size(px(t.ty.small))
            .text_color(t.muted)
            .child(div().text_color(t.strong).child(s.profile))
            .child(format!("{} running · {} need you · today {}", s.running, s.need_you, s.today))
            .when(!pending.is_empty(), |d| d.child(div().text_color(t.accent).child(format!("{pending} …"))))
            .when_some(self.note.clone(), |d, n| d.child(div().text_color(t.warn).child(n)))
            .child(div().flex_1())
            .child(remotes)
            .into_any_element()
    }
}

macro_rules! on {
    ($el:expr, $cx:expr, $A:ty, |$this:ident| $body:expr) => {
        $el.on_action($cx.listener(|$this, _: &$A, _w, cx| {
            $this.note = None;
            $body;
            cx.notify();
        }))
    };
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus.is_focused(window) {
            window.focus(&self.focus, cx);
        }
        let t = self.theme.clone();
        let vw: f32 = window.viewport_size().width.into();
        let tab = &self.tabs[self.active];
        let rects = tab.rects();

        let main = match tab.zoom {
            Some(z) => self.render_node(&Node::Leaf(z), tab, vw, &rects),
            None => self.render_node(&tab.root, tab, vw, &rects),
        };
        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .when_some(self.render_bar(tab, Side::Left), |d, b| d.child(b))
            .child(div().flex_1().min_w_0().h_full().child(main))
            .when_some(self.render_bar(tab, Side::Right), |d, b| d.child(b));

        let overlay = match self.overlay {
            Overlay::None => None,
            Overlay::Agent(s) => Some(palette::agent(&t, &self.data, s)),
            Overlay::Full(s) => Some(palette::full(&t, &self.data, s)),
            Overlay::Floating(id) => {
                let c = ViewCtx { t: &t, data: &self.data, session: id, focused: true, wide: false, pinned: false };
                Some(palette::floating(&t, &self.data, id, session::view(&c)))
            }
        };

        let root = div()
            .key_context(self.open_palette_ctx())
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.window)
            .text_color(t.text)
            .child(self.render_tab_bar())
            .child(body)
            .child(self.render_status_bar(window))
            .when_some(overlay, |d, o| {
                d.child(div().absolute().inset_0().bg(t.scrim).flex().justify_center().items_center().child(o))
            });

        let root = on!(root, cx, AgentPalette, |this| {
            this.overlay = if matches!(this.overlay, Overlay::Agent(_)) { Overlay::None } else { Overlay::Agent(0) }
        });
        let root = on!(root, cx, FullPalette, |this| {
            this.overlay = if matches!(this.overlay, Overlay::Full(_)) { Overlay::None } else { Overlay::Full(0) }
        });
        let root = on!(root, cx, JumpRequest, |this| this.jump_request());
        let root = on!(root, cx, CloseOverlay, |this| this.overlay = Overlay::None);
        let root = on!(root, cx, PaletteUp, |this| this.move_selection(false));
        let root = on!(root, cx, PaletteDown, |this| this.move_selection(true));
        let root = on!(root, cx, PaletteRun, |this| this.run_palette());
        let root = on!(root, cx, OpenInTile, |this| {
            if let Overlay::Floating(id) = this.overlay {
                this.overlay = Overlay::None;
                let tab = this.tab();
                if let Some(t) = tab.focused_tile() {
                    tab.tiles[t] = View::Session(id);
                    tab.last_session = id;
                }
            }
        });
        let root = on!(root, cx, NewTab, |this| {
            this.tabs.push(Tab::single("New tab", View::Agents));
            this.active = this.tabs.len() - 1;
        });
        let root = on!(root, cx, CloseTab, |this| {
            if this.tabs.len() > 1 {
                this.tabs.remove(this.active);
                this.active = this.active.min(this.tabs.len() - 1);
            }
        });
        let root = on!(root, cx, NextTab, |this| this.active = (this.active + 1) % this.tabs.len());
        let root = on!(root, cx, PrevTab, |this| this.active = (this.active + this.tabs.len() - 1) % this.tabs.len());
        let root = on!(root, cx, GoTab1, |this| this.go_tab(0));
        let root = on!(root, cx, GoTab2, |this| this.go_tab(1));
        let root = on!(root, cx, GoTab3, |this| this.go_tab(2));
        let root = on!(root, cx, GoTab4, |this| this.go_tab(3));
        let root = on!(root, cx, GoTab5, |this| this.go_tab(4));
        let root = on!(root, cx, GoTab6, |this| this.go_tab(5));
        let root = on!(root, cx, GoTab7, |this| this.go_tab(6));
        let root = on!(root, cx, GoTab8, |this| this.go_tab(7));
        let root = on!(root, cx, GoTab9, |this| this.go_tab(8));
        let root = on!(root, cx, RenameTab, |this| this.note = Some("rename: not in the sketch".into()));
        let root = on!(root, cx, SaveLayout, |this| this.note = Some("layouts: not in the sketch".into()));
        let root = on!(root, cx, SwitchLayout, |this| this.note = Some("layouts: not in the sketch".into()));
        let root = on!(root, cx, Split, |this| this.note = this.tab().split(None).map(Into::into));
        let root = on!(root, cx, SplitRight, |this| this.note = this.tab().split(Some(Axis::Row)).map(Into::into));
        let root = on!(root, cx, SplitDown, |this| this.note = this.tab().split(Some(Axis::Col)).map(Into::into));
        let root = on!(root, cx, CloseTile, |this| this.note = this.tab().close_tile().map(Into::into));
        let root = on!(root, cx, FocusLeft, |this| this.tab().focus_dir(Dir::Left));
        let root = on!(root, cx, FocusRight, |this| this.tab().focus_dir(Dir::Right));
        let root = on!(root, cx, FocusUp, |this| this.tab().focus_dir(Dir::Up));
        let root = on!(root, cx, FocusDown, |this| this.tab().focus_dir(Dir::Down));
        let root = on!(root, cx, SwapLeft, |this| this.tab().swap(Dir::Left));
        let root = on!(root, cx, SwapRight, |this| this.tab().swap(Dir::Right));
        let root = on!(root, cx, SwapUp, |this| this.tab().swap(Dir::Up));
        let root = on!(root, cx, SwapDown, |this| this.tab().swap(Dir::Down));
        let root = on!(root, cx, Narrower, |this| this.tab().resize(Axis::Row, -0.05));
        let root = on!(root, cx, Wider, |this| this.tab().resize(Axis::Row, 0.05));
        let root = on!(root, cx, Shorter, |this| this.tab().resize(Axis::Col, -0.05));
        let root = on!(root, cx, Taller, |this| this.tab().resize(Axis::Col, 0.05));
        let root = on!(root, cx, Zoom, |this| {
            let tab = this.tab();
            tab.zoom = if tab.zoom.is_some() { None } else { tab.focused_tile() };
        });
        let root = on!(root, cx, ToggleLeftBar, |this| {
            let tab = this.tab();
            tab.left.get_or_insert(View::Agents);
            tab.left_on = !tab.left_on;
            if !tab.left_on && tab.focus == Focus::Bar(Side::Left) {
                tab.focus = Focus::Tile(tab.root.first_leaf());
            }
        });
        let root = on!(root, cx, ToggleRightBar, |this| {
            let tab = this.tab();
            tab.right.get_or_insert(View::Jobs);
            tab.right_on = !tab.right_on;
            if !tab.right_on && tab.focus == Focus::Bar(Side::Right) {
                tab.focus = Focus::Tile(tab.root.first_leaf());
            }
        });
        let root = on!(root, cx, OpenView, |this| this.tab().open_view());
        let root = on!(root, cx, Pin, |this| this.note = this.tab().pin().map(Into::into));
        let root = on!(root, cx, CyclePalette, |this| {
            this.palette_ix = (this.palette_ix + 1) % palette_names().len();
            this.rebuild_theme();
        });
        let root = on!(root, cx, ToggleScale, |this| {
            this.scale_ix = (this.scale_ix + 1) % crate::theme::SCALES.len();
            this.rebuild_theme();
        });
        root.on_action(|_: &Quit, _, cx| cx.quit())
    }
}
