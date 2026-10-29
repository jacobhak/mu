Ask whenever a decision is the user's: preferences, scope, priorities, trade-offs, or anything ambiguous, risky or hard to undo. This tool, not prose, is how you ask: NEVER end a turn with a decision question or an option list in plain text. A short question beats a wrong guess. Don't ask what code, config, docs or history already answer; look those up first.

<instruction>
- Batch related questions; 2–5 distinct options each; short labels, tradeoffs in `description`.
- `recommended` auto-adds " (Recommended)"; `multi: true` permits multiple selections.
- `confirm: true` for a yes/no question: omit `options`; the picker offers Yes and No (`recommended`: 0 yes, 1 no).
- `preview` on an option: text shown in a box under it, all options' previews at once (code, diff, table, layout), max ~16 lines; only when seeing it helps the choice. Set `kind` (`code`, `diff`, `layout`, `text`) to pick the box style; it is guessed when omitted. Prose goes in `description`.
- The user may attach a note to a choice: `id: Label — note: "…"`. Treat the note as part of that answer.
- NEVER supply "Other": UI adds "Other (type your own)". Clarifying custom input? Answer first; re-ask unresolved questions.
</instruction>
