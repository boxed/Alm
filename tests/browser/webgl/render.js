// Headless GPU verification for the WebGL kernel. node has no WebGL context, so
// this drives a real (headless Chromium + ANGLE/SwiftShader) one via puppeteer:
// it mounts the compiled app, fires the deferred first-frame draw, and reads the
// canvas back — the quad's center must be green and a corner the blue clearColor.
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
    // Capture rAF callbacks so we can fire the deferred first draw ourselves.
    await page.evaluate(() => {
        window.__raf = [];
        window.requestAnimationFrame = function (cb) { window.__raf.push(cb); return window.__raf.length; };
    });
    await page.addScriptTag({ content: fs.readFileSync(appJs, 'utf8') });
    await page.evaluate(() => window.Elm.Main.init({ node: document.getElementById('app') }));
    await new Promise((r) => setTimeout(r, 100));
    await page.evaluate(() => { for (let i = 0; i < 5; i++) { (window.__raf.splice(0)).forEach((cb) => cb(0)); } });

    const res = await page.evaluate(() => {
        const c = document.getElementById('glcanvas');
        const gl = c.getContext('webgl');
        const px = (x, y) => { const b = new Uint8Array(4); gl.readPixels(x, y, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, b); return [...b].join(','); };
        return { center: px(50, 30), corner: px(5, 95) };
    });
    await browser.close();

    const ok = near(res.center, 0, 255, 0) && near(res.corner, 0, 0, 255);
    console.log((ok ? 'PASS' : 'FAIL') + ' center=' + res.center + ' corner=' + res.corner);
    process.exit(ok ? 0 : 1);
})().catch((e) => { console.log('ERROR ' + e.message); process.exit(1); });
