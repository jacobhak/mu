//! Binds for the select context(s): pickers, the ask dialog, and overlays built on them.

binds! {
    /// Close the picker, or leave the free-text row or a note for the list.
    SELECT_CANCEL = "select", "cancel", ["esc"];
    /// Step back in a multi-step dialog (when back keys are on).
    SELECT_BACK = "select", "back", ["shift+tab"];
    /// Step back, only while the filter is empty (otherwise it moves the filter's cursor).
    SELECT_BACK_EMPTY = "select", "back_empty", ["left"];
    SELECT_UP = "select", "move_up", ["up", "ctrl+p"];
    SELECT_DOWN = "select", "move_down", ["down", "ctrl+n"];
    SELECT_CHOOSE = "select", "choose", ["enter"];
    /// Type a note on the highlighted row (when notes are on).
    SELECT_NOTE = "select", "note", ["tab"];
    /// Same, only while the filter is empty.
    SELECT_NOTE_EMPTY = "select", "note_empty", ["right"];
    /// Mark or unmark the highlighted row of a multi-select (filter empty).
    SELECT_TOGGLE = "select", "toggle", ["space"];
    /// Scroll the context text above the rows, when a dialog has one.
    SELECT_CONTEXT_UP = "select", "context_up", ["pageup"];
    SELECT_CONTEXT_DOWN = "select", "context_down", ["pagedown"];
    /// In a permission prompt: type a pattern instead of picking a width.
    OVERLAY_PATTERN = "overlay", "approval_pattern", ["e"];
    /// In the memory history: revert to the highlighted version (filter empty).
    OVERLAY_REVERT = "overlay", "memory_revert", ["r"];
}
