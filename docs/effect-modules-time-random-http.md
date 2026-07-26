# Making Time / Random / Http real effect modules

Goal (user directive): convert the EFFECT part of Time/Random/Http from hardcoded
"concrete runtime effects" into real `effect module` managers running through
alm's `_Platform` protocol, on **all three backends** (JS, native, wasm-gc).
Keep pure helpers (calendar math, Random generators, Http body/expect builders)
as intrinsics. **No leftover shims** — remove all the old hardcoded Cmd/Sub
interception once the managers work.

## The three managers (from the real elm sources)

- **Time** (`effect module Time where { subscription = MySub }`): `every` builds
  `subscription (Every interval tagger)`. Manager state = Dict interval→ProcessId
  + Dict interval→taggers. `onEffects` diffs the interval set: spawn a
  `setInterval` process per new interval, `Process.kill` dropped ones.
  `onSelfMsg interval` reads `now` and fires that interval's taggers via
  `sendToApp`. Kernel prims: `Elm.Kernel.Time.now/here/setInterval/getZoneName`.
- **Random** (`effect module Random where { command = MyCmd }`): `generate`
  builds `command (Generate (map tagger gen))`. `init` seeds from `Time.now`
  (DEPENDS ON TIME). `onEffects` steps each generator, `sendToApp` the value. No
  spawn/kill, no kernel of its own. Trivial once the protocol works.
- **Http** (`effect module Http where { command = MyCmd, subscription = MySub }`):
  `request` builds a `Request` cmd; progress `track` builds a `MySub`. Manager
  state = Dict tracker→ProcessId + sub list. `onEffects` spawns a request process
  per cmd (`Process.spawn (Elm.Kernel.Http.toTask router send req)`), kills on
  Cancel. `onSelfMsg (tracker,progress)` delivers progress to matching subs.
  Kernel prims: `Elm.Kernel.Http.toTask/expect/emptyBody/pair/mapExpect/...`.

## KEY PREREQUISITE: task cancellation (Process.spawn/kill)

alm currently has NO task cancellation. `$Process$kill` is a no-op and
`$Process$spawn` runs the task to completion (`runtime.js:5082-5093`). Time's
manager `Process.kill`s a timer process to `clearInterval` when a subscription is
dropped; Http kills to abort. Without real kill, dropped `Time.every`
subscriptions would leak timers — a correctness regression. So each backend needs
lightweight cancellation, sufficient for **single-binding spawns** (Time spawns
`setInterval …`, Http spawns `toTask …`; Random never spawns).

### JS design (cheap — `_Task_fork` already returns the raw binding's value, `runtime.js:3750`)
- `Elm.Kernel.Time.setInterval`: a `_Task(fork)` whose fork does
  `var id=setInterval(()=>rawSpawn(task),interval); return function(){clearInterval(id);};`
  (returns a canceller).
