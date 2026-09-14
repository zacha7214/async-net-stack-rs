/* The Python frontend uses real acquire/release operations on shared indices. */
#include <stdint.h>
#include <stdatomic.h>
uint16_t ring_load(const void *p) {
    return atomic_load_explicit((const _Atomic uint16_t *)p, memory_order_acquire);
}
void ring_store(void *p, uint16_t value) {
    atomic_store_explicit((_Atomic uint16_t *)p, value, memory_order_release);
}
