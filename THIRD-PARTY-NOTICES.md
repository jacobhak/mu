# Third-party notices

mu is MIT licensed (see `LICENSE`). It includes material from these projects.

## oh-my-pi

<https://github.com/can1357/oh-my-pi>, release v18.12.6 (commit 45c35e4). mu reimplements much of omp's
behaviour in Rust and copies or adapts this material from it:

- `src/prompts/*.md`: compaction, summarisation and plan-mode prompts; `crates/mu-tools/src/prompts/*.md`: `ask` and `todo` prompts
- `plugins/memory/prompts/*.md`: memory extraction and Mnemopi instructions
- parts of the tool descriptions in `crates/mu-tools/src/tools/`
- `design/theme/omp-dark-tokyo-night.json`: an omp theme, kept as a design reference

mu also depends on omp's Rust crates (`pi-edit`, `pi-shell`, `pi-diff`, `pi-walker`), the
`@oh-my-pi/pi-mnemopi` package and the `omp` CLI. Those are fetched, not vendored, and carry their own licenses.

```
MIT License

Copyright (c) 2025 Mario Zechner
Copyright (c) 2025-2026 Can Bölük
Copyright (c) 2026 Stencil Labs, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## omarchy

<https://github.com/basecamp/omarchy>. The colour palettes in `src/tui/palettes/*.toml` are copied from
omarchy's `themes/*/colors.toml`. Their license is in `src/tui/palettes/LICENSE`.

## rmcp

<https://github.com/modelcontextprotocol/rust-sdk>, version 3.8.0, Apache-2.0. The `mu-mcp-http` bridge
(`plugins/mcp-http`) uses its Streamable HTTP client transport. No other part of mu uses it.
