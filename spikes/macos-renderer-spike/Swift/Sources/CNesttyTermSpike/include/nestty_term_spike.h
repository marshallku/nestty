// Phase 0 spike — see ../../src/lib.rs and
// docs/macos-renderer-migration-plan.md §Phase 0 / §D3.
//
// All `NesttyRun` fields are POD; no pointer-into-snapshot indirection
// per run. Row's utf8 buffer is fetched once per row via
// `nestty_snapshot_row_utf8`, then each run indexes into it with
// `utf8_offset` + `utf8_len`.

#ifndef NESTTY_TERM_SPIKE_H
#define NESTTY_TERM_SPIKE_H

#include <stddef.h>
#include <stdint.h>

typedef struct NesttySnapshot NesttySnapshot;

typedef struct {
    uint16_t start_col;        // inclusive
    uint16_t end_col;          // exclusive; wide CJK / ZWJ emoji span both cells in one run
    uint32_t utf8_offset;      // byte offset into the row's utf8 buffer
    uint32_t utf8_len;         // byte length within the row buffer
    uint32_t fg_rgba;          // RGBA, 0xRRGGBBAA
    uint32_t bg_rgba;          // 0 = default-bg sentinel; renderer materializes
    uint16_t flags;            // see flags table below
    uint8_t  underline_style;  // 0=none 1=single 2=double 3=curly 4=dotted 5=dashed
    uint8_t  reserved;
    uint32_t underline_color_rgba; // 0 = use fg
    uint32_t hyperlink_id;     // 0 = none, opaque key into separate hyperlink table (Phase 4+)
} NesttyRun;

// Flags bit layout (must match nestty_term_spike::flags):
//   1 << 0  BOLD
//   1 << 1  ITALIC
//   1 << 2  UNDERLINE
//   1 << 3  INVERSE  (reverse video)
//   1 << 4  DIM
//   1 << 5  STRIKE
//   1 << 6  BLINK
//   1 << 7  WIDE_LEADING
//   1 << 8  WIDE_TRAILING

NesttySnapshot* nestty_snapshot_create_fixture(void);
void nestty_snapshot_destroy(NesttySnapshot* snap);

uint16_t nestty_snapshot_rows(const NesttySnapshot* snap);

// Sets *out_runs to a borrowed pointer; returns the run count. Both
// the pointer and the underlying memory live until
// nestty_snapshot_destroy.
size_t nestty_snapshot_row_runs(const NesttySnapshot* snap, uint16_t row,
                                 const NesttyRun** out_runs);

// Borrowed pointer to the row's utf8 bytes; same lifetime contract.
const uint8_t* nestty_snapshot_row_utf8(const NesttySnapshot* snap, uint16_t row,
                                         size_t* out_len);

// Static string, no free required.
const char* nestty_spike_version(void);

// Symbol from the existing nestty-ffi staticlib — pulled in only to
// prove dual-staticlib linking (the Rust-std symbol-collision risk
// from §R7). Declared here so the spike doesn't need a separate
// CNesttyFFI module just for one call.
const char* nestty_ffi_version(void);

#endif
