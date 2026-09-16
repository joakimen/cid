#!/usr/bin/env bash
# Refuse a commit that would land directly on `main`.
#
# PreToolUse payload on stdin. Exit 2 blocks and hands stderr back as the
# reason; every other path exits 0, so a missing `jq` never blocks work.
set -uo pipefail

command -v jq >/dev/null 2>&1 || exit 0

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
[ -n "${command:-}" ] || exit 0

# Anchored to a command position so a quoted `git commit` in prose is not a
# match. `-C <path>` and `-c <k>=<v>` are matched with their value, or
# `git -C . commit` slips through.
invocation=$(printf '%s' "$command" |
	grep -Eo '(^|[;&|(]|&&|\|\|)[[:space:]]*git([[:space:]]+(-[Cc][[:space:]]+[^[:space:]]+|-[^[:space:]]+))*[[:space:]]+commit([[:space:]]|$)' |
	tail -n 1) || exit 0
[ -n "$invocation" ] || exit 0

# A path as written in the command, unquoted and resolved against `$1`.
resolve() {
	local base=$1 path=$2
	path=${path#[\"\']}
	path=${path%[\"\']}
	case $path in
	/*) printf '%s' "$path" ;;
	\~ | \~/*) printf '%s' "${HOME}${path#\~}" ;;
	*) printf '%s' "$base/$path" ;;
	esac
}

# The directory git commits in, which decides the branch: the command's own
# directory, moved by the last `cd` chained before the commit and then by each
# `-C` git is given, in order.
cwd=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null)
[ -n "${cwd:-}" ] || cwd="${CLAUDE_PROJECT_DIR:-$PWD}"

before=${command%"$invocation"*}
target=$(printf '%s' "$before" |
	grep -Eo '(^|[;&|(])[[:space:]]*cd[[:space:]]+[^;&|)]+' |
	tail -n 1 |
	sed -E 's/^[;&|(]?[[:space:]]*cd[[:space:]]+//; s/[[:space:]]+$//') || true
[ -z "${target:-}" ] || cwd=$(resolve "$cwd" "$target")

while read -r path; do
	[ -n "$path" ] || continue
	cwd=$(resolve "$cwd" "$path")
done < <(printf '%s' "$invocation" | grep -Eo -- '-C[[:space:]]+[^[:space:]]+' | sed -E 's/^-C[[:space:]]+//')

branch=$(git -C "$cwd" rev-parse --abbrev-ref HEAD 2>/dev/null) || exit 0
[ "$branch" = "main" ] || exit 0

# CLAUDE.md's exception: a commit of nothing but Markdown may land on `main`.
#
# What the commit would contain is the staged set, plus every modified tracked
# file when the command stages them itself. That flag is matched loosely — a
# `-a` inside the message text matches too — because widening the set can only
# ever refuse a commit this hook would otherwise have allowed. An amend is
# never markdown-only: it carries whatever the commit it rewrites carried.
paths=$(git -C "$cwd" diff --cached --name-only 2>/dev/null)
if printf '%s' "$command" | grep -Eq '(^|[[:space:]])(-[A-Za-z]*a[A-Za-z]*|--all)([[:space:]]|$)'; then
	paths=$(printf '%s\n%s' "$paths" "$(git -C "$cwd" diff --name-only 2>/dev/null)")
fi
amends=$(printf '%s' "$command" | grep -Eq '(^|[[:space:]])--amend([[:space:]]|$)' && echo yes)

# An empty set means nothing is staged and the paths are on the command line,
# which this cannot see. Unprovable is refused, not waved through.
if [ -z "${amends:-}" ] && [ -n "$(printf '%s' "$paths" | tr -d '[:space:]')" ] &&
	! printf '%s\n' "$paths" | grep -v '^[[:space:]]*$' | grep -qv '\.md$'; then
	exit 0
fi

cat >&2 <<'EOF'
Refusing: HEAD is `main`, and this commit would land on it directly.

CLAUDE.md: "Never commit straight to `main`, with one exception: a commit whose
every path ends in `.md` may be committed and pushed to `main` directly."

Only a commit whose every staged path ends in `.md` is exempt, and this one
does not qualify — stage the Markdown on its own, or put the work on a branch.
Anything larger than a one-line fix gets its own worktree:

    git worktree add .claude/worktrees/<name> -b <name>

If this really has to go on main, run the commit outside Claude Code.
EOF
exit 2
