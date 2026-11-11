#!/usr/bin/env bash
# Run: scripts/maint/test-only.test.sh   Cases for test-only.sh, each in a throwaway git repository.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0

base() {
  rm -rf "$tmp/r"
  mkdir -p "$tmp/r/crates/a/src" "$tmp/r/crates/a/tests" "$tmp/r/docs"
  cd "$tmp/r"
  git init -q
  git config user.email t@localhost
  git config user.name t
  cat > crates/a/src/lib.rs <<'RS'
pub fn one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_one() {
        assert_eq!(one(), 1);
    }
}
RS
  cat > crates/a/src/two.rs <<'RS'
#[cfg(test)]
mod helpers {
    pub fn h() {}
}

pub fn two() -> u32 {
    2
}
RS
  echo 'pub fn t() {}' > crates/a/src/agent_tests.rs
  echo '# readme' > README.md
  echo 'usage' > docs/usage.md
  echo '[package]' > Cargo.toml
  echo 'prompt' > crates/a/src/system.md
  git add -A
  git commit -q -m base
}

# expect <0|1> <name>: stage the tree and compare test-only.sh's exit code
expect() {
  local want="$1" name="$2" got=0
  git add -A
  "$here/test-only.sh" "$tmp/r" 2> /dev/null || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok $name"
  else
    echo "FAIL $name: exit $got, want $want"
    fails=$((fails + 1))
  fi
}

# line <n> <text> <file>: insert <text> as line <n>
line() {
  awk -v n="$1" -v t="$2" 'NR == n { print t } { print }' "$3" > "$3.new"
  mv "$3.new" "$3"
}

base; line 12 '    #[test] fn again() { assert_eq!(one(), 1); }' crates/a/src/lib.rs
expect 0 "a test added inside the trailing test module"

base; sed -i 's/assert_eq!(one(), 1);/assert!(one() == 1);/' crates/a/src/lib.rs
expect 0 "a test changed inside the trailing test module"

base; sed -i 's/^    1$/    1 + 0/' crates/a/src/lib.rs
expect 1 "code changed above the test module"

base; line 12 '    #[test] fn again() {}' crates/a/src/lib.rs; sed -i 's/^    1$/    1 + 0/' crates/a/src/lib.rs
expect 1 "a test added and code changed in one file"

base; line 5 'pub fn extra() {}' crates/a/src/lib.rs
expect 1 "code inserted right above the test module"

base; echo 'pub fn after() {}' >> crates/a/src/lib.rs
expect 1 "code appended after the test module"

base; sed -i '/^#\[cfg(test)\]$/d' crates/a/src/lib.rs
expect 1 "the cfg(test) attribute removed"

base; line 3 '    pub fn g() {}' crates/a/src/two.rs
expect 1 "a cfg(test) module that is not the end of the file"

base; echo 'pub fn u() {}' >> crates/a/src/agent_tests.rs; echo 'x' > crates/a/tests/it.rs; echo 'more' >> docs/usage.md; echo 'more' >> README.md
expect 0 "test files, docs and the README"

base; echo 'more' >> crates/a/src/system.md
expect 1 "a markdown prompt under crates/"

base; echo 'x = 1' >> Cargo.toml; line 12 '    #[test] fn again() {}' crates/a/src/lib.rs
expect 1 "Cargo.toml next to a test"

base; echo 'pub fn n() {}' > crates/a/src/new.rs
expect 1 "a new source file"

base; git rm -q docs/usage.md
expect 1 "a deleted doc"

base
expect 1 "nothing staged"

[ "$fails" = 0 ]
