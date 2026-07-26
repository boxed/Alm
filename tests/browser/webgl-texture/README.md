# WebGL.Texture GPU verification

`node` has neither a WebGL context nor a DOM `Image`, so `WebGL.Texture` is
verified in a real headless browser here (the `cargo test` `webgl_test` only
checks that a WebGL program compiles and wires up the kernel).

```sh
npm install puppeteer
alm make Main.elm --output=app.js
node render.js app.js   # PASS if the quad samples the texture's green
```

`Main.elm` loads a 2×2 opaque-green PNG with `Texture.load` (async `new
Image()`), then draws a full-canvas quad whose fragment shader samples it
(`texture2D`). `render.js` mounts the app, lets the load Task resolve, fires the
deferred first-frame draw (headless Chromium throttles `requestAnimationFrame`,
so the harness captures and invokes it), then reads the canvas back: the center
must be the texture's green — proving the image loaded, uploaded to the GPU
(`gl.texImage2D`), and sampled correctly.
