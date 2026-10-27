#!/usr/bin/env bash
# stage_layout <repo-root> <release-dir> <dest-dir>: lays out the mu distribution under <dest-dir>/mu.
stage_layout() {
  local root="$1" rel="$2" dest="$3" p
  rm -rf "$dest/mu"
  mkdir -p "$dest/mu/bin" "$dest/mu/plugins" "$dest/mu/scripts"
  for p in mu mu-gateway mu-web mu-predict; do
    cp "$rel/$p" "$dest/mu/bin/$p"
  done
  for p in gateway predict; do
    mkdir -p "$dest/mu/plugins/$p"
    cp "$root/plugins/$p/plugin.toml" "$dest/mu/plugins/$p/"
  done
  mkdir -p "$dest/mu/plugins/web"
  cp "$root/plugins/web/plugin.toml" "$dest/mu/plugins/web/"
  cp -R "$root/plugins/web/schemas" "$dest/mu/plugins/web/schemas"
  # The memory plugin is two binaries: `mu-memory`, a small Rust shim that starts the per-MU_HOME shared daemon
  # on demand and relays stdio to it, and `mu-memory-daemon`, a `bun build --compile` binary, so installs need no Bun.
  # Without bun at build time it is left out, and install.sh keeps whatever memory plugin is installed.
  if command -v bun >/dev/null; then
    (cd "$root/plugins/memory" && bun install --frozen-lockfile >&2 && bun build --compile --outfile "$dest/mu/bin/mu-memory-daemon" main.ts >&2)
    cp "$rel/mu-memory" "$dest/mu/bin/mu-memory"
    mkdir -p "$dest/mu/plugins/memory"
    cp -R "$root/plugins/memory/schemas" "$dest/mu/plugins/memory/schemas"
    sed 's|^command = .*|command = ["mu-memory"]|' "$root/plugins/memory/plugin.toml" > "$dest/mu/plugins/memory/plugin.toml"
  else
    echo "memory plugin not packaged: bun not found" >&2
  fi
  cp "$root/upstream.toml" "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$dest/mu/"
  cp "$root/scripts/install.sh" "$root/scripts/fetch-omp.sh" "$dest/mu/scripts/"
}
