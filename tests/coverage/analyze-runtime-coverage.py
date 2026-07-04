"""Map V8 coverage from all node test runs onto runtime.js.

Every alm bundle embeds runtime.js verbatim at a fixed offset, so ranges
from any bundle can be translated to runtime.js positions and merged.
"""
import json, glob, os, bisect

RUNTIME = open('/Users/boxed/Projects/alm/crates/compiler/src/generate/runtime.js').read()
PRELUDE = "(function () {\n'use strict';\n\n"
R_START = len(PRELUDE)
R_END = R_START + len(RUNTIME)

# line index for runtime.js
line_starts = [0]
for i, ch in enumerate(RUNTIME):
    if ch == '\n':
        line_starts.append(i + 1)
def to_line(offset):  # runtime-relative byte -> 1-based line
    return bisect.bisect_right(line_starts, offset)

# Collect per-function coverage across all runs, keyed by extent.
functions = {}   # (start,end) -> {'name': str, 'covered': bool}
covered_bytes = bytearray(len(RUNTIME))  # resolved count>0 anywhere

matched_scripts = 0
for path in glob.glob(os.environ['SCRATCH'] + '/v8cov/*.json'):
    data = json.load(open(path))
    for script in data.get('result', []):
        url = script.get('url', '')
        if not (url.endswith('bundle.js') or url.endswith('Main.js') or url.endswith('/test.js') or url.endswith('app.js')):
            continue
        # verify this script embeds the runtime where we expect (cheap check)
        matched_scripts += 1
        ranges = []
        for fn in script['functions']:
            extent = fn['ranges'][0]
            fs, fe = extent['startOffset'], extent['endOffset']
            if fs >= R_START and fe <= R_END:
                key = (fs - R_START, fe - R_START)
                entry = functions.setdefault(key, {'name': fn['functionName'], 'covered': False})
                if extent['count'] > 0:
                    entry['covered'] = True
                if fn['functionName'] and not entry['name']:
                    entry['name'] = fn['functionName']
            for r in fn['ranges']:
                ranges.append((r['startOffset'], r['endOffset'], r['count']))
        # resolve nested ranges: apply wider first, narrower override
        ranges.sort(key=lambda r: (r[0], -r[1]))
        resolved = bytearray(len(RUNTIME))
        for s, e, c in ranges:
            s = max(s, R_START) - R_START
            e = min(e, R_END) - R_START
            if s >= e: continue
            resolved[s:e] = (b'\x01' if c > 0 else b'\x00') * (e - s)
        for i, v in enumerate(resolved):
            if v: covered_bytes[i] = 1

total_fns = len(functions)
covered_fns = sum(1 for f in functions.values() if f['covered'])
print(f"scripts analyzed: {matched_scripts}")
print(f"runtime.js functions: {covered_fns}/{total_fns} covered ({covered_fns/total_fns*100:.1f}%)")

# line coverage approximation: executable = line intersecting any function extent
executable = set()
covered_lines = set()
for (fs, fe), info in functions.items():
    for line in range(to_line(fs), to_line(max(fs, fe - 1)) + 1):
        executable.add(line)
for line in executable:
    ls = line_starts[line - 1]
    le = line_starts[line] if line < len(line_starts) else len(RUNTIME)
    if any(covered_bytes[ls:le]):
        covered_lines.add(line)
print(f"runtime.js lines (in function extents): {len(covered_lines)}/{len(executable)} ({len(covered_lines)/len(executable)*100:.1f}%)")

print("\nUNCOVERED functions (name @ line):")
uncovered = sorted(((to_line(fs), info['name'] or '<anonymous>') for (fs, fe), info in functions.items() if not info['covered']))
for line, name in uncovered:
    print(f"  {line:5d}  {name}")
