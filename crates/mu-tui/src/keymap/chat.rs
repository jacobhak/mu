//! The composer's commands. Text editing is `editor`; submitting is `editor.submit`.

binds! {
    QUEUE = "chat", "queue", ["ctrl+q"];
    ABORT = "chat", "abort", ["esc"];
    POP_QUEUE = "chat", "pop_queue", ["shift+up"];
    CYCLE_THINKING = "chat", "cycle_thinking", ["shift+tab"];
    TOGGLE_THINKING = "chat", "toggle_thinking", ["ctrl+t"];
    TOGGLE_EXPANDED = "chat", "toggle_expanded", ["ctrl+o"];
    MODEL_PICKER = "chat", "model_picker", ["alt+m"];
    EDIT_EXTERNAL = "chat", "edit_external", ["ctrl+g"];
    REPLY = "chat", "reply", ["ctrl+r"];
    JOBS = "chat", "jobs", ["alt+j"];
    JOB_CANCEL = "chat", "job_cancel", ["alt+k"];
    JOB_NEXT = "chat", "job_next", ["alt+down"];
    JOB_PREV = "chat", "job_prev", ["alt+up"];
    /// With a job marked: cancel it.
    JOB_MARK_CANCEL = "chat", "job_mark_cancel", ["k", "x"];
    /// With a job marked: take the mark off.
    JOB_UNMARK = "chat", "job_unmark", ["esc"];
    HISTORY_PREV = "chat", "history_prev", ["up", "ctrl+p"];
    HISTORY_NEXT = "chat", "history_next", ["down", "ctrl+n"];
    PASTE_IMAGE = "chat", "paste_image", ["ctrl+v"];
    /// Tab: take the ghost text or the selected `/` completion.
    COMPLETE = "chat", "complete", ["tab"];
    /// Right at the end of the line: take the ghost text.
    ACCEPT_GHOST = "chat", "accept_ghost", ["right"];
    MENU_PREV = "chat", "menu_prev", ["up", "ctrl+p"];
    MENU_NEXT = "chat", "menu_next", ["down", "ctrl+n"];
    MENU_CLOSE = "chat", "menu_close", ["esc"];
}
