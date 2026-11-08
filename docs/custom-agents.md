# Custom agents

Custom agents are omp-format `.md` files in `.omp/agents` and `<agentDir>/agents` (frontmatter `name`, `description`, `tools`, `model` as a list of `@role`/`provider/model`, `thinkingLevel`). `mu -p PROMPT --agent NAME` runs one; they also appear in the `task` tool.

mu ships the two agents a `task` call can name itself: `task` (a worker with every tool the run offers) and `scout` (read-only: `read`, `grep`, `glob`, `web_search`, `fetch`). A file named `task`, `scout`, `main`, `sub` or `review-lead` is ignored, because those names are reserved.
