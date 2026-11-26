# The encrypted channel

What a client and an agent (or a launcher and a worker) say to each other is encrypted end to end, on top of whatever carries it: `ssh`, `kubectl exec`, the Kubernetes API server and every hop in between see only ciphertext. The channel is Noise `XX` (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, from the pure-Rust `snow` crate).

## Where it applies

```
mu UI --plain JSON lines--> mu-remote connect --sealed--> adapter connect (ssh / kubectl exec) --sealed--> mu-remote bridge --plain--> mu attach --stdio --> agent
```

- **mu's `--attach` stream is plain.** `mu --attach '<command>'` is a client of whatever `sh -c <command>` pipes: plain JSON lines, the `hello` first. mu does not encrypt it and reruns the command with backoff when it drops. It is `mu-remote connect` that seals the stream (`mu-remote NAME` runs `mu --attach 'mu-remote connect NAME ID'`), and `mu-remote bridge` on the far side that opens it. mu guarantees only the plain stream; the encrypted channel is this program's addition.
- **Why the channel ends next to the agent.** Secrets cross the plain stream: the `save_key` and `chatgpt_paste` host calls, `program_input` (what you type into `/login`) and `state_unlock` (the key that unlocks a sealed agent). Only the machine the agent runs on may see them in the clear, so `bridge` opens the channel on that machine and pipes plain lines to `mu attach --stdio ID` (a raw pipe to the agent's private socket). Nothing between `connect` and `bridge` sees them.
- **Workers.** For a worker's launch id served on the same machine, `bridge` does not open the channel at all: it copies raw lines to and from the worker's socket, and the worker (see "The worker link" below) ends its own channel. The launcher and the worker are the two ends; the bridge and the adapter only carry ciphertext.

## Handshake

1. The initiator (`mu-remote connect`, or a launcher) sends the first Noise message.
2. The responder (`mu-remote bridge`, or a worker) answers with the second, which carries its static key. The initiator compares that key with the one it pinned (below) and, only if it is the pinned one or the first, sends the third message, which carries its own static key. An initiator that sees another key stops there, before it has told the responder anything.
3. Every later line, both ways, is inside the channel. There is no plaintext line: the first line mu sends (its `hello`) is sealed.

On the wire each Noise message is one line, a JSON string holding its base64, so an adapter's `connect` carries it as it carries any line. A line longer than a Noise message (65 535 bytes) is cut into chunks and put together on arrival, within a line limit of 64 MiB. A line that does not decrypt, repeated, dropped or out of order, ends the connection; a reconnect starts a new handshake with new session keys.

## Keys and pinning

`mu-remote bridge` has a static key, made on first use in `<state>/keys/bridge.key` (mode 0600, in a 0700 directory); every agent served by that machine's `mu-remote` shows it. A client's static key is `<state>/keys/client.key`. `mu-remote key` prints the bridge key's SHA-256 fingerprint (`SHA256:…`, as ssh shows host keys).

`mu-remote connect` pins the key it sees per remote name and agent id, in `<state>/keys/pins.json` (never in `config.toml`). On the first contact it prints, on stderr,

```
first contact with agent ID on NAME: its key is SHA256:…, now pinned
```

so you can compare it with `mu-remote key` run on the remote another way (the remote's own terminal, say). A later connection to that agent id with another key is refused with a message that names both fingerprints and the file to edit if the machine's state was reset on purpose, and nothing is told to the agent. A new agent id is a new pin: agents served by one machine share a key, so their fingerprints match.

For tasks the keys exist only in memory: the launcher makes a new key for every task and the worker makes its own when it starts. The launcher pins the worker's key from the first connection and refuses a different one later; the worker pins the launcher's key likewise.

## What it does not do

- On the first contact the responder's key arrives over the same transport as everything else. An active man in the middle of that transport can sit in the first handshake and show its own key; compare the fingerprint (`mu-remote key`) the first time, and after pinning such a man cannot come back. For a worker nothing carries its key to the launcher out of band, so the first handshake trusts the transport: the encryption keeps the transport's middle hops from reading the task and the model traffic, and it does not hide them from whoever runs the node.
- It does not hide the stream from root on the node the pod runs on, or from the agent's own machine, which has the plaintext.
- The bridge key is not a credential: it names the machine's agents to their clients and gives no access to anything by itself, but whoever can read `<state>/keys/bridge.key` can pose as that machine to a client that has not pinned it.
- It does not authenticate you to the agent: who may reach the agent is who may run `mu-remote bridge` there (an ssh login, `kubectl exec`).
- mu does not encrypt anything itself, and a `mu --attach` run by hand with another command is plain.

## The worker link

The launcher (`mu-remote launch`) and the worker (`mu-remote worker run`) talk through the adapter's `connect ID` → `mu-remote bridge ID` → the worker's Unix socket (mode 0600 in a 0700 directory under the state directory). Every connection starts with the handshake above, then plain JSON lines inside the channel.

- **Resume.** Each side's first line on every connection is `{"type":"resume","after":N}`, N being the highest `seq` of the other side's numbered lines it has received without a gap (0 at the start). It also acknowledges those lines.
- **Numbering.** Numbered lines are JSON objects with `"seq":k` (k = 1, 2, … per direction; a line that is not a JSON object is wrapped as `{"type":"stray","text":…}` first). Launcher to worker: seq 1 is mu's first stdin line, the task payload (the worker starts `mu --headless` when it gets it), then every later line mu sends (approval and relay answers). Worker to launcher: every line the child `mu --headless` writes to stdout, and last `{"type":"exit","code":N,"stderr":"<tail of the child's stderr>"}`.
- **Keeping and replaying.** Each side keeps the numbered lines it sent until they are acknowledged (`{"type":"ack","seq":N}`, sent after every 64 received numbered lines and when a connection ends cleanly) and, on a reconnect, replays everything after the peer's `resume.after`. The worker keeps the newest 16 MB in memory and spills older lines to a private file; the launcher keeps its lines in memory.
- **Duplicates and gaps.** A receiver drops a line whose seq is not above what it already has, and drops the connection on a gap (seq above last+1), so the other side replays. The receiver strips `seq` before passing the line on: the launcher writes the plain child line to mu's pipe, the worker writes mu's plain line to the child's stdin. So no line is lost or seen twice, and mu's relay sees each call once.
- **Reconnecting.** While the stream is down the launcher reconnects with growing pauses (0.25 s up to 10 s). After `reconnect_timeout` seconds (a remote's config key, default 600) without a stream it gives up: it fetches what the worker has committed and finishes, writing `lost the stream to worker ID for Ns: <last error>` to stderr. A key mismatch (the worker's key is not the one it showed first) ends the task at once. The worker keeps running when the stream drops; one that never gets its task exits after 30 minutes.
