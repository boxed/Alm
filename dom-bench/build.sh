#!/usr/bin/env bash
# Build all five keyed-table apps into build/. Requires: elm 0.19.1, node (with
# `npm install` already run here), and alm's release binary. React & Svelte need
# node_modules (npm install).
set -e
cd "$(dirname "$0")"
ALM=${ALM:-../target/release/alm}
mkdir -p build
echo "elm ...";      elm make Main.elm --optimize --output=build/elm.js >/dev/null
echo "alm-js ...";   "$ALM" make Main.elm                 --output=build/almjs.js   >/dev/null
echo "alm-wasm ..."; "$ALM" make Main.elm --target=wasm-gc --output=build/almwasm.wasm >/dev/null
# Optimized (Html.Lazy) variants of the alm targets — see Main_lazy.elm.
echo "alm-js (opt) ...";   "$ALM" make Main_lazy.elm                 --output=build/almjs_lazy.js     >/dev/null
echo "alm-wasm (opt) ..."; "$ALM" make Main_lazy.elm --target=wasm-gc --output=build/almwasm_lazy.wasm >/dev/null
echo "react+svelte ..."; node build.mjs
echo "done. now: node drive.mjs   (set CHROME=/path/to/chrome if not macOS)"
