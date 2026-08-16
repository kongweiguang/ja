#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# @author kongweiguang

# Keep Bash as a CI shell boundary while delegating every fixture assertion to
# the exact Rust cleanup binary used by the production workflow step.
set -euo pipefail
cargo +1.88.0 run \
  --manifest-path spikes/sandbox/macos/Cargo.toml \
  --bin marker_cleanup \
  --locked \
  -- --fixture
