# Key bindings

Every key mu reacts to can be rebound in `~/.mu/config.toml`. A binding is an action in a context:

```toml
[keys.chat]
toggle_thinking = "ctrl+t"             # one chord
model_picker = ["alt+m", "f2"]         # several; the list replaces the defaults
toggle_expanded = []                   # unbound (so is "none")

[keys.editor]
kill_eol = "ctrl+k"
```

Setting an action replaces all of its default chords. Contexts are separate: the same chord may mean different things in `chat` and in `jobs`, because only one of them reads a key at a time. The exception is the composer, where `app`, `chat` and `editor` all see every key; mu warns at start-up when a rebinding puts two of them on one chord (the first one wins). An unknown action, or a chord mu cannot read, is warned about and ignored, and that action keeps its default.

## Chord syntax

Modifiers and one key joined by `+`, case-insensitive: `ctrl+t`, `alt+enter`, `ctrl+shift+up`, `f5`, `?`.

- Modifiers: `ctrl`, `alt` (also `meta`, `opt`), `shift`, `super` (also `cmd`, `win`).
- Keys: a single character, or `enter`, `esc`, `tab`, `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`, `space`, `plus`, `f1` to `f24`.
- A shifted character is the character: write `K`, not `shift+k`; `ctrl+T` is `ctrl+t`. `shift+tab` is the back-tab key.
- Terminals cannot tell some chords apart: `ctrl+i` is Tab, `ctrl+m` is Enter, `ctrl+[` is Esc, and without the kitty keyboard protocol `shift+enter` and `ctrl+enter` arrive as plain Enter. Bind what your terminal sends.

The defaults below are what mu does without any `[keys]` section.

## app

| Action | Default | |
|---|---|---|
| `quit` | `ctrl+c` | The first press arms the exit and the second leaves, whatever is on screen. |

## chat (the composer)

| Action | Default | |
|---|---|---|
| `queue` | `ctrl+q` | Send the line as a follow-up while a turn runs. |
| `abort` | `esc` | Abort the running turn; clears a masked input. |
| `pop_queue` | `shift+up` | Take the last queued message back into the composer. |
| `cycle_thinking` | `shift+tab` | Next thinking level. |
| `toggle_thinking` | `ctrl+t` | Show or hide thinking blocks. |
| `toggle_expanded` | `ctrl+o` | Expand or collapse tool output. |
| `model_picker` | `alt+m` | |
| `edit_external` | `ctrl+g` | Edit the line in `$EDITOR`. |
| `reply` | `ctrl+r` | The reply picker. |
| `jobs` | `alt+j` | The jobs panel. |
| `job_cancel` | `alt+k` | Cancel the marked job. |
| `job_next`, `job_prev` | `alt+down`, `alt+up` | Mark the next or previous job. |
| `job_mark_cancel` | `k`, `x` | With a job marked: cancel it. |
| `job_unmark` | `esc` | With a job marked: take the mark off. |
| `history_prev`, `history_next` | `up`, `ctrl+p` / `down`, `ctrl+n` | Input history, from the first or last line. |
| `paste_image` | `ctrl+v` | |
| `complete` | `tab` | Take the ghost text or the selected `/` completion. |
| `accept_ghost` | `right` | At the end of the line: take the ghost text. |
| `menu_prev`, `menu_next` | `up`, `ctrl+p` / `down`, `ctrl+n` | Move in the `/` completion menu. |
| `menu_close` | `esc` | |

## editor (every text field)

| Action | Default | |
|---|---|---|
| `submit` | `enter` | Send the line in the composer, accept it in a field. |
| `newline` | `shift+enter`, `alt+enter`, `ctrl+enter`, `ctrl+j` | |
| `left`, `right` | `left`, `ctrl+b` / `right`, `ctrl+f` | |
| `word_left`, `word_right` | `alt+b` / `alt+f` | |
| `home`, `end` | `home`, `ctrl+a` / `end`, `ctrl+e` | |
| `up`, `down` | `up`, `ctrl+p` / `down`, `ctrl+n` | Between lines of the text. |
| `backspace` | `backspace`, `ctrl+h` | |
| `delete` | `delete`, `ctrl+d` | |
| `delete_word_back` | `alt+backspace`, `ctrl+w` | |
| `delete_word_fwd` | `alt+d` | |
| `kill_eol`, `kill_bol` | `ctrl+k` / `ctrl+u` | Cut to the end or start of the line. |
| `yank` | `ctrl+y` | Paste what the last cut took. |

