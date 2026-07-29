# Benchmarks and the report they feed.
#
#   make bench      run every harness, then rebuild the report
#   make report     rebuild the report from whatever results exist
#   make runtime    just the keyed-table app (needs Chrome)
#   make compute    just the pure-computation workloads
#   make compile    just compile speed
#   make check      build and test everything
#
# Each harness writes a results.json; `report` turns those into
# bench-report/report.html. Targets carry real prerequisites, so `make bench`
# after touching one benchmark's sources re-runs that one and leaves the others
# alone — the report then shows each section's own measurement date and flags
# any that have fallen behind.

SHELL := /bin/bash
.DELETE_ON_ERROR:

ALM        := target/release/alm
SOURCES    := $(shell find crates -name '*.rs' -o -name '*.js' -o -name '*.c' 2>/dev/null)
CHROME     ?= /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
export CHROME

RUNTIME_RESULTS := dom-bench/build/results.json
COMPUTE_RESULTS := compute-bench/results.json
COMPILE_RESULTS := compile-bench/results.json
REPORT          := bench-report/report.html

.PHONY: all bench report runtime compute compile check clean-bench help

all: help

help:
	@sed -n '2,15p' Makefile | sed -e 's/^# //' -e 's/^#$$//'

# ---------------------------------------------------------------- the compiler

# Every harness measures the release binary, so nothing runs against a stale one.
$(ALM): $(SOURCES) Cargo.toml Cargo.lock
	cargo build --release -p alm

# ------------------------------------------------------------------- harnesses

runtime: $(RUNTIME_RESULTS)
compute: $(COMPUTE_RESULTS)
compile: $(COMPILE_RESULTS)

# build.sh compiles the elm and alm bundles; build.mjs (which it calls) does
# only React and Svelte. Running the driver without the former measures stale
# bundles whose pages never mount, and every operation then times an empty
# frame.
$(RUNTIME_RESULTS): $(ALM) dom-bench/Main.elm dom-bench/Main_lazy.elm \
                    dom-bench/App.jsx dom-bench/App_opt.jsx dom-bench/App.svelte \
                    dom-bench/build.sh dom-bench/build.mjs dom-bench/drive.mjs
	cd dom-bench && ./build.sh && node drive.mjs

$(COMPUTE_RESULTS): $(ALM) $(wildcard compute-bench/src/*.elm) \
                    compute-bench/build.sh compute-bench/run.mjs
	cd compute-bench && ./build.sh && node run.mjs

$(COMPILE_RESULTS): $(ALM) compile-bench/run.py compile-bench/workloads.py
	python3 compile-bench/run.py

# --------------------------------------------------------------------- report

$(REPORT): bench-report/generate.py bench-report/template.html \
           $(RUNTIME_RESULTS) $(COMPUTE_RESULTS) $(COMPILE_RESULTS)
	python3 bench-report/generate.py

report: bench-report/generate.py bench-report/template.html
	python3 bench-report/generate.py

# Run everything, then report. A harness that fails does not stop the others:
# its results.json keeps the last good numbers and the report marks that
# section stale, which is the designed behaviour. The failure is still an
# error at the end, so this cannot pass quietly.
bench:
	@rc=0; \
	for target in runtime compute compile; do \
	  echo "=== $$target ==="; \
	  $(MAKE) --no-print-directory $$target || { echo "!!! $$target failed"; rc=1; }; \
	done; \
	echo "=== report ==="; \
	$(MAKE) --no-print-directory report; \
	exit $$rc

# ---------------------------------------------------------------------- checks

check:
	cargo build --release -p alm
	cargo test --workspace

clean-bench:
	rm -f $(COMPUTE_RESULTS) $(COMPILE_RESULTS) $(REPORT)
	rm -rf dom-bench/build compute-bench/build
