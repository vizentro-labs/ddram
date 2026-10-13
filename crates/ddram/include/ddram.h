/*
 * ddram — extend DRAM with NVMe/SSD capacity (C API).
 * Copyright 2026 Vizentro Labs
 * SPDX-License-Identifier: Apache-2.0
 *
 * Link against libddram.a / ddram.lib (static) or libddram.so / .dylib / .dll.
 * Static linking also needs: Linux -lpthread -ldl -lm; macOS -framework IOKit
 * -framework CoreFoundation; Windows ntdll.lib userenv.lib ws2_32.lib.
 */
#ifndef DDRAM_H
#define DDRAM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ddram_region ddram_region;
typedef struct ddram_plan ddram_plan;

/* Zero fields take defaults. */
typedef struct ddram_options {
    uint64_t size;             /* region bytes (ignored when read_only) */
    uint64_t ram_budget;       /* bytes of RAM for resident blocks */
    uint64_t block_size;       /* paging unit, power of two; default 1 MiB */
    uint32_t prefetch_depth;   /* readahead blocks; default 16 */
    uint32_t io_threads;       /* background I/O threads; default 4 */
    uint64_t write_rate_limit; /* write-back cap in bytes/s; 0 = unlimited */
    uint32_t policy;           /* 0 auto, 1 clock, 2 cyclic */
    uint32_t read_only;        /* map an existing file read-only */
    uint32_t persist;          /* write back dirty blocks on destroy */
    uint32_t buffered_io;      /* 1 = use the OS page cache (not recommended) */
} ddram_options;

typedef struct ddram_stats {
    uint64_t hard_faults, soft_faults, write_faults;
    uint64_t loads, prefetch_loads;
    uint64_t bytes_read, bytes_written;
    uint64_t evictions, writebacks, resident_blocks;
    uint64_t plan_hits, plan_misses, plan_position;
} ddram_stats;

enum ddram_advice {
    DDRAM_ADV_NORMAL = 0,
    DDRAM_ADV_RANDOM = 1,
    DDRAM_ADV_SEQUENTIAL = 2,
    DDRAM_ADV_CYCLIC = 3,   /* repeated sweeps larger than RAM (LLM weights) */
    DDRAM_ADV_WILLNEED = 4, /* load in the background now */
    DDRAM_ADV_COLD = 5,     /* write back if dirty and drop from RAM */
    DDRAM_ADV_HOT = 6       /* keep resident */
};

/* `path` is the backing file, or a directory for an auto-deleted temp file.
 * Returns NULL on error (message on stderr). */
ddram_region *ddram_region_create(const char *path, const ddram_options *opts);
void ddram_region_destroy(ddram_region *r);

/* Transparent view: an ordinary pointer valid for ddram_region_len() bytes.
 * Note: system calls that write into non-resident blocks (e.g. read(2) into
 * the region) fail with EFAULT; use ddram_pin() or touch the memory first. */
uint8_t *ddram_region_ptr(const ddram_region *r);
uint64_t ddram_region_len(const ddram_region *r);

/* The RAM arena holding resident blocks. Register it once with a GPU/DMA API
 * (cudaHostRegister, cuFileBufRegister, Metal newBufferWithBytesNoCopy, a
 * DirectStorage destination); pinned blocks are then at arena + arena_off. */
void ddram_region_arena(const ddram_region *r, uint8_t **ptr, uint64_t *len);

int ddram_advise(const ddram_region *r, uint64_t off, uint64_t len, int advice);
void ddram_prefetch(const ddram_region *r, uint64_t off, uint64_t len);

/* Pin a block in RAM (it cannot be evicted until unpinned). */
uint8_t *ddram_pin(const ddram_region *r, uint64_t block, int writable, uint64_t *arena_off);
void ddram_unpin(const ddram_region *r, uint64_t block);

int ddram_flush(const ddram_region *r);
void ddram_stats_get(const ddram_region *r, ddram_stats *out);

/* Access plans: declare the order in which the region will be touched (a
 * list of byte ranges, optionally repeating). With a plan installed the pager
 * prefetches exactly what comes next and evicts the block whose next planned
 * use is furthest away (Belady's optimal policy). Ranges should be the units
 * consumed together (a layer, an expert, an asset). */
ddram_plan *ddram_plan_new(int cyclic);
void ddram_plan_free(ddram_plan *p);
void ddram_plan_add(ddram_plan *p, uint64_t off, uint64_t len);
void ddram_plan_set_lookahead(ddram_plan *p, uint64_t bytes); /* 0 = budget/4 */
uint64_t ddram_plan_len(const ddram_plan *p);
int ddram_plan_save(const ddram_plan *p, const char *path);
ddram_plan *ddram_plan_load(const char *path);
/* The region copies the plan; free it afterwards if no longer needed. */
int ddram_set_plan(const ddram_region *r, const ddram_plan *p);
void ddram_clear_plan(const ddram_region *r);

/* Learned plans: record the order in which blocks are faulted in, then turn
 * the trace into a plan (one pass over everything recorded) to install now
 * and save for the next run. */
void ddram_record(const ddram_region *r, uint64_t max_entries);
void ddram_stop_recording(const ddram_region *r);
ddram_plan *ddram_recorded_plan(const ddram_region *r, int cyclic); /* NULL if none */

#ifdef __cplusplus
}
#endif
#endif /* DDRAM_H */