## jobs (the jobs panel)

The list, on both of its tabs (Jobs and Sessions), is context `jobs`:

| Action | Default | |
|---|---|---|
| `close` | `esc`, `q` | Leave the panel. |
| `down`, `up` | `down`, `j`, `ctrl+n` / `up`, `k`, `ctrl+p` | Move the selection. |
| `top`, `bottom` | `home`, `g` / `end`, `G` | |
| `page_down`, `page_up` | `pagedown` / `pageup` | |
| `open` | `enter` | Jobs: open the transcript. Sessions: peek at the session. |
| `switch_tab` | `tab`, `shift+tab` | Between Jobs and Sessions. |
| `kill` | `x` | Jobs: cancel the selected running job. |
| `kill_all` | `X` | Jobs: cancel every running job (asks first). |
| `diff`, `apply` | `d` / `a` | Jobs: show or apply a finished job's result. |
| `message` | `n` | Sessions: type a message to the selected session. |
| `stop` | `s` | Sessions: stop the selected session's turn. |
| `terminate` | `S` | Sessions: terminate the session (asks first). |
| `take` | `t` | Sessions: take the session over. |

A transcript opened from either tab is context `jobs_view`:

| Action | Default | |
|---|---|---|
| `close` | `esc`, `q` | Back to the list. |
| `down`, `up` | `down`, `j`, `ctrl+n` / `up`, `k`, `ctrl+p` | Scroll a line. |
| `page_down`, `page_up` | `pagedown`, `ctrl+f` / `pageup`, `ctrl+b` | |
| `half_page_down`, `half_page_up` | `ctrl+d` / `ctrl+u` | |
| `top`, `bottom` | `home`, `g` / `end`, `G` | `bottom` also follows the output. |
| `follow` | `f` | Pause or resume following. |
| `kill` | `x` | Cancel the job being viewed. |

The message prompt (Sessions, `message`) is context `jobs_input`; it sends with `submit` from `editor`:

| Action | Default | |
|---|---|---|
| `cancel` | `esc` | Drop the message. |
| `backspace` | `backspace` | |
| `clear` | `ctrl+u` | |
| `delete_word_back` | `ctrl+w` | |

Confirmations (kill all, terminate) read context `jobs_confirm`; any other key cancels:

| Action | Default | |
|---|---|---|
| `confirm` | `y` | |

## reply

Counts (`3j`, `3[`) are typed digits, not binds.

| Action | Default | |
|---|---|---|
| `cancel` | `esc`, `ctrl+g` | Drop a pending count or selection, then close. |
| `close` | `q` | Close at once. |
| `quote` | `enter` | Quote the marks and selection (or the current unit) into the composer. |
| `mark` | `space` | Mark the current unit, or turn the selection into a mark. |
| `clear` | `c` | Clear the marks. |
| `up`, `down` | `up`, `k`, `ctrl+p` / `down`, `j`, `ctrl+n` | Previous or next unit. |
| `left`, `right` | `left`, `h`, `b`, `ctrl+b`, `alt+b` / `right`, `l`, `w`, `e`, `ctrl+f`, `alt+f` | Previous or next word. |
| `extend_up`, `extend_down` | `shift+up`, `K` / `shift+down`, `J` | Extend the selection by unit. |
| `extend_left`, `extend_right` | `shift+left`, `H`, `B` / `shift+right`, `L`, `W`, `E` | Extend the selection by word. |
| `page_up`, `page_down` | `pageup`, `alt+v` / `pagedown`, `ctrl+v` | |
| `extend_page_up`, `extend_page_down` | `shift+pageup` / `shift+pagedown` | |
| `half_up`, `half_down` | `ctrl+u` / `ctrl+d` | Half a page. |
| `top`, `bottom` | `home`, `alt+<` / `end`, `G`, `alt+>` | |
| `extend_top`, `extend_bottom` | `shift+home` / `shift+end` | |
| `goto_prefix` | `g` | Pressed twice, goes to the top. |
| `unit_start`, `unit_end` | `0`, `^`, `ctrl+a` / `$`, `ctrl+e` | First or last word of the unit. |
| `block_prev`, `block_next` | `{` / `}` | Previous or next block. |
| `prev_reply`, `next_reply` | `[`, `shift+tab`, `ctrl+alt+[` / `]`, `tab`, `ctrl+alt+]` | Older or newer reply (`ctrl+alt` is how some terminals report AltGr). |

