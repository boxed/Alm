// Shared browser test harness. Drives the compiled app (either compiler)
// through real DOM events and writes PASS/FAIL lines into #results.
// Steps run asynchronously with a double-rAF wait between them, because
// official Elm batches renders on requestAnimationFrame.

(function () {
    var results = [];
    function report(name, ok, detail) {
        results.push((ok ? 'PASS ' : 'FAIL ') + name + (ok ? '' : ' :: ' + detail));
    }
    function assertEq(name, actual, expected) {
        report(name, actual === expected, 'expected ' + JSON.stringify(expected) + ', got ' + JSON.stringify(actual));
    }
    function $(id) { return document.getElementById(id); }
    function textOf(id) { var n = $(id); return n ? n.textContent : '<missing #' + id + '>'; }
    function click(id) {
        $(id).dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
    }
    function finish() {
        var pre = document.createElement('pre');
        pre.id = 'results';
        pre.textContent = results.join('\n') + '\nTOTAL ' +
            results.filter(function (r) { return r.indexOf('PASS') === 0; }).length + '/' + results.length;
        document.body.appendChild(pre);
        document.title = 'TESTS-DONE';
    }

    var app, echoes = [], beta = null, customEvent = null;

    var steps = [
        function boot() {
            var mount = document.createElement('div');
            document.body.appendChild(mount);
            var ns = window.Elm.Main;
            // alm exposes every top-level value, so prefer the Program
            // wrapper at ns.main.init; official Elm has only ns.init.
            var initFn = ns.main && ns.main.init
                ? ns.main.init.bind(ns.main)
                : ns.init.bind(ns);
            app = initFn({ node: mount, flags: null });
            app.ports.toJs.subscribe(function (s) { echoes.push(s); });
        },
        function initialRender() {
            assertEq('initial count', textOf('count'), '0');
            assertEq('initial keyed order', textOf('keyed-list'), 'alphabetagamma');
            assertEq('lazy badge initial', textOf('lazy-badge'), 'badge:0');
            report('svg renders', !!document.querySelector('#the-svg circle'), 'no circle in svg');
            var svg = $('the-svg');
            assertEq('svg namespace', svg.namespaceURI, 'http://www.w3.org/2000/svg');
            assertEq('svg viewBox attr', svg.getAttribute('viewBox'), '0 0 100 100');
            report('panel hidden initially', !$('panel'), 'panel should not exist yet');
            click('inc');
        },
        function afterOneClick() {
            assertEq('count after click', textOf('count'), '1');
            click('inc');
        },
        function afterTwoClicks() {
            assertEq('count after two clicks', textOf('count'), '2');
            click('child-button');
        },
        function afterChildClick() {
            assertEq('Html.map wraps child msg', textOf('count'), '12');
            assertEq('lazy badge updates', textOf('lazy-badge'), 'badge:12');
            beta = $('keyed-list').childNodes[1];
            beta.__marker = 'the-real-beta';
            click('reorder');
        },
        function afterReorder() {
            assertEq('keyed order after reverse', textOf('keyed-list'), 'gammabetaalpha');
            report('keyed reorder preserves node identity',
                $('keyed-list').childNodes[1].__marker === 'the-real-beta',
                'the beta <li> was rebuilt instead of moved');
            click('insert');
        },
        function afterInsert() {
            assertEq('keyed insert at front', textOf('keyed-list'), 'newcomergammabetaalpha');
            report('keyed insert preserves beta identity',
                $('keyed-list').childNodes[2].__marker === 'the-real-beta',
                'beta rebuilt on unrelated insert');
            click('remove');
        },
        function afterRemove() {
            assertEq('keyed remove second', textOf('keyed-list'), 'newcomerbetaalpha');
            report('keyed remove preserves beta identity',
                $('keyed-list').childNodes[1].__marker === 'the-real-beta',
                'beta rebuilt on removal of sibling');
            var box = $('text-in');
            box.value = 'stressed';
            box.dispatchEvent(new Event('input', { bubbles: true }));
        },
        function afterInput() {
            assertEq('onInput flows through model', textOf('text-out'), 'desserts');
            assertEq('input value controlled', $('text-in').value, 'stressed');
            var check = $('check-in');
            check.checked = true;
            check.dispatchEvent(new Event('change', { bubbles: true }));
        },
        function afterCheck() {
            assertEq('onCheck flows through model', textOf('check-out'), 'on');
            $('the-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
        },
        function afterSubmit() {
            assertEq('onSubmit handled', textOf('submit-out'), '1');
            click('stopper');
        },
        function afterStopper() {
            assertEq('stopPropagationOn blocks bubbling', textOf('click-out'), '0/1');
            click('bubbler');
        },
        function afterBubbler() {
            assertEq('normal click bubbles to outer', textOf('click-out'), '1/2');
            customEvent = new MouseEvent('click', { bubbles: true, cancelable: true });
            $('custom-btn').dispatchEvent(customEvent);
        },
        function afterCustom() {
            assertEq('custom handler fires', textOf('custom-out'), 'custom');
            report('custom preventDefault applied', customEvent.defaultPrevented,
                'default was not prevented');
            click('toggle-panel');
        },
        function afterPanelShow() {
            report('panel appears', !!$('panel') && textOf('panel') === 'panel-content',
                'panel missing after toggle');
            click('toggle-panel');
        },
        function afterPanelHide() {
            report('panel removed', !$('panel'), 'panel still present after second toggle');
            var styled = $('styled');
            assertEq('initial style', styled.style.color, 'rgb(0, 0, 255)');
            assertEq('initial class', styled.getAttribute('class'), 'cold');
            click('toggle-style');
        },
        function afterStyleOn() {
            var styled = $('styled');
            assertEq('style patched', styled.style.color, 'rgb(255, 0, 0)');
            assertEq('class patched', styled.getAttribute('class'), 'hot');
            report('disabled property set', styled.disabled === true, 'disabled not set');
            click('toggle-style');
        },
        function afterStyleOff() {
            var styled = $('styled');
            assertEq('style patched back', styled.style.color, 'rgb(0, 0, 255)');
            report('disabled property cleared', !styled.disabled, 'disabled still set');
            app.ports.fromJs.send('ping');
        }
    ];

    var index = 0;
    function runNext() {
        if (index >= steps.length) {
            waitForAsync(0);
            return;
        }
        try {
            steps[index++]();
        } catch (e) {
            report('step ' + steps[index - 1].name, false, 'threw ' + e.message);
            finish();
            return;
        }
        // Two animation frames: one for Elm's rAF-batched render, one spare.
        requestAnimationFrame(function () {
            requestAnimationFrame(runNext);
        });
    }

    function waitForAsync(waited) {
        var asleep = textOf('sleep-out') !== 'awake';
        var noEcho = echoes.length === 0;
        if ((asleep || noEcho) && waited < 100) {
            setTimeout(function () { waitForAsync(waited + 1); }, 20);
            return;
        }
        try {
            assertEq('Process.sleep task completes', textOf('sleep-out'), 'awake');
            assertEq('incoming port updates model', textOf('port-out'), 'ping');
            assertEq('outgoing port called', echoes.join('|'), 'echo:ping');
        } catch (e) {
            report('async assertions', false, 'threw ' + e.message);
        }
        finish();
    }

    window.addEventListener('error', function (e) {
        report('window.onerror', false, e.message);
    });
    runNext();
})();
