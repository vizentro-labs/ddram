/* Copyright 2026 Vizentro Labs
 * SPDX-License-Identifier: Apache-2.0 */
/* A C program using 4x more memory than its RAM budget.
 * Build: see examples/c/Makefile */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "ddram.h"

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : ".";
    ddram_options o;
    memset(&o, 0, sizeof o);
    o.size = 256ull << 20;       /* 256 MiB of memory ... */
    o.ram_budget = 64ull << 20;  /* ... in 64 MiB of RAM */
    ddram_region *r = ddram_region_create(dir, &o);
    if (!r) return 1;

    uint64_t *p = (uint64_t *)ddram_region_ptr(r);
    uint64_t n = ddram_region_len(r) / 8;
    for (uint64_t i = 0; i < n; i++) p[i] = i * 2654435761u;
    uint64_t bad = 0;
    for (uint64_t i = 0; i < n; i++) bad += p[i] != i * 2654435761u;

    /* Explicit API: pin block 3 (e.g. to upload it to a GPU from the arena). */
    uint64_t off;
    uint8_t *blk = ddram_pin(r, 3, 0, &off);
    uint8_t *arena; uint64_t alen;
    ddram_region_arena(r, &arena, &alen);
    int same = blk == arena + off;
    ddram_unpin(r, 3);

    ddram_stats s;
    ddram_stats_get(r, &s);
    printf("verified %llu words, %llu errors; evictions=%llu writebacks=%llu pin_in_arena=%d\n",
           (unsigned long long)n, (unsigned long long)bad, (unsigned long long)s.evictions,
           (unsigned long long)s.writebacks, same);
    ddram_region_destroy(r);
    return bad != 0 || !same;
}
