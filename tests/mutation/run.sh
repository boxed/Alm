#!/bin/sh
# Mutation testing for the JavaScript runtime kernel.
#
# 1. Collects V8 coverage from the node-executing test suites to find
#    which runtime.js lines are actually exercised.
# 2. Generates single-point mutants on those lines (operators, literals,
#    data tags) and runs the test binaries against each, injected via
#    the ALM_RUNTIME_JS override.
#
# A mutant is killed when any test binary fails or hangs. Survivors are
# printed with line numbers; each one is either a missing test or an
# equivalent mutant (documented in equivalents.txt).
set -e
cd "$(dirname "$0")/../.."

SCRATCH="${TMPDIR:-/tmp}/alm-mutation"
rm -rf "$SCRATCH/v8cov" && mkdir -p "$SCRATCH/v8cov"
echo "== collecting coverage to scope mutants..."
NODE_V8_COVERAGE="$SCRATCH/v8cov" cargo test --quiet > /dev/null

SCRATCH="$SCRATCH" python3 - <<'EOF'
import json, glob, os, bisect
RUNTIME = open('crates/compiler/src/generate/runtime.js').read()
PRELUDE = "(function () {\n'use strict';\n\n"
R_START = len(PRELUDE); R_END = R_START + len(RUNTIME)
line_starts=[0]
for i,ch in enumerate(RUNTIME):
    if ch=='\n': line_starts.append(i+1)
def to_line(off): return bisect.bisect_right(line_starts, off)
covered = bytearray(len(RUNTIME))
for path in glob.glob(os.environ['SCRATCH']+'/v8cov/*.json'):
    data=json.load(open(path))
    for script in data.get('result',[]):
        if not script.get('url','').endswith(('bundle.js','Main.js')): continue
        ranges=[]
        for fn in script['functions']:
            for r in fn['ranges']:
                ranges.append((r['startOffset'],r['endOffset'],r['count']))
        ranges.sort(key=lambda r:(r[0],-r[1]))
        resolved=bytearray(len(RUNTIME))
        for s,e,c in ranges:
            s=max(s,R_START)-R_START; e=min(e,R_END)-R_START
            if s>=e: continue
            resolved[s:e]=(b'\x01' if c>0 else b'\x00')*(e-s)
        for i,v in enumerate(resolved):
            if v: covered[i]=1
lines=sorted({to_line(i) for i,v in enumerate(covered) if v})
open('tests/mutation/covered-lines.txt','w').write('\n'.join(map(str,lines)))
print(f"== {len(lines)} covered lines")
EOF

echo "== running mutants..."
WORKERS="${WORKERS:-8}" python3 tests/mutation/mutate.py
