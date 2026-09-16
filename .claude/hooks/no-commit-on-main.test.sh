#!/usr/bin/env bash
# Tests for no-commit-on-main.sh: feeds it PreToolUse payloads against a
# throwaway repository whose main checkout is on `main` and whose worktree is on
# a branch, and checks which commits it refuses.
set -euo pipefail

hook="$(cd "$(dirname "$0")" && pwd)/no-commit-on-main.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
main="$tmp/repo"
tree="$tmp/repo-feat"
git init -q -b main "$main"
git -C "$main" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m init
git -C "$main" worktree add -q -b feat "$tree"
touch "$main/lib.rs" "$tree/lib.rs"
git -C "$main" add lib.rs
git -C "$tree" add lib.rs

markdown="$tmp/docs"
git init -q -b main "$markdown"
git -C "$markdown" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m init
touch "$markdown/README.md"
git -C "$markdown" add README.md

failures=0

# expect <exit status> <cwd> <command>
expect() {
	local want=$1 cwd=$2 command=$3 got=0
	jq -n --arg command "$command" --arg cwd "$cwd" \
		'{tool_input: {command: $command}, cwd: $cwd}' |
		"$hook" >/dev/null 2>&1 || got=$?
	if [ "$got" != "$want" ]; then
		echo "FAIL: \`$command\` from ${cwd#"$tmp"/} exited $got, want $want" >&2
		failures=$((failures + 1))
	fi
}

refused=2
allowed=0

expect $refused "$main" 'git commit -m x'
expect $allowed "$tree" 'git commit -m x'
expect $allowed "$main" 'echo "git commit" is prose'

expect $allowed "$main" "git -C $tree commit -m x"
expect $refused "$tree" "git -C $main commit -m x"
expect $refused "$tmp" 'git -C repo commit -m x'
expect $allowed "$tmp" 'git -C repo -C ../repo-feat commit -m x'
expect $refused "$tree" "git -c user.name=x -C '$main' commit -m x"

expect $allowed "$main" "cd $tree && git commit -m x"
expect $refused "$tree" "cd \"$main\" && git commit -m x"
expect $refused "$tmp" 'cd repo; git commit -m x'
expect $allowed "$main" "cd $tree && git add -A && git commit -m x"

expect $allowed "$tree" "git -C $markdown commit -m x"
expect $refused "$tree" "git -C $markdown commit --amend -m x"

if [ "$failures" -gt 0 ]; then
	echo "no-commit-on-main: $failures failing case(s)" >&2
	exit 1
fi
echo "no-commit-on-main: all cases pass"
