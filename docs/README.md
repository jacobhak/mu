# mu docs

- [Using mu](usage.md): command line, models and thinking, steering, `/copy`, images
- [Reply picker](reply-picker.md): quote parts of a reply into your answer
- [Durable plans](durable-plans.md): park, list, refine and approve plans later; `mu plan`, `/deliver`
- [Background jobs](background-jobs.md): subagents that report back when done; the jobs panel (`Alt+J`) for jobs and other mu sessions
- [Detached runs](detached-runs.md): interrupting `-p`, `mu take`, `mu notify`
- [Scheduled prompts](schedule.md): send a prompt after a delay (`/schedule`)
- [Headless review](review.md): `mu review`
- [Ghost text](ghost-text.md): prompt prediction and the opt-in SmolLM engine
- [MCP servers](mcp.md)
- [Agent Client Protocol](acp.md): `mu --acp`
- [Remote use](remote.md): `mu agent`, `mu -r`: the agent on one machine, the terminal UI on another
- [Remote use on Kubernetes](remote-kubernetes.md): a generic example of the main agent in a trusted cluster
- [Providers](providers.md): API keys for Anthropic, OpenAI and Baseten, custom vLLM/OpenAI-compatible endpoints (`mu providers`), Sign in with ChatGPT, `mu auth`, `/login`, routing and thinking per provider, what still needs omp
- [Model roles](model-roles.md): what each role is for, and where to see and set them
- [Custom agents](custom-agents.md)
- [Memory](memory.md)
- [Secrets](secrets.md): what mu redacts before storing or sending text, and what it does not protect against
- [Tool permissions](permissions.md): modes (`ask`, `edits`, `yolo`), rules, and the approval prompt
- [Config](config.md): `~/.mu/config.toml`
- [mu documents itself](self-documentation.md): `mu://`, these pages and generated references for the agent to read and search

Design and measurements: [`DESIGN.md`](../DESIGN.md). Ghost-text evaluation record: [`ghost-text/`](ghost-text/README.md).
