# Reply picker

`Ctrl+R` or `/reply` opens a reply picker on the newest finished assistant text (it works during a turn and shows the last finished reply, not the streaming one). It uses all the height the screen leaves and is framed by a titled rule above and a plain rule below. `[` and `]` switch to older and newer replies.

## Moving

Up/Down move between sentences, list items, headings, table rows and code lines; Left/Right move word by word; PgUp/PgDn, Home/End jump.

Vim and Emacs keys work too: `j k h l`, `w b e` (word), `0 ^ $` (start and end of the unit), `{ }` (previous and next block), `gg G`, Ctrl+D/Ctrl+U (half page), counts such as `5j`, `q` to close; Ctrl+N/P (unit), Ctrl+F/B and Alt+F/B (word), Ctrl+A/E (start and end of the unit), Ctrl+V/Alt+V (page), `Alt+<`/`Alt+>` (top and bottom), Ctrl+G (cancel).

## Marking and quoting

Space marks the current unit. Shift with an arrow (or H/J/K/L) selects: Shift+Left/Right by word, Shift+Up/Down to the edge of the unit and then by unit; Space turns the selection into a mark, Esc drops it, a plain move drops it too. `c` clears the marks.

Enter quotes the marks plus any selection (or the current unit when there is neither) as `> ` blockquotes at the end of the composer and puts the cursor on the row right under the first quote; the quote keeps its structure (blank `>` lines between paragraphs, list markers, code lines in a fence) so it renders as markdown. As in chat apps, a quote ends at the first line without `>`: the transcript shows your comment under it, not inside it, though the model gets the plain text. Esc closes without a change.
