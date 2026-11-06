# Secrets

mu replaces recognisable secrets with placeholders such as `[REDACTED:GEMINI_API_KEY]` before it stores or sends text. This covers tool results, your messages, plugin context such as recalled memories, full output saved to `artifact://` files, and sessions opened with `--resume` (old sessions may hold raw values). Nothing with a secret it recognises reaches the provider, the session file, memory or a subagent's result.

What counts as a secret:

- Environment variables whose name says they hold one (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*PASSWORD`, `*_PAT`, …), but not ones about one (`*_KEY_PATH`, `*_KEY_ID`, `SSH_AUTH_SOCK`). The value must be at least 8 characters and look like a credential rather than a word or path.
- The same for `.env` and `.env.*` files in the working directory (not `.env.example`, `.sample`, `.template`). Changes to them are picked up.
- The password in any `scheme://user:password@host` value of those variables.
- Keys in an unambiguous format, wherever they appear: GitHub (`ghp_`, `github_pat_`, …), Anthropic (`sk-ant-`), OpenAI (`sk-`), AWS access key ids, Google API keys (`AIza`), Slack (`xox?-`), Stripe live keys and private key blocks.
- Provider API keys you added to mu's key store (`mu auth`, `/login`), whatever their format (Baseten keys have none): the literal keys in `auth.json`, and the values of `$NAME` and `!command` references once resolved. They show as `[REDACTED:ANTHROPIC_API_KEY]`, `[REDACTED:OPENAI_API_KEY]` or `[REDACTED:BASETEN_API_KEY]`, so `cat ~/.mu/auth.json` run by the model comes back redacted. Provider error bodies pass through the same filter before they are shown or stored.
- ChatGPT sign-in tokens (the `chatgpt` section of `auth.json`): the access, refresh and ID tokens, as `[REDACTED:OPENAI_CHATGPT_TOKEN]`, `[REDACTED:OPENAI_CHATGPT_REFRESH_TOKEN]` and `[REDACTED:OPENAI_CHATGPT_ID_TOKEN]`.

Known values are matched exactly; there is no guessing from randomness, so hashes and ids are left alone.

When a tool result had secrets replaced, the model is told which placeholders it saw, that the values are hidden, and to use environment variables by name (`"$GEMINI_API_KEY"`). A `bash`, `edit` or `write` call that contains a placeholder mu handed out is refused with an explanation, since it would write the placeholder where the secret was. When your own message had a secret replaced, mu shows a notice.

Prevention comes first: the `bash` tool description tells the model never to print secret values, to check a variable with `[ -n "$NAME" ]` or `${#NAME}` instead, to keep tokens out of verbose output, and to pass secrets by name. The system prompt explains the placeholders, so the model does not treat them as errors.

Every replacement is recorded in the session as a `secrets_redacted` entry with where it happened (`tool:bash`, `message`, `plugin:memory`, `resume`, `user-bash`) and the placeholder names, never the values. Counting them shows how often the filter has to step in:

```sh
jq -c 'select(.customType == "secrets_redacted") | .data' ~/.mu/sessions/*/*.jsonl
```

This guards against a careless model printing a key (`env`, `cat .env`, a token in a git remote). It is not a boundary: anything with bash can encode or move a value without mu seeing it. Keep credentials the agent should not have out of the account it runs as.

Redaction cannot be turned off. The only setting adds variables to treat as secrets:

```toml
[secrets]
env = ["MY_ODD_NAME"]   # extra variables whose values count as secrets, whatever their name
```
