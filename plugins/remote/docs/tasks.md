# Tasks on a remote

With mu's `[task] launcher = ["mu-remote", "launch", "NAME"]` and `[task] launch = ["task"]` (see mu's `docs/remote.md`, "For extension authors"), mu runs `task` subagents through `mu-remote launch NAME` instead of as a local `mu --headless`; scouts and kinds not listed stay local. To mu the launcher is a child: its stdin and stdout carry the subagent stream as a local child's would, and it reads the task's environment (`MU_TASK_*`). The remote's adapter only moves bytes and runs processes; the launcher does the rest.

## The launcher's flow

1. **Admit.** The [trust](trust.md) check runs first. A refusal prints the reason on stderr, exits 1 and starts nothing.
2. **Start.** mu has committed the working tree, untracked files included, as `refs/mu/task/<id>` and written it with its full history as a git bundle (`MU_TASK_BUNDLE`). The launcher runs the adapter's `start --kind worker --launch $MU_TASK_NAME --bundle $MU_TASK_BUNDLE --dir /work/$MU_TASK_NAME`. The adapter makes a git checkout of the snapshot in that directory and runs `mu-remote worker run --launch <id> --dir <dir> --detach` there, which prints `{"id":"<id>"}` once it listens. The adapter's progress lines go to the launcher's stderr as `{"progress":"…"}` lines for mu's jobs panel.
3. **Link.** The launcher connects through the adapter's `connect <id>` and the [worker link](channel.md#the-worker-link). mu's first stdin line, the task payload, goes to the worker, which runs `mu --headless` on it in the checkout and keeps running when the connection drops. The worker's model calls and searches go through your mu session (the worker holds no credential). Later stdin lines (approval and relay answers) follow, and the child's stdout lines come back, to mu's stdout, plain.
4. **Result.** When the worker's child exits, the worker commits what it changed, untracked files included, as `refs/mu/result/<id>` on top of the checkout's commit, and sends its `exit` line. The launcher runs `fetch <id>` (`mu-remote worker bundle <id>`), writing into `$MU_TASK_RESULT_BUNDLE`: a git bundle of that ref on top of the snapshot, or an empty file when the worker changed nothing. It then runs `stop <id>`. A result that cannot be fetched is left on the remote, the worker is not stopped and a note goes to stderr, except when the task was cancelled. If the child's stderr has text and no `result` line was seen, the launcher writes that text to its stderr so mu shows it as the error.
5. **Exit.** The launcher exits 0; non-zero only when it could not start or was refused. mu then imports the result bundle (it checks that it holds exactly `refs/mu/result/<id>` and descends from the snapshot), applies it when every file it touches is unchanged since the snapshot, and otherwise keeps the ref and says how to apply it (the jobs panel offers the diff and the apply).

Nothing but the child's own lines goes to the launcher's stdout.

## Cancel

mu sends `SIGTERM` to the launcher's process group and `SIGKILL` 30 seconds later. The launcher stops its worker (committing first), fetches what it can into the result bundle, and exits. A cancelled task leaves its changes unapplied in mu.

## A lost stream

mu never sees a reconnect: to mu the stream is one pipe. The launcher reconnects with growing pauses (0.25 s up to 10 s) and the [numbering, acknowledgement and replay](channel.md#the-worker-link) make the stream lossless. After `reconnect_timeout` seconds (remote config, default 600) without a stream the launcher stops, fetches what the worker had committed and finishes; the task then fails with `lost the stream to worker ID for Ns: <last error>`. Once that fetch succeeded it also runs `stop`. A worker that never gets its task exits after 30 minutes.

## On the remote

The worker's record is `<state>/workers/<id>/state.json` under the remote's `mu-remote` state directory; `mu-remote worker bundle ID` and `mu-remote worker stop ID` read it. They are for adapters, not for people.
