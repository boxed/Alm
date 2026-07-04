#!/bin/sh
# Measure JavaScript runtime coverage: run the cargo test suite with V8
# coverage collection on every node child process, then map the ranges
# back onto runtime.js (which sits at a fixed offset in every bundle).
#
# Rust-side coverage: cargo llvm-cov --summary-only
set -e
cd "$(dirname "$0")/../.."
export SCRATCH="${TMPDIR:-/tmp}/alm-jscov"
rm -rf "$SCRATCH/v8cov" && mkdir -p "$SCRATCH/v8cov"
NODE_V8_COVERAGE="$SCRATCH/v8cov" cargo test --quiet
python3 tests/coverage/analyze-runtime-coverage.py
