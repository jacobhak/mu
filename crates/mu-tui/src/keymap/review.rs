//! The document review and the embedded terminal. The model roles editor and the Sessions tab
//! read their keys through `select` and `jobs`.

binds! {
    /// Leave the review without deciding.
    REVIEW_CANCEL = "review", "cancel", ["esc", "ctrl+c"];
    /// Typing feedback: back to the action list.
    REVIEW_FEEDBACK_BACK = "review", "feedback_back", ["esc"];
    REVIEW_COPY = "review", "copy", ["c"];
    /// Open the document in the editor (when it is a file).
    REVIEW_EDIT = "review", "edit", ["ctrl+g"];
    /// On an action that takes an optional note: type it.
    REVIEW_NOTE = "review", "note", ["n"];
    REVIEW_TOGGLE_FOCUS = "review", "toggle_focus", ["tab", "shift+tab"];
    REVIEW_PAGE_UP = "review", "page_up", ["pageup"];
    REVIEW_PAGE_DOWN = "review", "page_down", ["pagedown"];
    REVIEW_TOP = "review", "top", ["home"];
    REVIEW_BOTTOM = "review", "bottom", ["end"];
    /// Document focused.
    REVIEW_SCROLL_UP = "review", "scroll_up", ["up", "k"];
    /// Document focused; past the end it moves to the actions.
    REVIEW_SCROLL_DOWN = "review", "scroll_down", ["down", "j"];
    REVIEW_DOC_TOP = "review", "doc_top", ["g"];
    REVIEW_DOC_BOTTOM = "review", "doc_bottom", ["G"];
    /// Document focused: move to the actions.
    REVIEW_TO_ACTIONS = "review", "to_actions", ["enter", "right", "l"];
    /// Actions focused; above the first it moves to the document.
    REVIEW_SELECT_PREV = "review", "select_prev", ["up", "k"];
    REVIEW_SELECT_NEXT = "review", "select_next", ["down", "j"];
    REVIEW_TIER_PREV = "review", "tier_prev", ["left"];
    REVIEW_TIER_NEXT = "review", "tier_next", ["right"];
    /// Only with the model slider showing.
    REVIEW_SLIDER_PREV = "review", "slider_prev", ["h"];
    REVIEW_SLIDER_NEXT = "review", "slider_next", ["l"];
    /// Take the selected action.
    REVIEW_CONFIRM = "review", "confirm", ["enter"];
    /// Leave the command running on the embedded terminal: it is killed.
    TERMINAL_KILL = "terminal", "kill", ["esc"];
}
