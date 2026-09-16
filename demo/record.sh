#!/bin/sh
# Render the README demo with VHS: build cid, generate the sandbox the tape
# runs against, play the tape. `--check` renders a dev build to a throwaway
# path instead, which is what CI runs.
#
# Usage: demo/record.sh [--check]
set -eu

CHECK=false
[ "${1:-}" = "--check" ] && CHECK=true

if ! command -v vhs > /dev/null 2>&1; then
    echo "demo: vhs is not installed." >&2
    echo "  macOS:  brew install vhs" >&2
    echo "  other:  https://github.com/charmbracelet/vhs#installation" >&2
    exit 1
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

# A check only proves the tape plays, which a dev build does in half the
# compile time; the recording a reader sees is made with the release binary.
if [ "$CHECK" = true ]; then
    cargo build
    CID_BIN_DIR=$ROOT/target/debug
else
    cargo build --release
    CID_BIN_DIR=$ROOT/target/release
fi
export CID_BIN_DIR

sh demo/fixture.sh target/demo-fixture

# VHS occasionally loses its terminal socket mid-recording ("use of closed
# network connection"). A recorder flake, not a broken tape — retry once.
play() {
    vhs "$@" || {
        echo "demo: recording failed, retrying once..." >&2
        sleep 2
        vhs "$@"
    }
}

if [ "$CHECK" = true ]; then
    out=$(mktemp -d)/demo-check.gif
    play --output "$out" demo/demo.tape
    [ -s "$out" ] || { echo "demo: tape produced no output" >&2; exit 1; }
    echo "demo: tape renders ($(wc -c < "$out" | tr -d ' ') bytes)"
    rm -f "$out"
else
    mkdir -p docs
    play demo/demo.tape
    echo "demo: wrote docs/demo.gif"
fi
