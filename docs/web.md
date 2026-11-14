# Web search and fetch

The `web` plugin (`mu-web`) gives the model `web_search` and `fetch`. Neither runs omp.

## Search: you choose where queries go

`web_search` does nothing until you choose a backend. Exactly one is used at a time; a failing backend never falls back to another, so nothing you did not choose is contacted. The model writes the query; your files and conversation are not sent.

| Backend | Needs | Notes |
|---|---|---|
| Parallel (no key) | your approval | Parallel's free endpoint; lower rate limit |
| Parallel (API key) | `mu auth add parallel` | billed per request by Parallel |
| Exa (no key) | your approval | Exa's free endpoint; unpublished rate limit |
| Exa (API key) | `mu auth add exa` | $10 of free credits a month, then per use |
| Anthropic web search | an Anthropic key | runs `claude-haiku-4-5`; billed per search and token |
| OpenAI web search | an OpenAI key | runs `gpt-5.5`; billed per search call and token |
| Off | nothing | `web_search` returns an error, nothing is contacted |

Parallel and Exa say in their terms that queries may be used to train their models; `/web` and `mu web status` show the full text for the backend. Approving a keyless endpoint is stored once per profile (with the date) and removed by choosing another backend, or by `mu web reset`.

An Anthropic or OpenAI key used for search is not used for chat models unless you turn that on (`/login`, or `mu auth use <provider> --chat`).

### First use

In the TUI, the first time the model calls `web_search` with nothing chosen, the `/web` wizard opens and the call waits. Pick a backend; a keyless one shows its terms and asks for approval, a keyed one asks for the key first (saved for search only). Cancelling tells the model search was not set up. `/web` opens the same wizard any time.

Headless runs (`mu -p`, subagents, ACP) cannot ask: `web_search` returns an error that names `mu web use`.

```sh
mu web status
mu web use exa                       # a stored key
mu web use parallel --keyless --yes  # approve Parallel's free endpoint without a prompt
mu web use off
mu web reset                         # ask again next time
```

The choice lives in `auth.json` under `web` (see [providers.md](providers.md)). The plugin never reads the store: for each search it asks mu (`core/webSearch`), which answers only to the web plugin and hands over the one key that backend needs.

## Fetch

`fetch` loads the URL itself (no remote reader, no other third party): redirects are followed by hand with at most 10 hops, and every hop's addresses are checked and pinned, so loopback, link-local (cloud metadata), `0.0.0.0/8`, Alibaba Cloud's metadata address `100.100.100.200` and multicast addresses are refused, including through redirects and DNS rebinding (`MU_WEB_ALLOW_PRIVATE=1` lifts this). LAN addresses (10/8, 172.16/12, 192.168/16, fc00::/7) and 100.64/10 (Tailscale) are reachable only when the URL you (or the model) pass to `fetch` is itself on the LAN: a redirect from a public site, or a link taken from a public page (a markdown alternate, `llms.txt`), is refused if it lands on one, so a malicious page cannot aim the model at your router or NAS. A LAN page may redirect and link within the LAN, but once a chain leaves it, it cannot come back. For HTML it tries, in order: a page-declared markdown alternate, the `.md` suffix, content negotiation for markdown, a native HTML-to-markdown conversion, and `llms.txt`; the output header says which one produced the text (`Method:`). JSON, plain text and GitHub URLs (files, trees, repos, commits, issues and pull requests, using `GITHUB_TOKEN` or `GH_TOKEN` when set, sent only to GitHub) are handled natively. Feeds, images, PDFs, office documents and archives are not converted.
