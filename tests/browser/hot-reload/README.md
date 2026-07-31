# Hot reload into a page alm does not serve

`alm make --live --output=…` is for the case where the compiled program is one
piece of a larger app: that app serves the page, and only loads the bundle. This
verifies the whole chain in a real browser, across two origins — which is the
part no unit test can reach.

```sh
node run.mjs      # needs Google Chrome; uses puppeteer-core from ../../../dom-bench
```

The harness starts `alm make --live --output=work/static/app.js` on one port and
a plain static server on **another** port, standing in for Django or Vite. The
page it serves is the minimal thing such an app would write:

```html
<div id="widget"></div>
<script src="/static/app.js"></script>
<script>Elm.Main.init({ node: …, flags: { greeting: 'hello' } });</script>
```

Nothing in it knows about alm. Everything the reload needs — the registry, the
event stream, the swap — rides in `app.js`.

It then checks the things that can each independently be broken:

1. **The swap arrives at all.** Click the counter to 3, change the *view* only,
   and the new view must appear.
2. **The model survives a cross-origin swap.** The count must still be 3. This
   is the CORS-sensitive one: the `Model` fingerprint travels in a response
   header, and a cross-origin reader cannot see a custom header unless the
   server names it in `Access-Control-Expose-Headers`. Get that wrong and every
   swap silently starts fresh.
3. **The flags survive.** A swap re-initializes the program, so it has to pass
   the flags the page gave the first time — an embedded program is normally
   started with some, and there is nowhere else to get them from.
4. **A changed `Model` starts fresh.** Add a field and the count must reset,
   rather than the new build reading the old model as if it fit.
5. **It really was a swap.** Counted in `sessionStorage`, because a reload is
   the client's own fallback for a swap it cannot do — and a reload re-runs the
   page's script with the real flags, so a broken swap otherwise *looks* like a
   working one. Checks 1–4 are worth much less without this one.

`Served.elm` is the same program without flags, served on a third port for the
last three checks: those go through the page alm serves itself, which starts a
program with no options at all and so cannot run one that takes flags. It shares
the registry machinery with the embedded case, so it has to keep working too.

`work/` is scratch and is rewritten on every run.
