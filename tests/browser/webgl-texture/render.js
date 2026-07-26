// Headless GPU verification for WebGL.Texture. node has no WebGL context (and no
// DOM Image), so this drives a real headless Chromium + ANGLE/SwiftShader via
// puppeteer: it mounts the compiled app, lets `Texture.load` fetch the image and
// resolve its Task, fires the deferred draw, then reads the canvas back — the
// quad samples the texture, so its center must be the texture's green.
//
//   npm install puppeteer
//   ../../../target/release/alm make Main.elm --output=app.js   # or debug
//   node render.js app.js
//
// Exits 0 on PASS, 1 on FAIL. rAF is captured and fired manually because
// headless Chromium throttles requestAnimationFrame (never paints on its own).
const puppeteer = require('puppeteer');
const fs = require('fs');

function near(px, r, g, b) {
    var p = px.split(',').map(Number);
    return Math.abs(p[0] - r) < 40 && Math.abs(p[1] - g) < 40 && Math.abs(p[2] - b) < 40;
}

(async () => {
    const appJs = process.argv[2] || 'app.js';
    const browser = await puppeteer.launch({
        headless: 'new',
        args: ['--no-sandbox', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
    });
    const page = await browser.newPage();
    await page.setViewport({ width: 200, height: 200 });
    page.on('pageerror', (e) => console.log('PAGEERROR ' + e.message));
    await page.setContent('<div id="app"></div>');
    // Capture rAF callbacks so we can fire the deferred draw ourselves.
    await page.evaluate(() => {
        window.__raf = [];
        window.requestAnimationFrame = function (cb) { window.__raf.push(cb); return window.__raf.length; };
    });
    await page.addScriptTag({ content: fs.readFileSync(appJs, 'utf8') });
    await page.evaluate(() => window.Elm.Main.main.init({ node: document.getElementById('app') }));
    // Give the image load + Task.attempt time to resolve (mounts the canvas),
    // then drain rAF a few times to run the deferred first-frame draw.
    for (let i = 0; i < 6; i++) {
        await new Promise((r) => setTimeout(r, 80));
        await page.evaluate(() => { for (let j = 0; j < 20; j++) { const c = window.__raf.splice(0); if (!c.length) break; c.forEach((cb) => cb(0)); } });
    }

    const res = await page.evaluate(() => {
        const c = document.getElementById('glcanvas');
        if (!c) return { center: 'no-canvas' };
        const gl = c.getContext('webgl');
        const px = (x, y) => { const b = new Uint8Array(4); gl.readPixels(x, y, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, b); return [...b].join(','); };
        return { center: px(50, 50) };
    });
    await browser.close();

    const ok = near(res.center, 0, 200, 0);
    console.log((ok ? 'PASS' : 'FAIL') + ' center=' + res.center);
    process.exit(ok ? 0 : 1);
})().catch((e) => { console.log('ERROR ' + e.message); process.exit(1); });
