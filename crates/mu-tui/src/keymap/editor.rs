//! The text editor behind the composer and every input field in a panel or wizard.

binds! {
    /// Send the line (composer) or accept it (a field).
    SUBMIT = "editor", "submit", ["enter"];
    /// Enter and Ctrl+Enter only arrive distinct from Enter on terminals with the kitty keyboard
    /// protocol; Ctrl+J and Alt+Enter work everywhere.
    NEWLINE = "editor", "newline", ["shift+enter", "alt+enter", "ctrl+enter", "ctrl+j"];
    LEFT = "editor", "left", ["left", "ctrl+b"];
    RIGHT = "editor", "right", ["right", "ctrl+f"];
    WORD_LEFT = "editor", "word_left", ["alt+b"];
    WORD_RIGHT = "editor", "word_right", ["alt+f"];
    HOME = "editor", "home", ["home", "ctrl+a"];
    END = "editor", "end", ["end", "ctrl+e"];
    UP = "editor", "up", ["up", "ctrl+p"];
    DOWN = "editor", "down", ["down", "ctrl+n"];
    BACKSPACE = "editor", "backspace", ["backspace", "ctrl+h"];
    DELETE = "editor", "delete", ["delete", "ctrl+d"];
    DELETE_WORD_BACK = "editor", "delete_word_back", ["alt+backspace", "ctrl+w"];
    DELETE_WORD_FWD = "editor", "delete_word_fwd", ["alt+d"];
    KILL_EOL = "editor", "kill_eol", ["ctrl+k"];
    KILL_BOL = "editor", "kill_bol", ["ctrl+u"];
    YANK = "editor", "yank", ["ctrl+y"];
}
