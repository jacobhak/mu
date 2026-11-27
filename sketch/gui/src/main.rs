//! A throwaway GPUI sketch of mu's desktop app: fake data, no wire, no agent. See docs/design/gui-sketch.md.
//!
//! Environment: `MU_SKETCH_TAB`, `MU_SKETCH_PALETTE` and `MU_SKETCH_SCALE` (indexes) choose what it opens on.
mod data;
mod shell;
mod theme;
mod views;

use gpui::{App, AppContext as _, Bounds, Focusable as _, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use shell::Shell;
fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        shell::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1500.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("mu (sketch)".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| {
                let shell = cx.new(Shell::new);
                let focus = shell.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
                shell
            },
        )
        .expect("open the window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
