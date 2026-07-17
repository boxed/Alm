/* Register-spill trampoline for the custom Immix-lite collector.
 *
 * `setjmp` spills the callee-saved registers into `buf` (which lives on the
 * stack), so a conservative scan of the stack from `&buf` upward also covers
 * any live heap pointers held in registers. We then hand the collector the
 * current stack pointer (`&buf`, the lowest live address) so it scans
 * `[&buf, stack_base)`. In C because register-spill + `setjmp` semantics are
 * awkward to express portably in Rust. */
#include <setjmp.h>

extern void immix_mark_and_sweep(void *sp);

void immix_collect_roots(void) {
    jmp_buf buf;
    setjmp(buf);
    immix_mark_and_sweep((void *)&buf);
}
