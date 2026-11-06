# mu documents itself

mu carries its own documentation, so an agent (or you) can learn what the running mu does without reading its source on GitHub. The pages ship inside the binary and match its version.

They live under `mu://`, which the file tools understand:

```
read mu://                       # the index
read mu://config.md              # a page; selectors work: mu://config.md:1-40
grep "modelset" mu://            # search every page; rows are mu://page.md:LINE:text
glob "reference/*.md" mu://
```

`mu://` is read-only. The first use writes the pages to `<mu home>/docs/<stamp>/` (`~/.mu/docs`), where the file tools read them; a new version of mu replaces the directory.

## What is there

- The pages in [docs/](README.md), plus `overview.md` (the README) and `design.md` (DESIGN.md).
- `reference/`: generated from the code, so it cannot drift: `tools.md` (every built-in tool and its parameters), `cli.md` (every subcommand and flag, as `mu --help` prints them), `interactive.md` (slash commands and key bindings).
- `state/`: this session, rewritten each time it is read: `session.md` (version, directories, session id and file), `config.md` (`config.toml` and `models.toml` as they are), `plugins.md` (plugins, MCP servers and their tools), `context.md` (context files, skills, prompt commands and custom agents loaded). `state/` is not part of a search of `mu://`; search `mu://state` itself.

The system prompt tells the model about `mu://` in one line, so asking mu "how do I set the warn colour?" or "which MCP servers are on?" is answered from these pages rather than from memory.

## Adding a page

Put a Markdown file in `docs/` and list it in `PAGES` in `crates/mu-tools/src/tools/docs.rs` (a test fails until you do), and in `docs/README.md`. For a page generated from code, call `mu_tools::tools::docs::register("reference/<name>.md", fn)` at startup.
