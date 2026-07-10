/* Non-local exit for Bytes.Decode failures — the native twin of the JS
 * runtime's exception-based `_Bytes_decode` (`try { ... } catch { return
 * Nothing }`).
 *
 * elm/bytes' `map`/`andThen`/`loop` are plain Elm compiled as-is: they thread
 * `(offset, value)` pairs and apply user callbacks unconditionally, relying on
 * a failed read ABORTING the whole decode (in JS, `_Bytes_read_*` throws). A
 * sentinel return instead lets those combinators keep running and hand the
 * failure dummy to user code, which inspects it at an arbitrary layout —
 * dereferencing a tagged int as a tuple/record pointer (SIGSEGV at 0x1).
 *
 * setjmp/longjmp reproduces the JS semantics. It lives in C because `setjmp`
 * requires `returns_twice` codegen, which C compilers apply automatically and
 * Rust does not guarantee for a plain `extern` declaration. `_setjmp` (no
 * signal-mask save) suffices: no signal handling is involved.
 *
 * Decodes nest (a decoder can run another decode inside — e.g. backtracking
 * `oneOf` wrappers), so the current jump target is saved and restored around
 * each try. The runtime is single-threaded (one Elm worker thread), so a plain
 * static suffices.
 */
#include <setjmp.h>
#include <stdlib.h>

typedef unsigned long long u64;

static jmp_buf *alm_bytes_jmp_cur = 0;

u64 alm_bytes_try(u64 (*run)(u64, u64), u64 decoder, u64 bytes, u64 *failed) {
    jmp_buf buf;
    jmp_buf *prev = alm_bytes_jmp_cur;
    alm_bytes_jmp_cur = &buf;
    u64 result;
    if (_setjmp(buf) == 0) {
        result = run(decoder, bytes);
        *failed = 0;
    } else {
        result = 0;
        *failed = 1;
    }
    alm_bytes_jmp_cur = prev;
    return result;
}

void alm_bytes_fail(void) {
    if (alm_bytes_jmp_cur) {
        _longjmp(*alm_bytes_jmp_cur, 1);
    }
    /* A failing read outside any decode should be impossible; abort loudly
     * rather than continue with a corrupt value. */
    abort();
}
