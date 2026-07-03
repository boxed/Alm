// Browser.application test harness: link interception, pushUrl, popstate.
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
        $(id).dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
    }
    function finish() {
        var pre = document.createElement('pre');
        pre.id = 'results';
        pre.textContent = results.join('\n') + '\nTOTAL ' +
            results.filter(function (r) { return r.indexOf('PASS') === 0; }).length + '/' + results.length;
        (document.documentElement || document.body).appendChild(pre);
        document.title = 'TESTS-DONE';
    }

    var steps = [
        function boot() {
            var ns = window.Elm.App;
            var initFn = ns.main && ns.main.init ? ns.main.init.bind(ns.main) : ns.init.bind(ns);
            initFn({ flags: null });
        },
        function initial() {
            assertEq('initial path rendered', textOf('path'), '/');
            assertEq('initial change count', textOf('changes'), '0');
            assertEq('document title set by view', document.title, 'page:/');
            click('link-two');
        },
        function afterLinkClick() {
            assertEq('link click intercepted and routed', textOf('path'), '/two');
            assertEq('browser URL updated by pushUrl', location.pathname, '/two');
            assertEq('title follows route', document.title, 'page:/two');
            assertEq('one url change', textOf('changes'), '1');
            click('go-three');
        },
        function afterPushUrl() {
            assertEq('programmatic pushUrl routes', textOf('path'), '/three');
            assertEq('browser URL is /three', location.pathname, '/three');
            history.back();
        },
        function afterBack() {
            assertEq('history.back routes via popstate', textOf('path'), '/two');
            assertEq('browser URL back to /two', location.pathname, '/two');
            assertEq('three url changes total', textOf('changes'), '3');
        }
    ];

    var index = 0;
    function runNext() {
        if (index >= steps.length) { finish(); return; }
        try {
            steps[index++]();
        } catch (e) {
            report('step ' + steps[index - 1].name, false, 'threw ' + e.message);
            finish();
            return;
        }
        // popstate needs a macrotask; give every step a generous delay.
        setTimeout(runNext, 120);
    }
    window.addEventListener('error', function (e) {
        report('window.onerror', false, e.message);
    });
    runNext();
})();
