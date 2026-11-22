//! Binds for the reply picker (`reply`). The count digits (`3j`) are not binds.

binds! {
    /// Drops a pending count or selection first, then closes.
    REPLY_CANCEL = "reply", "cancel", ["esc", "ctrl+g"];
    REPLY_CLOSE = "reply", "close", ["q"];
    REPLY_UP = "reply", "up", ["up", "k", "ctrl+p"];
    REPLY_DOWN = "reply", "down", ["down", "j", "ctrl+n"];
    REPLY_LEFT = "reply", "left", ["left", "h", "b", "ctrl+b", "alt+b"];
    REPLY_RIGHT = "reply", "right", ["right", "l", "w", "e", "ctrl+f", "alt+f"];
    REPLY_EXTEND_UP = "reply", "extend_up", ["shift+up", "K"];
    REPLY_EXTEND_DOWN = "reply", "extend_down", ["shift+down", "J"];
    REPLY_EXTEND_LEFT = "reply", "extend_left", ["shift+left", "H", "B"];
    REPLY_EXTEND_RIGHT = "reply", "extend_right", ["shift+right", "L", "W", "E"];
    REPLY_PAGE_UP = "reply", "page_up", ["pageup", "alt+v"];
    REPLY_PAGE_DOWN = "reply", "page_down", ["pagedown", "ctrl+v"];
    REPLY_EXTEND_PAGE_UP = "reply", "extend_page_up", ["shift+pageup"];
    REPLY_EXTEND_PAGE_DOWN = "reply", "extend_page_down", ["shift+pagedown"];
    REPLY_HALF_UP = "reply", "half_up", ["ctrl+u"];
    REPLY_HALF_DOWN = "reply", "half_down", ["ctrl+d"];
    REPLY_TOP = "reply", "top", ["home", "alt+<"];
    REPLY_BOTTOM = "reply", "bottom", ["end", "G", "alt+>"];
    REPLY_EXTEND_TOP = "reply", "extend_top", ["shift+home"];
    REPLY_EXTEND_BOTTOM = "reply", "extend_bottom", ["shift+end"];
    /// Pressed twice, goes to the top.
    REPLY_GOTO_PREFIX = "reply", "goto_prefix", ["g"];
    REPLY_UNIT_START = "reply", "unit_start", ["0", "^", "ctrl+a"];
    REPLY_UNIT_END = "reply", "unit_end", ["$", "ctrl+e"];
    REPLY_BLOCK_PREV = "reply", "block_prev", ["{"];
    REPLY_BLOCK_NEXT = "reply", "block_next", ["}"];
    REPLY_MARK = "reply", "mark", ["space"];
    REPLY_CLEAR = "reply", "clear", ["c"];
    REPLY_QUOTE = "reply", "quote", ["enter"];
    /// Ctrl+Alt is how some terminals report AltGr, which types `[` and `]` on many layouts.
    REPLY_PREV_REPLY = "reply", "prev_reply", ["[", "shift+tab", "ctrl+alt+["];
    REPLY_NEXT_REPLY = "reply", "next_reply", ["]", "tab", "ctrl+alt+]"];
}
