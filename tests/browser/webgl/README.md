# WebGL GPU verification

`node` has no WebGL context, so the WebGL rendering kernel is verified in a real
headless browser here (the `cargo test` `webgl_test` only checks that a WebGL
program compiles and wires up the real kernel).

```sh
npm install puppeteer
alm make Main.elm --output=app.js
node render.js app.js   # PASS if the quad renders green on a blue clearColor
```

`Main.elm` draws a green quad (indexed mesh, a `mat4` uniform transform, and a
per-vertex color varying) on a blue `clearColor` background. `render.js` mounts
it, fires the deferred first-frame draw (headless Chromium throttles
`requestAnimationFrame`, so the harness captures and invokes it), then reads the
canvas back: center must be green, a corner the blue clear color.
