#!/usr/bin/env python3
"""Mutation testing for the alm JavaScript runtime kernel.

Generates single-point mutants of runtime.js (skipping strings, comments,
and regex literals except where noted), injects each via ALM_RUNTIME_JS,
and runs the node-executing test binaries. A mutant is killed when any
test binary fails or hangs.

Usage:
    python3 tests/mutation/mutate.py            # full run
    python3 tests/mutation/mutate.py --list     # just count mutants
    python3 tests/mutation/mutate.py --only 123 # run one mutant, verbose
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ProcessPoolExecutor, as_completed

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
RUNTIME = os.path.join(ROOT, "crates/compiler/src/generate/runtime.js")
TIMEOUT = 90  # seconds per test binary; a hang counts as killed

# ---------------------------------------------------------------- lexing

NORMAL, LINE_COMMENT, BLOCK_COMMENT, SQ_STRING, DQ_STRING, REGEX = range(6)


def lex_regions(src):
    """Classify every byte of the source: code, comment, string, regex."""
    kinds = bytearray(len(src))  # NORMAL=0 etc.
    state = NORMAL
    # Track the last significant char to decide if `/` starts a regex.
    last_sig = ";"
    i = 0
    while i < len(src):
        c = src[i]
        nxt = src[i + 1] if i + 1 < len(src) else ""
        if state == NORMAL:
            if c == "/" and nxt == "/":
                state = LINE_COMMENT
                kinds[i] = LINE_COMMENT
            elif c == "/" and nxt == "*":
                state = BLOCK_COMMENT
                kinds[i] = BLOCK_COMMENT
            elif c == "'":
                state = SQ_STRING
                kinds[i] = SQ_STRING
            elif c == '"':
                state = DQ_STRING
                kinds[i] = DQ_STRING
            elif c == "/" and last_sig in "(,=:[!&|?{;+<>%*~^" :
                state = REGEX
                kinds[i] = REGEX
            else:
                kinds[i] = NORMAL
                if not c.isspace():
                    last_sig = c
        elif state == LINE_COMMENT:
            kinds[i] = LINE_COMMENT
            if c == "\n":
                state = NORMAL
        elif state == BLOCK_COMMENT:
            kinds[i] = BLOCK_COMMENT
            if c == "/" and src[i - 1] == "*":
                state = NORMAL
        elif state in (SQ_STRING, DQ_STRING):
            kinds[i] = state
            if c == "\\":
                if i + 1 < len(src):
                    kinds[i + 1] = state
                i += 2
                continue
            if (state == SQ_STRING and c == "'") or (state == DQ_STRING and c == '"'):
                state = NORMAL
                last_sig = c
        elif state == REGEX:
            kinds[i] = REGEX
            if c == "\\":
                if i + 1 < len(src):
                    kinds[i + 1] = REGEX
                i += 2
                continue
            if c == "/":
                state = NORMAL
                last_sig = c
        i += 1
    return kinds


# ------------------------------------------------------------- operators

def generate_mutations(src, covered_lines):
    """Yield (pos, length, replacement, description) single-point mutants."""
    kinds = lex_regions(src)
    line_of = []
    line = 1
    for ch in src:
        line_of.append(line)
        if ch == "\n":
            line += 1

    def code_at(i):
        return kinds[i] == NORMAL

    def on_covered_line(i):
        return line_of[i] in covered_lines

    muts = []

    # Multi-char operators, longest first so we never split them.
    swaps = [
        ("===", "!=="), ("!==", "==="),
        (">>>", None), ("<<", None), (">>", None),  # recognized, not swapped here
        ("<=", "<"), (">=", ">"),
        ("&&", "||"), ("||", "&&"),
        ("+=", None), ("-=", None), ("++", None), ("--", None),
    ]
    i = 0
    while i < len(src):
        if not code_at(i):
            i += 1
            continue
        matched = False
        for op, repl in swaps:
            if src.startswith(op, i) and all(code_at(j) for j in range(i, i + len(op))):
                if repl and on_covered_line(i):
                    muts.append((i, len(op), repl, f"{op} -> {repl}"))
                i += len(op)
                matched = True
                break
        if matched:
            continue
        c = src[i]
        prev = src[i - 1] if i else ""
        nxt = src[i + 1] if i + 1 < len(src) else ""
        if on_covered_line(i):
            if c == "<" and nxt not in "=<" and prev != "<":
                muts.append((i, 1, "<=", "< -> <="))
            elif c == ">" and nxt not in "=>" and prev not in ">=":
                muts.append((i, 1, ">=", "> -> >="))
            elif c == "+" and nxt not in "+=" and prev != "+":
                muts.append((i, 1, "-", "+ -> -"))
            elif c == "-" and nxt not in "-=" and prev != "-" and (nxt.isalnum() or nxt in " (_$"):
                # skip negative literals in obvious `= -1` positions? keep: unary minus removal is a fine mutant
                muts.append((i, 1, "+", "- -> +"))
            elif c == "*" and nxt != "=" and prev != "/" and nxt != "/":
                muts.append((i, 1, "/", "* -> /"))
            elif c == "%" and nxt != "=":
                muts.append((i, 1, "*", "% -> *"))
            elif c == "!" and nxt not in "=":
                muts.append((i, 1, "", "delete !"))
            elif c == "&" and nxt != "&" and prev != "&":
                muts.append((i, 1, "|", "& -> |"))
            elif c == "|" and nxt != "|" and prev != "|":
                muts.append((i, 1, "&", "| -> &"))
            elif c == "^":
                muts.append((i, 1, "&", "^ -> &"))
        i += 1

    # Keywords and numbers via regex over code regions.
    for m in re.finditer(r"\btrue\b|\bfalse\b|\b\d+(?:\.\d+)?\b", src):
        i = m.start()
        if not code_at(i) or not on_covered_line(i):
            continue
        tok = m.group(0)
        if tok == "true":
            muts.append((i, 4, "false", "true -> false"))
        elif tok == "false":
            muts.append((i, 5, "true", "false -> true"))
        elif "." not in tok:
            n = int(tok)
            muts.append((i, len(tok), str(n + 1), f"{n} -> {n+1}"))
            if n > 0:
                muts.append((i, len(tok), str(n - 1), f"{n} -> {n-1}"))

    # Short string literals are data tags ('::', '#2', 'Just', ...): high value.
    for m in re.finditer(r"'([^'\\\n]{1,6})'", src):
        i = m.start()
        if kinds[i] != NORMAL:  # opening quote must be code
            continue
        if not on_covered_line(i):
            continue
        muts.append((i + 1, len(m.group(1)), "MUT", f"'{m.group(1)}' -> tag mutated"))

    return muts


# --------------------------------------------------------------- running

def test_binaries():
    """Discover the node-executing test binaries, fastest killers first."""
    out = subprocess.run(
        ["cargo", "test", "--no-run", "--message-format=json"],
        cwd=ROOT, capture_output=True, text=True,
    )
    binaries = {}
    for line in out.stdout.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("executable") and msg.get("target", {}).get("kind") == ["test"]:
            binaries[msg["target"]["name"]] = msg["executable"]
    order = ["runtime_test", "e2e_test", "tea_test", "project_test"]
    return [(n, binaries[n]) for n in order if n in binaries]


def run_mutant(args):
    index, pos, length, repl, desc, src, binaries, workdir = args
    mutated = src[:pos] + repl + src[pos + length:]
    path = os.path.join(workdir, f"mutant_{index}.js")
    with open(path, "w") as f:
        f.write(mutated)
    env = dict(os.environ, ALM_RUNTIME_JS=path)
    for name, binary in binaries:
        try:
            r = subprocess.run(
                [binary], env=env, capture_output=True, timeout=TIMEOUT, cwd=ROOT,
            )
        except subprocess.TimeoutExpired:
            os.unlink(path)
            return (index, "killed", f"timeout in {name}", desc, pos)
        if r.returncode != 0:
            os.unlink(path)
            return (index, "killed", name, desc, pos)
    os.unlink(path)
    return (index, "SURVIVED", "", desc, pos)


def main():
    src = open(RUNTIME).read()

    # Only mutate lines the node suite actually executes (from V8 coverage).
    coverage_file = os.path.join(ROOT, "tests/mutation/covered-lines.txt")
    if os.path.exists(coverage_file):
        covered = set(int(l) for l in open(coverage_file) if l.strip())
    else:
        print("no covered-lines.txt: mutating every line", file=sys.stderr)
        covered = set(range(1, src.count("\n") + 2))

    muts = generate_mutations(src, covered)
    print(f"{len(muts)} mutants on {len(covered)} covered lines")
    if "--list" in sys.argv:
        return

    binaries = test_binaries()
    print("test binaries:", ", ".join(n for n, _ in binaries))

    line_of = []
    line = 1
    for ch in src:
        line_of.append(line)
        if ch == "\n":
            line += 1

    if "--only" in sys.argv:
        index = int(sys.argv[sys.argv.index("--only") + 1])
        pos, length, repl, desc = muts[index]
        with tempfile.TemporaryDirectory() as d:
            result = run_mutant((index, pos, length, repl, desc, src, binaries, d))
        print(result, "line", line_of[pos])
        return

    started = time.time()
    survivors = []
    killed = 0
    with tempfile.TemporaryDirectory() as workdir:
        jobs = [
            (i, pos, length, repl, desc, src, binaries, workdir)
            for i, (pos, length, repl, desc) in enumerate(muts)
        ]
        with ProcessPoolExecutor(max_workers=int(os.environ.get("WORKERS", "8"))) as pool:
            for n, fut in enumerate(as_completed(pool.submit(run_mutant, j) for j in jobs)):
                index, status, where, desc, pos = fut.result()
                if status == "killed":
                    killed += 1
                else:
                    survivors.append((line_of[pos], desc, index))
                if (n + 1) % 100 == 0:
                    elapsed = time.time() - started
                    print(f"  {n+1}/{len(muts)}  killed={killed} survived={len(survivors)}  ({elapsed:.0f}s)")

    survivors.sort()
    print(f"\nmutation score: {killed}/{len(muts)} = {killed/len(muts)*100:.1f}%")
    print(f"SURVIVORS ({len(survivors)}):")
    for line, desc, index in survivors:
        print(f"  line {line:5d}  [{index}]  {desc}")


if __name__ == "__main__":
    main()