## select (pickers, the ask dialog, overlays)

Text typed into a picker's filter, the free-text row or a note goes through the `editor` context, and Enter in those uses `submit` and `newline` from it.

| Action | Default | |
|---|---|---|
| `cancel` | `esc` | Close the picker; in the free-text row or a note, go back to the list. |
| `back` | `shift+tab` | Previous step of a multi-step dialog (the ask dialog's review, the permission prompt). |
| `back_empty` | `left` | The same, only while the filter is empty. |
| `move_up` | `up`, `ctrl+p` | Highlight the previous row. |
| `move_down` | `down`, `ctrl+n` | Highlight the next row. |
| `choose` | `enter` | Choose the highlighted row. |
| `note` | `tab` | Type a note on the highlighted row, where notes are offered. |
| `note_empty` | `right` | The same, only while the filter is empty. |
| `toggle` | `space` | Mark or unmark the row in a multi-select, while the filter is empty. |
| `context_up` | `pageup` | Scroll the context text above the rows up. |
| `context_down` | `pagedown` | Scroll it down. |

Shortcuts on a dialog's rows (`y`, `n`, digits and the letters some pickers give their rows) are not bindable.

### overlay

| Action | Default | |
|---|---|---|
| `approval_pattern` | `e` | In a permission prompt, type a pattern instead of picking a width. |
| `memory_revert` | `r` | In the memory history, revert to the highlighted version (while the filter is empty). |

## review (the document review, the model roles editor, the Sessions tab, the embedded terminal)

The model roles editor reads its keys through the `select` context, and the Sessions tab through `jobs`.

The document review (context `review`). Typing feedback uses the `editor` keys (`submit`, `backspace`, `kill_bol`).

| Action | Default | |
|---|---|---|
| `cancel` | `esc`, `ctrl+c` | Leave the review without deciding. |
| `feedback_back` | `esc` | While typing feedback: back to the action list. |
| `copy` | `c` | Copy the document. |
| `edit` | `ctrl+g` | Open the document in the editor (when it is a file). |
| `note` | `n` | On an action that takes an optional note: type it. |
| `toggle_focus` | `tab`, `shift+tab` | Move between the document and the actions. |
| `page_up`, `page_down` | `pageup` / `pagedown` | |
| `top`, `bottom` | `home` / `end` | |
| `scroll_up`, `scroll_down` | `up`, `k` / `down`, `j` | Document focused; scrolling down past the end moves to the actions. |
| `doc_top`, `doc_bottom` | `g` / `G` | Document focused. |
| `to_actions` | `enter`, `right`, `l` | Document focused: move to the actions. |
| `select_prev`, `select_next` | `up`, `k` / `down`, `j` | Actions focused; up from the first moves to the document. |
| `tier_prev`, `tier_next` | `left` / `right` | Actions focused: the model slider. |
| `slider_prev`, `slider_next` | `h` / `l` | The same, only while the slider shows. |
| `confirm` | `enter` | Take the selected action. |

The embedded terminal (context `terminal`); every other key goes to the command.

| Action | Default | |
|---|---|---|
| `kill` | `esc` | Kill the command and close the view. |

## wizards (login, custom providers, MCP)

| Action | Default | |
|---|---|---|
| `cancel` | `esc` | Give up on a sign-in, the API-key or command entry, or the MCP wizard; in the custom-provider panel, go back. |

Accepting a typed value uses the `editor` context's `submit`.