- `$Process$spawn(task)`: `var cancel=_Task_fork(task,noop,noop); return {$:'ProcessId',cancel:cancel};`
- `$Process$kill(id)`: `if(id.cancel) id.cancel();`
  (Chains don't propagate a canceller — fine, Time/Http spawn raw bindings.)

## JS wiring plan
1. Register managers: `_Platform_effectManagers['Time'|'Random'|'Http'] = _Platform_createManager(init,onEffects,onSelfMsg,cmdMap,subMap)`.
2. Make `Time.every`/`Random.generate`/`Http.request`(+track) return
   `{$:'Leaf',home,value}` bags (via `_Platform_leaf(home)`), so
   `_Platform_gatherEffects` buckets them and delivers to `onEffects`.
   `_Platform_instantiateManager` must run for these homes at startup.
3. Provide kernel prims: `Elm.Kernel.Time.now/here/setInterval/getZoneName`,
   `Elm.Kernel.Http.toTask` (a cancellable fetch/XHR binding calling the router's
   send + progress), plus the pure Http builders (already exist as `_Http_*`).
4. **Remove** the concrete dispatch: the `activeTimers`/`setInterval` sub
   reconciler (`~runtime.js:5353`), the `CmdTask`/`_Http_makeTask` cmd branch for
   Http (`~5275`), and any Random cmd branch. The manager protocol replaces them.
5. Random.init needs `Time.now` → keep `_Time_now` prim.

## Chosen strategy (user pick): THIN SOURCE REPLACES BUILTINS

Time/Random/Http become real compiled `effect module` sources, bundled with the
compiler (`crates/compiler/src/builtin_src/*.elm`, via `include_str!`) and
injected into the compile graph. Pure helpers stay as the EXISTING optimized
intrinsics, re-keyed to `Elm.Kernel.{Time,Random,Http}.*` names, which the thin
source delegates to. The effect part (every/generate/request/track + MyCmd/MySub
+ subMap/cmdMap + init/onEffects/onSelfMsg) is real Elm → real manager via the
existing `_Platform` protocol. Builtins for these three are REMOVED.

### Injection seam (front-end)
- Remove Time/Random/Http from `builtins.rs`: `VALUES`, `UNIONS`, `ALIASES`,
  `OPAQUE_TYPES`, `lookup_type_home`, `MODULES`, `is_builtin_module`.
- `project.rs`: bundled sources are pre-empted in `load_module_file` — when an
  import name ∈ BUNDLED set, load the `include_str!` source under a synthetic key
  instead of `find_module_file` on disk. Bundled modules' own imports
  (Dict/Task/Process/Platform/List/Maybe/Basics + `Elm.Kernel.*`) are all builtin
  (filtered by `user_imports`) except cross-bundled (Random imports Time) which
  the same pre-emption handles recursively.

### Rep-matching (the crux)
The re-keyed intrinsics read specific runtime reps. The bundled source type
declarations MUST produce those exact reps, per backend, OR the intrinsics get
adjusted to the source ctor rep. Verify each: Posix/Zone/Month/Weekday/ZoneName;
Seed/Generator; Http.Error/Body/Expect/Progress/Response/Metadata/Header/Part.

### Prerequisite: Process cancellation (see above) — needed for Time/Http kill.

## Milestones (each = all 3 backends + differential test + commit)
1. **Time** — bundled Time.elm, Elm.Kernel.Time prims (now/here/setInterval/
   getZoneName + calendar), Process cancellation, remove SubTime dispatch.
   STATUS: DONE on all 3 backends (JS/native/wasm-gc). `Time.every` fires through
   the manager and unsubscribe cancels the timer (no leak) on every backend;
   full suite green. Task cancellation (`Process.spawn`/`kill`) added to all three
   schedulers. Old `SubTime`/`ST_TIME` dispatch + calendar intrinsics removed.
   Single-module `compile` API can't host effect modules → Time-using single-
   module tests routed through the project path (`compile_via_project`).
2. **Random** — bundled Random.elm (init depends on Time.now), Elm.Kernel.Random
   backing for generators/step/seed, remove CmdRandom dispatch.
3. **Http** — bundled Http.elm, Elm.Kernel.Http prims (toTask/expect/…), remove
   CmdHttp dispatch. Biggest (progress track sub, cancel, multipart).
   STATUS: Time + Random DONE (committed). Http design (below) ready to implement.

## Http implementation design (milestone 3)

Same pattern as Random (opaque types + Elm.Kernel backing + real manager). The
real elm/http ALREADY declares `Header`/`Body`/`Part`/`Expect`/`Resolver` as
opaque (kernel-built) — keep them opaque, delegate builders to
`Elm.Kernel.Http.*` (alm's existing `$Http$*` builders, rekeyed). alm's runtime
reps already MATCH the source ctors: `Error` (BadUrl/Timeout/NetworkError/
BadStatus/BadBody), `Response` (BadUrl_/Timeout_/NetworkError_/BadStatus_ Meta
body/GoodStatus_ Meta body), `Metadata` = {url,statusCode,statusText,headers}.
Declared (exposed) source types: Error(..), Response(..), Progress(..)=Sending
{sent,size}|Receiving{received,size}, Metadata alias.

- **Bundled Http.elm** (`effect module Http where { command = MyCmd, subscription = MySub }`):
  all pure builders delegate to `Elm.Kernel.Http.*`; `get`/`post`/`request` →
  `command (Request {method,headers,url,body,expect,timeout,tracker})`;
  `track tracker toMsg` → `subscription (MySub tracker toMsg)`; `cancel tracker`
  → `command (Cancel tracker)`. `MyCmd = Cancel String | Request <config>`.
  Manager: `init = Task.succeed (State Dict.empty [])`; `onEffects` = updateReqs
  (per Request: `Process.spawn (Elm.Kernel.Http.toTask router (Platform.sendToApp
  router) req)`, track pid by tracker; per Cancel: `Process.kill` the pid);
  `onSelfMsg (tracker,progress)` delivers to matching MySub via sendToApp.
- **Elm.Kernel.Http.toTask(router, sendToApp, req)** (JS): reuse alm's
  `_Http_makeTask(req)` → response, apply `req.expect.handle(response)` → Result
  → `req.expect.toMsg(result)`, then RUN `sendToApp(msg)` task; cancellable
  (AbortController.abort on Process.kill via the canceller the binding returns —
  same Process.spawn/kill mechanism Time/Http share). Progress via sendToSelf is
  limited by fetch; track is a real sub but may only deliver coarse/no progress
  (document it) — the DISPATCH is a real manager either way.
- **Rekey** JS `$Http$*` pure builders → `$Elm$Kernel$Http$*`; REMOVE the
  `CmdHttp` runCmd branch + `$Http$request`/`riskyRequest` old leaf + no-op
  `$Http$track`/`$Http$cancel`. Config `headers` is now an Elm `List Header`
  (source), so `_Http_makeTask` must `_List_toArray` them.
- **native**: Http is currently a no-op (native CLI has no fetch). Wire the
  manager (request→command→onEffects→Process.spawn toTask); `Elm.Kernel.Http.toTask`
  stays unsupported/failing on native as today — DO NOT add an HTTP client dep.
  The effect DISPATCH becomes a real manager; actual I/O remains native-unsupported.
- **wasm-gc**: rekey Http builders to Elm.Kernel.Http; route request through the
  manager (LEAF) instead of CMD_HTTP(3); `Elm.Kernel.Http.toTask` reuses the
  existing `host_http` + `emit_http_response` settlement, driven from the manager.
  Remove CMD_HTTP branch in emit_run_cmd/emit_cmd_map.
- Types stay: Http.Error/Body/Expect/Part/Header/Response/Metadata/Progress/
  Resolver come from the bundled source now (remove from builtins). Bytes/File
  deps: keep expectBytes/bytesBody delegating to kernel (Bytes available).
- Tests: differential get/expectString + a mock; convert any single-module Http
  tests (runtime_test http_*) to run_proj.

## native + wasm-gc

(to be filled from the backend-map subagents — Process/task model, where
CmdHttp/SubTime/Random cmd are dispatched, and how to add cancellation +
manager registration there.)

## Tests
Extend `effect_manager_test.rs` style: differential JS/native/wasm-gc programs
using `Time.every` (subscription fires), `Random.generate` (cmd → msg),
`Http`-shaped cmd (mock), asserting outputs agree. Verify no leaked timers
(subscribe then unsubscribe).
