#!/usr/bin/env bash
# stage_layout <repo-root> <release-dir> <dest-dir>: lays out the mu distribution under <dest-dir>/mu.
stage_layout() {
  local root="$1" rel="$2" dest="$3" p
  rm -rf "$dest/mu"
  mkdir -p "$dest/mu/bin" "$dest/mu/plugins" "$dest/mu/scripts"
  for p in mu mu-gateway mu-web mu-mcp-http mu-predict mu-memory; do
    cp "$rel/$p" "$dest/mu/bin/$p"
  done
  for p in gateway predict memory; do
    mkdir -p "$dest/mu/plugins/$p"
    cp "$root/plugins/$p/plugin.toml" "$dest/mu/plugins/$p/"
  done
  mkdir -p "$dest/mu/plugins/gateway-pi"
  cp "$root/plugins/gateway-pi/plugin.toml" "$root/plugins/gateway-pi/proxy.mjs" "$root/plugins/gateway-pi/translate.mjs" "$dest/mu/plugins/gateway-pi/"
  mkdir -p "$dest/mu/plugins/web"
  cp "$root/plugins/web/plugin.toml" "$dest/mu/plugins/web/"
  cp -R "$root/plugins/web/schemas" "$dest/mu/plugins/web/schemas"
  cp -R "$root/plugins/memory/schemas" "$dest/mu/plugins/memory/schemas"
  cp "$root/upstream.toml" "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$dest/mu/"
  cp "$root/scripts/install.sh" "$root/scripts/fetch-omp.sh" "$root/scripts/fetch-pi.sh" "$dest/mu/scripts/"
}
