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
  mkdir -p "$dest/mu/plugins/memory"
  (cd "$root/plugins/memory" && tar --exclude=./node_modules -cf - .) | (cd "$dest/mu/plugins/memory" && tar -xf -)
  cp "$root/upstream.toml" "$root/LICENSE" "$root/THIRD-PARTY-NOTICES.md" "$dest/mu/"
  cp "$root/scripts/install.sh" "$root/scripts/fetch-omp.sh" "$dest/mu/scripts/"
}
