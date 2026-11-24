# gateway-pi

A mu gateway plugin (`provider = "gateway"`) that reaches AI providers through the [pi coding agent](https://github.com/earendil-works/pi). It is the `pi` backend of the `mu-gateway` binary (`mu-gateway --backend pi mu-plugin`, see [`plugins/gateway`](../gateway/README.md) for the daemon, state directory and CLI), and it is chosen with `[gateway] plugin = "gateway-pi"` in `~/.mu/config.toml`.

Pi has no gateway server, so the backend runs `pi --mode rpc --no-session -e proxy.mjs`. [`proxy.mjs`](proxy.mjs) is a pi extension: an HTTP proxy on loopback that serves mu's gateway contract (Anthropic Messages at `/v1/messages`, `/v1/models`, `/v1/usage`, `/healthz`) over pi's own model registry, so pi's logins, token refresh, catalog and streaming do the work. [`translate.mjs`](translate.mjs) holds the pure Anthropic to pi-ai translation, tested with `node --test plugins/gateway-pi/test/*.test.mjs`.

Install pi with `scripts/install.sh --with-pi` (the pinned version is `pi` in `upstream.toml`) and log in with `pi auth login`.
