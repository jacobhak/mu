# mu

> I love omp, but in the age of AI, RAM ain't cheap and every session being the equivalent of a chrome tab simply isn't sustainable.

mu is a terminal coding agent with a Rust core (agent loop, TUI, sessions) built on the crates of [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp), and borrowing many of its prompts and much of its behaviour. Credit for the design goes to the omp authors.

## How mu is different

mu keeps most of what omp does and changes where the memory goes. A session holds your conversation, and anything else runs only while it's being used.

- **Small core.** A native binary holds the agent loop, the TUI and the built-in tools. An idle session holds about 3.4 MB of private memory (about 24 MB counting the binary's shared pages).
- **Features start on use.** Web, memory and prediction are separate processes, and so is the optional model gateway (a [gateway provider plugin](docs/providers.md#gateway-plugins)). Each starts on first use and exits after a few idle minutes. A session that talks to a provider with your own key or ChatGPT sign-in starts no gateway at all (see [Providers](docs/providers.md)). MCP servers start on their first tool call; their tool lists are cached so mu can advertise the tools without starting them.
- **Shared across sessions.** When a gateway is needed, sessions on one omp profile share one gateway and its broker (about 275 MB total, 199 MB private, idle; measured with `scripts/measure-gateway.sh`). [`BENCHMARKS.md`](BENCHMARKS.md) shows what that means per session.
- **Bulky content goes to disk.** Large tool output and images are written to files instead of being kept in memory.
- **Works with your omp files.** mu reads omp's context files, skills and MCP config, and writes sessions in omp's format, so you can use both and switch either way. Model roles and limits are mu's own (`mu models import` copies omp's or pi's). You don't need an existing omp setup, and the installer fetches omp only when you ask (`--with-omp`, for the gateway).

The cost: features that live inside omp's runtime, such as TypeScript extensions, aren't available. The reasoning and measurements behind the design are in [`DESIGN.md`](DESIGN.md).

## Beyond omp

mu also adds a few things omp doesn't have:

- **[Reply picker](docs/reply-picker.md).** `Ctrl+R` lets you move through the last reply by sentence, list item or code line, with vim or emacs keys, and mark parts of it. The marked parts are quoted into your answer, so you can reply point by point.
- **[Durable plans](docs/durable-plans.md).** A plan doesn't have to be approved in the session that wrote it. You can park it, list it and refine it, then approve it later in the TUI or headless (`mu plan`), or hand it to `/deliver` to be implemented, reviewed and committed in its own worktree. When you open a parked plan, mu tells you which of the files it was based on have changed since.
- **[Your own modes](docs/modes.md).** A mode is a markdown file: a prompt, a model, a tool policy and an exit step with approval actions. Plan mode is just the built-in one, and you can override it or add others without writing code.

## Status

Early and experimental: started in October 2026 and changing quickly.

- No stable version. Releases are builds of `main`, tagged `mu-YYYYMMDD-<sha7>` and published once `main` passes the checks; config, flags and plugin interfaces can change without notice.
- Providers: Anthropic, OpenAI and Baseten work with an API key you add in mu (`/login`, `mu auth add <provider>`), and OpenAI models also run on your ChatGPT plan (`/login`). Neither involves a gateway. Every other provider, and subscription logins, go through the `mu-gateway` plugin that ships with mu, which needs the omp CLI (`install.sh --with-omp`, about 300 MB; sign in from `/login`). Details in [Providers](docs/providers.md).
- Prebuilt for Linux x86_64 and macOS arm64; other platforms are untested.
- No support promised. Issues and PRs are welcome but may go unanswered.

## Build and install

Releases (Linux x86_64, macOS arm64) are published from `main`:

```sh
curl -fsSL https://raw.githubusercontent.com/jacobhak/mu/main/scripts/install.sh | bash
curl -fsSL https://raw.githubusercontent.com/jacobhak/mu/main/scripts/install.sh | bash -s -- --tag mu-YYYYMMDD-<sha7>
scripts/install.sh --from-source .            # build and install a checkout
```

The installer verifies the download against the release's checksums and, where available, its build attestation. It puts `mu`, `mu-remote`, `mu-gateway` and the plugins under `~/.mu` and links `mu`, `mu-remote` and `mu-gateway` into `~/.local/bin`. `mu-remote`: remotes (agents and tasks on other machines); see [plugins/remote/README.md](plugins/remote/README.md). A private fork needs an authenticated `gh` for `install.sh` and `mu update`.

Then start `mu` and run `/login` to add a key or sign in with ChatGPT (see Status above for which providers need what). For the gateway, install with `--with-omp` (`curl … | bash -s -- --with-omp`). Web search and fetch need no gateway, and MCP OAuth is done by mu itself (`/mcp login <name>`).

### Updating

`mu update` installs the latest release; `mu update --check` only reports. The TUI checks for a newer release once a day and shows a notice; turn that off with `[update] check = false` in config.toml.

### Manual build

```sh
cargo build --release --workspace        # target/release/{mu,mu-gateway,mu-web,mu-predict,mu-memory,mu-remote}
scripts/build-mcp-http.sh                # target/release/mu-mcp-http (see docs/mcp.md)
mkdir -p ~/.mu/plugins
for p in gateway web memory predict; do ln -sfn "$PWD/plugins/$p" ~/.mu/plugins/$p; done
```

Plugins are found in `~/.mu/plugins/*/plugin.toml`, and their binaries are resolved next to the `mu` binary. The gateway's source is in [`plugins/gateway`](plugins/gateway/README.md).

## Documentation

Usage, configuration and the individual features are documented in [`docs/`](docs/README.md).

## Development

### Tests

`scripts/ci-checks.sh` runs everything CI does (fmt, shellcheck, cargo-deny, clippy, the tests); run it before pushing. `scripts/smoke.sh` and `scripts/smoke-native.sh` are opt-in live end-to-end checks, and `scripts/measure-idle.sh target/release/mu` reports idle memory.

### Maintenance bot

A local systemd timer keeps mu current with oh-my-pi and publishes the releases; see [`scripts/maint`](scripts/maint/README.md).

## License

MIT, see `LICENSE`. mu includes prompts and other material from [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT) and colour palettes from [omarchy](https://github.com/basecamp/omarchy) (MIT); see `THIRD-PARTY-NOTICES.md`.
