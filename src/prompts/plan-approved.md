Plan approved.
{{context}}
<instruction>
Full plan inlined below; durable copy at `{{path}}` (identical content).
Execute plan step-by-step with full tool access; MUST verify each step before next.
NEVER re-read `{{path}}` while the inline plan is intact; the path is for subagent handoff and recovery only.
Track progress with `todo` when available.
</instruction>

<plan path="{{path}}">
{{plan}}
</plan>

<critical>
Inline plan compressed, expired, or unrecoverable: NEVER stop; read `{{path}}`.
Read failure: report exact path and error; NEVER guess.
MUST continue until complete.
</critical>
