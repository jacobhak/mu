# Custom agents

Custom agents are omp-format `.md` files in `.omp/agents` and `<agentDir>/agents` (frontmatter `name`, `description`, `tools`, `model` as a list of `@role`/`provider/model`, `thinkingLevel`). `mu -p PROMPT --agent NAME` runs one; they also appear in the `task` tool.
