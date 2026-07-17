#!/usr/bin/env bash
# Compile every workload in src/ to js, wasm-gc, and native.
set -e
cd "$(dirname "$0")"
ALM=../target/release/alm
mkdir -p build
for f in src/*.elm; do
    m=$(basename "$f" .elm)
    echo "building $m ..."
    "$ALM" make "$f" --target=js           --output="build/$m.js"     >/dev/null
    "$ALM" make "$f" --target=wasm-gc      --output="build/$m.wasm"   >/dev/null
    # native-typed = the unboxed/typed native backend (the real AOT ceiling;
    # plain --target=native is the uniform boxed+Boehm backend, ~10x slower).
    "$ALM" make "$f" --target=native-typed --output="build/$m.native" >/dev/null
done
echo "done."
