.DEFAULT_GOAL := build

.PHONY: check
check: fmt-check lint test test-hooks

.PHONY: build
build: check
	cargo build --release

.PHONY: test
test:
	cargo test

# The agent hooks in .claude/hooks, against a throwaway repository. Needs jq.
.PHONY: test-hooks
test-hooks:
	.claude/hooks/no-commit-on-main.test.sh

.PHONY: lint
lint:
	cargo clippy --all-targets -- -D warnings

.PHONY: fmt
fmt:
	cargo fmt

.PHONY: fmt-check
fmt-check:
	cargo fmt --check

.PHONY: install
install:
	cargo install --locked --path . --force

# Install the git hooks in prek.toml. Opt-in, and once per clone — the hooks
# directory lives in the common `.git`, so every worktree shares it.
.PHONY: hooks
hooks:
	prek install --hook-type pre-commit --hook-type pre-push

# Releasing has no target here, and cannot have one: release-plz tags the merge
# of the pull request it keeps open, and dist builds and publishes what that tag
# starts. Both run in CI — see README.md under Releasing.

# Re-record docs/demo.gif. Local only — the render depends on installed fonts.
# Requires vhs (brew install vhs).
.PHONY: demo
demo:
	./demo/record.sh

# Render the tape to a throwaway path, leaving the committed GIF alone.
.PHONY: demo-check
demo-check:
	./demo/record.sh --check

# Build the demo sandbox and print how to enter it.
.PHONY: demo-fixture
demo-fixture:
	cargo build --release
	CID_BIN_DIR=$(CURDIR)/target/release sh demo/fixture.sh target/demo-fixture
	@echo
	@echo "Enter the sandbox with:"
	@echo "  source target/demo-fixture/env.sh"
