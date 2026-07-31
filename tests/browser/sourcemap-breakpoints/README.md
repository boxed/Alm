# Source maps you can actually set a breakpoint in

```sh
node run.mjs                                  # needs Google Chrome
node probe.mjs <dir> <bundle.js> [Module.elm] # point it at any built bundle
```

A source map can be perfectly well-formed and still useless to a debugger. If its
generated line numbers are off by even one, Chrome resolves a click in the Elm
source to a position it cannot place a breakpoint at, and drops the breakpoint —
without saying so. The map validates, the Elm sources display, breakpoints just
never appear. Clicking a line number highlights it briefly and nothing happens.

Nothing about that is visible to a test that only checks the map is valid, so
this measures the property that matters instead: **how many Elm lines a
breakpoint can actually be bound on**, asked of a real Chrome over the debugger
protocol. `probe.mjs` reproduces what DevTools does — reverse-map the line, set
the breakpoint, then check the position V8 *chose* still maps back to the line
that was clicked — and it is that last step that silently discards them.

`run.mjs` builds one program twice, plain and through `--live --output`, and
requires the second to be no worse than the first. That comparison is the point:
the `--live` bundle has the live-reload client added to it, and appending is safe
while **prepending shifts every mapping in the file**. It caught exactly that
regression once already.

Not every line can hold a breakpoint, in any language: an Elm line whose
generated form is a record field with a constant value, or a bare operand, has no
breakable position for V8 to stop at, and Chrome refuses the same thing in
hand-written JavaScript. So the check is relative, not `100%`.

`work/` is scratch and is rewritten on every run.
