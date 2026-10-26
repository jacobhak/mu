#!/usr/bin/env bash
# Usage: setup.sh [--profile NAME]   Idempotent. Sets up the maintenance bot on this machine.
set -euo pipefail
profile=private
while [ $# -gt 0 ]; do
  case "$1" in
    --profile) profile="${2:?--profile needs a name}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAINT="${MAINT:-$HOME/.mu/maint}"
CLONE="${CLONE:-$MAINT/mu}"
REPO=jacobhak/mu
mkdir -p "$MAINT"

[ -d "$CLONE/.git" ] || git clone "https://github.com/$REPO.git" "$CLONE"
# Private gitconfig for the bot (the unit sets GIT_CONFIG_GLOBAL to it), so a
# user-level url.insteadOf https->ssh rewrite can't make unattended runs hang on ssh-agent.
email="$(git config --global user.email || true)"
cat > "$MAINT/gitconfig" <<EOC
[credential "https://github.com"]
	helper =
	helper = !$(command -v gh) auth git-credential
[user]
	name = mu-maint
	email = ${email:-mu-maint@localhost}
EOC
git -C "$CLONE" remote set-url origin "https://github.com/$REPO.git"

[ -f "$MAINT/config.env" ] || echo "OMP_PROFILE=$profile" > "$MAINT/config.env"
[ -f "$MAINT/digest-last" ] || sed -n 's/^omp = "\(.*\)"/\1/p' "$here/../../upstream.toml" > "$MAINT/digest-last"

mklabel() { gh label create --force "$1" -R "$REPO" -d "$2" -c "$3" >/dev/null; }
mklabel maint:s0 "main is broken" B60205
mklabel maint:bump "oh-my-pi version bump" 0E8A16
mklabel maint:auto "merged automatically by the bot" C2E0C6
mklabel maint:needs-review "needs a human look" FBCA04
mklabel maint:red "checks still failing" D93F0B
mklabel maint:digest "weekly omp parity digest" 5319E7
mklabel maint:budget "agent budget reached" 7057FF
mklabel maint:bot-error "the bot itself failed" 000000

mkdir -p "$HOME/.config/systemd/user"
cp "$here"/systemd/* "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
systemctl --user enable --now mu-maint-nightly.timer mu-maint-digest.timer

if [ "$(loginctl show-user "$USER" -p Linger 2>/dev/null || true)" != "Linger=yes" ]; then
  echo "warning: lingering is off, timers only run while you are logged in. Run: loginctl enable-linger $USER" >&2
fi
echo "maint bot set up (profile $profile)"
