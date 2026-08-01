"""Build the projects the compile benchmark measures.

A benchmark workload has to be something every compiler can actually build,
which rules out a bare library package: it has no `main`, and both elm and alm
refuse to emit a program without one. The original figures were taken against
"a package's project plus every dependency", so that is what this reconstructs
— a minimal application that imports a package's whole public surface, which
forces the package and its transitive dependencies to be compiled.

Everything is resolved from `~/.elm`; nothing is downloaded.
"""

import json
import pathlib
import shutil

PACKAGES = (
    pathlib.Path.home() / ".elm" / "0.19.1" / "packages"
)

# The application needs these whatever the package under test pulls in:
# `Platform.worker` for a `main`, and elm/json because the compiler requires it
# wherever flags or ports could appear.
BASE = ["elm/core", "elm/json"]


def version_key(text):
    try:
        return tuple(int(part) for part in text.split("."))
    except ValueError:
        return (0, 0, 0)


def cached_versions(name):
    author, _, project = name.partition("/")
    directory = PACKAGES / author / project
    if not directory.is_dir():
        return []
    return sorted(
        (v.name for v in directory.iterdir() if (v / "elm.json").is_file()),
        key=version_key,
    )


def parse_constraint(text):
    """`1.0.0 <= v < 2.0.0` -> the pair of bounds, or None if unparseable."""
    parts = text.split()
    if len(parts) != 5 or parts[2] != "v":
        return None
    return (version_key(parts[0]), parts[1], parts[3], version_key(parts[4]))


def allows(constraint, version):
    low, low_op, high_op, high = constraint
    v = version_key(version)
    above = v >= low if low_op == "<=" else v > low
    below = v <= high if high_op == "<=" else v < high
    return above and below


def solve(roots):
    """Newest cached version of each package satisfying everything asked of it.

    The same highest-version-first walk alm's own resolver does. Good enough
    here: the cache holds one usable version of most packages, and a workload
    that cannot be resolved is simply skipped.
    """
    wanted = {name: [] for name in roots}
    for name, constraint in roots.items():
        if constraint:
            wanted[name].append(constraint)
    chosen = {}
    queue = list(wanted)
    while queue:
        name = queue.pop()
        if name in chosen:
            continue
        options = [
            v for v in cached_versions(name)
            if all(allows(c, v) for c in wanted.get(name, []))
        ]
        if not options:
            return None
        version = options[-1]
        chosen[name] = version
        manifest = json.loads((PACKAGES / name / version / "elm.json").read_text())
        for dep, text in (manifest.get("dependencies") or {}).items():
            if "/" not in dep:
                continue
            constraint = parse_constraint(text) if isinstance(text, str) else None
            entry = wanted.setdefault(dep, [])
            if constraint and constraint not in entry:
                entry.append(constraint)
                # A package already chosen may no longer qualify.
                if dep in chosen and not allows(constraint, chosen[dep]):
                    del chosen[dep]
            if dep not in chosen:
                queue.append(dep)
    return chosen


def complete(outline):
    """An application's `elm.json` with its indirect dependencies completed.

    A real application's checked-in `elm.json` is not always a closed set. A
    project that patches a package (elm-sideload swaps in a fork whose manifest
    drops a dependency) records the *forked* graph, so against the published
    packages in `~/.elm` the file is missing entries and the official compiler
    refuses it as "edited by hand ... in an invalid state". Filling the gap from
    the cache builds the same project the maintainers do, without touching the
    user's package cache; the direct dependencies and their versions are left
    exactly as pinned.

    Returns `(outline, added)`, or `(None, reason)` if the cache cannot close it.
    """
    direct = dict(outline["dependencies"]["direct"])
    chosen = dict(direct)
    chosen.update(outline["dependencies"]["indirect"])

    added = []
    queue = list(chosen)
    while queue:
        name = queue.pop()
        manifest_path = PACKAGES / name / chosen[name] / "elm.json"
        if not manifest_path.is_file():
            return None, f"{name} {chosen[name]} is not in ~/.elm"
        manifest = json.loads(manifest_path.read_text())
        for dep, text in (manifest.get("dependencies") or {}).items():
            if "/" not in dep or dep in chosen:
                continue
            constraint = parse_constraint(text) if isinstance(text, str) else None
            options = [
                v for v in cached_versions(dep)
                if constraint is None or allows(constraint, v)
            ]
            if not options:
                return None, f"nothing in ~/.elm satisfies {dep} {text} (needed by {name})"
            chosen[dep] = options[-1]
            added.append(f"{dep} {options[-1]}")
            queue.append(dep)

    outline = dict(outline)
    outline["dependencies"] = {
        "direct": dict(sorted(direct.items())),
        "indirect": dict(sorted((k, v) for k, v in chosen.items() if k not in direct)),
    }
    return outline, added


def exposed_modules(manifest):
    exposed = manifest.get("exposed-modules", [])
    if isinstance(exposed, dict):
        return [m for group in exposed.values() for m in group]
    return exposed


def wrap(package, into):
    """An application that imports all of `package`, ready to compile.

    Returns `(directory, entry)`, or `None` when the cache cannot satisfy it.
    """
    versions = cached_versions(package)
    if not versions:
        return None
    version = versions[-1]
    manifest = json.loads((PACKAGES / package / version / "elm.json").read_text())
    modules = exposed_modules(manifest)
    if not modules:
        return None

    roots = {package: None}
    for name in BASE:
        roots.setdefault(name, None)
    solution = solve(roots)
    if solution is None:
        return None

    directory = into / package.replace("/", "-")
    if directory.exists():
        shutil.rmtree(directory)
    (directory / "src").mkdir(parents=True)

    (directory / "elm.json").write_text(json.dumps({
        "type": "application",
        "source-directories": ["src"],
        "elm-version": "0.19.1",
        # Everything direct: the wrapper imports the package under test, and
        # the rest has to be present for the solution to be complete.
        "dependencies": {"direct": dict(sorted(solution.items())), "indirect": {}},
        "test-dependencies": {"direct": {}, "indirect": {}},
    }, indent=4) + "\n")

    # Importing every exposed module is what makes this a real workload: the
    # compiler has to check the package and everything under it. `main` only
    # exists because neither compiler will emit a program without one.
    imports = "\n".join(f"import {m}" for m in modules)
    (directory / "src" / "Main.elm").write_text(
        "module Main exposing (main)\n\n"
        "import Platform\n"
        f"{imports}\n\n\n"
        "main : Program () () ()\n"
        "main =\n"
        "    Platform.worker\n"
        "        { init = \\_ -> ( (), Cmd.none )\n"
        "        , update = \\_ model -> ( model, Cmd.none )\n"
        "        , subscriptions = \\_ -> Sub.none\n"
        "        }\n"
    )
    return directory, "src/Main.elm"
