#!/usr/bin/env bash
# Compile every workload in src/ to js, wasm-gc, and native, plus an
# official-elm harness for the compute comparison.
set -e
cd "$(dirname "$0")"
ALM=../target/release/alm
mkdir -p build
for f in src/*.elm; do
    m=$(basename "$f" .elm)
    [ "$m" = "ElmBench" ] && continue   # elm-only harness, not an alm workload
    echo "building $m ..."
    "$ALM" make "$f" --target=js           --output="build/$m.js"     >/dev/null
    "$ALM" make "$f" --target=wasm-gc      --output="build/$m.wasm"   >/dev/null
    # native-typed = the unboxed/typed native backend (the real AOT ceiling;
    # plain --target=native is the uniform boxed+Boehm backend, ~10x slower).
    "$ALM" make "$f" --target=native-typed --output="build/$m.native" >/dev/null
done

# Official elm, for the compute comparison. `elm make` rejects `main : Int`, so
# strip each workload's `main` into an elm-only source dir; ElmBench.elm (a
# Platform.worker) drives each workload's `bench` through ports.
echo "building official-elm harness ..."
rm -rf elm-src && mkdir elm-src
for f in src/*.elm; do
    m=$(basename "$f" .elm)
    case "$m" in
        ElmBench) cp "$f" elm-src/ ;;
        Noop) ;;  # alm-only startup probe
        *) sed '/^main :/,$d' "$f" > "elm-src/$m.elm" ;;
    esac
done
elm make elm-src/ElmBench.elm --optimize --output=build/ElmBench.js >/dev/null
echo "done."
