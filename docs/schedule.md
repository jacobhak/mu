# Scheduled prompts

`/schedule <delay> <prompt>` sends the prompt when the delay is up, as if you had typed it then: it
appears in the transcript as your message, the model answers it in a turn of its own, and memory
review sees it like any other prompt. Use it to pick something up later without staying at the
keyboard.

```
/schedule 10m re-run the failing test and tell me what changed
/schedule 1h30m summarize what changed while I was away
/schedule list
/schedule cancel 2
```

The delay is a duration such as `90s`, `30m`, `2h`, `1d`, or several units run together, `1h30m` —
the same syntax as `[retry] max_wait` in [Config](config.md). There is no clock-time form: ask for
`2h`, not for `09:00`.

While a prompt waits, its line sits above the composer with the time left:

```
⏱ 1 · in 9m 30s · re-run the failing test and tell me what changed  /schedule cancel
```

`/schedule list` numbers the pending prompts, `/schedule cancel N` drops one and `/schedule cancel`
drops all of them. `/schedule` works while a turn runs.

A prompt that comes due while a turn is running waits for that turn to end — the row says `waiting
for the turn to end` — and goes out then, rather than interrupting a reply. The turn has ended once
the messages queued behind it have been answered too. A prompt that comes due while you are typing,
in a picker, or with an editor open is sent anyway and does not disturb what is on screen: a
scheduled prompt never lands in the composer.

The text is a prompt, not a command line: `/` commands, `!` shell commands and `/skill:` are not
interpreted, so `/schedule 20m /compact` sends the model the literal text `/compact`. Run a command
when you need it.

Schedules belong to the mu process, not to a session: quitting drops them (the transcript says
`N scheduled prompt(s) dropped`), they do not survive `--resume`, and a prompt fires into whichever
session is open when it comes due. A prompt that comes due with no model chosen is dropped with an
error rather than lost silently.

The turn a scheduled prompt starts is an ordinary turn: tools need the usual approvals and run under
the configured [permissions](permissions.md), and the terminal rings when the turn ends (see
[Detached runs](detached-runs.md#terminal-bell)). For a message from another process rather than a
timer, see `mu notify` in [Detached runs](detached-runs.md#messaging-a-session-mu-notify).
