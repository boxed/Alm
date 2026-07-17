/* Weak stubs for the MMTk binding's C ABI (crates/alm-mmtk).
 *
 * Always linked into native programs. When the real binding is built and
 * linked (its strong symbols in a merged .o), it overrides these; otherwise
 * these satisfy the runtime's references so the link succeeds. They abort if
 * ever actually called, which only happens if ALM_GC=mmtk without the binding
 * built in. */
#include <stdlib.h>
#include <stddef.h>

__attribute__((weak)) void almmtk_init(size_t heap_bytes) {
    (void)heap_bytes;
    abort();
}

__attribute__((weak)) void *almmtk_alloc(size_t size, size_t align) {
    (void)size;
    (void)align;
    abort();
}
