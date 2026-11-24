//! Binds for the jobs panel: the list (both tabs), a transcript view, and the message prompt.

binds! {
    /// Leave the panel (list).
    JOBS_CLOSE = "jobs", "close", ["esc", "q"];
    JOBS_DOWN = "jobs", "down", ["down", "j", "ctrl+n"];
    JOBS_UP = "jobs", "up", ["up", "k", "ctrl+p"];
    JOBS_TOP = "jobs", "top", ["home", "g"];
    JOBS_BOTTOM = "jobs", "bottom", ["end", "G"];
    JOBS_PAGE_DOWN = "jobs", "page_down", ["pagedown"];
    JOBS_PAGE_UP = "jobs", "page_up", ["pageup"];
    /// Open the transcript (Jobs) or peek at the session (Sessions).
    JOBS_OPEN = "jobs", "open", ["enter"];
    JOBS_SWITCH_TAB = "jobs", "switch_tab", ["tab", "shift+tab"];
    JOBS_KILL = "jobs", "kill", ["x"];
    JOBS_KILL_ALL = "jobs", "kill_all", ["X"];
    JOBS_DIFF = "jobs", "diff", ["d"];
    JOBS_APPLY = "jobs", "apply", ["a"];
    JOBS_MESSAGE = "jobs", "message", ["n"];
    JOBS_STOP = "jobs", "stop", ["s"];
    JOBS_TERMINATE = "jobs", "terminate", ["S"];
    JOBS_TAKE = "jobs", "take", ["t"];
    /// Forward the job's listening ports here and open the first in the browser.
    JOBS_OPEN_PORT = "jobs", "open_port", ["o"];

    JOBS_VIEW_CLOSE = "jobs_view", "close", ["esc", "q"];
    JOBS_VIEW_DOWN = "jobs_view", "down", ["down", "j", "ctrl+n"];
    JOBS_VIEW_UP = "jobs_view", "up", ["up", "k", "ctrl+p"];
    JOBS_VIEW_PAGE_DOWN = "jobs_view", "page_down", ["pagedown", "ctrl+f"];
    JOBS_VIEW_PAGE_UP = "jobs_view", "page_up", ["pageup", "ctrl+b"];
    JOBS_VIEW_HALF_DOWN = "jobs_view", "half_page_down", ["ctrl+d"];
    JOBS_VIEW_HALF_UP = "jobs_view", "half_page_up", ["ctrl+u"];
    JOBS_VIEW_TOP = "jobs_view", "top", ["home", "g"];
    JOBS_VIEW_BOTTOM = "jobs_view", "bottom", ["end", "G"];
    JOBS_VIEW_FOLLOW = "jobs_view", "follow", ["f"];
    JOBS_VIEW_KILL = "jobs_view", "kill", ["x"];

    JOBS_INPUT_CANCEL = "jobs_input", "cancel", ["esc"];
    JOBS_INPUT_BACKSPACE = "jobs_input", "backspace", ["backspace"];
    JOBS_INPUT_CLEAR = "jobs_input", "clear", ["ctrl+u"];
    JOBS_INPUT_DELETE_WORD = "jobs_input", "delete_word_back", ["ctrl+w"];

    /// Answer a confirmation (kill all, terminate a session); any other key cancels.
    JOBS_CONFIRM = "jobs_confirm", "confirm", ["y"];
}
