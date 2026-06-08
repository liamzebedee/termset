#!/usr/bin/env bash
# Regenerate the README screenshot.
#
# Renders the real UI headlessly over demo/termset.yml (a full-stack layout)
# with a few live-looking sessions and writes demo/screenshot.png, which the
# README embeds. Deterministic — no window, no PTY.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo run --quiet --example demo
echo "demo/screenshot.png updated"
