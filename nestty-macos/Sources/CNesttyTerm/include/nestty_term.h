// Phase 1 scaffold — see nestty-term/src/lib.rs and
// docs/macos-renderer-migration-plan.md §D3.
//
// Pointer ownership:
//   nestty_term_create -> NesttyHandle*       Rust-owned, free with nestty_term_destroy
//   nestty_term_snapshot -> NesttySnapshot*   Rust-owned, free with nestty_snapshot_destroy
//   *const NesttyRun from row_runs            Borrowed from snapshot, valid until snapshot_destroy
//   *const uint8_t   from row_utf8            Borrowed from snapshot, same lifetime
//   nestty_term_version() -> const char*      Static, no free

#ifndef NESTTY_TERM_H
#define NESTTY_TERM_H

#include <stddef.h>
#include <stdint.h>

typedef struct NesttyHandle NesttyHandle;
typedef struct NesttySnapshot NesttySnapshot;

typedef struct {
    uint16_t start_col;        // inclusive
    uint16_t end_col;          // exclusive; wide CJK / ZWJ emoji span both cells in one run
    uint32_t utf8_offset;      // byte offset into the row's utf8 buffer
    uint32_t utf8_len;
    // Tagged color: MSB is the discriminator.
    //   0x00_00_00_00            default (renderer materializes theme fg/bg)
    //   0x01_00_00_NN            indexed N (0..15 palette, 16..231 cube, 232..255 grayscale)
    //   0xFF_RR_GG_BB            direct RGB (always opaque)
    uint32_t fg_rgba;
    uint32_t bg_rgba;          // same encoding; 0 = default-bg sentinel
    uint16_t flags;
    uint8_t  underline_style;  // 0=none 1=single 2=double 3=curly 4=dotted 5=dashed
    uint8_t  reserved;
    uint32_t underline_color_rgba; // same encoding as fg_rgba; 0 = use fg
    uint32_t hyperlink_id;     // 0 = none; opaque key into separate hyperlink table (Phase 4+)
} NesttyRun;

// Flags bit layout — must match nestty_term::flags:
//   1 << 0  BOLD
//   1 << 1  ITALIC
//   1 << 2  UNDERLINE
//   1 << 3  INVERSE          (reverse video — fg/bg swap after default-bg materialize)
//   1 << 4  DIM
//   1 << 5  STRIKE
//   1 << 6  BLINK
//   1 << 7  WIDE_LEADING
//   1 << 8  WIDE_TRAILING

typedef struct {
    uint16_t row;
    uint16_t col;
    uint8_t  style;     // 0=hidden 1=block 2=bar 3=underline
    uint8_t  blink;     // 0=steady 1=blink
    uint16_t reserved;
} NesttyCursor;

// --- Terminal lifecycle ---

NesttyHandle* nestty_term_create(uint16_t cols, uint16_t rows,
                                  const char* shell, const char* cwd);
void nestty_term_destroy(NesttyHandle* handle);

void nestty_term_input(NesttyHandle* handle, const uint8_t* bytes, size_t len);
void nestty_term_resize(NesttyHandle* handle, uint16_t cols, uint16_t rows);

// --- Snapshot ---

NesttySnapshot* nestty_term_snapshot(NesttyHandle* handle);
void nestty_snapshot_destroy(NesttySnapshot* snap);

uint16_t nestty_snapshot_rows(const NesttySnapshot* snap);
uint16_t nestty_snapshot_cols(const NesttySnapshot* snap);

// Sets *out_runs to a borrowed pointer; returns the run count. Both
// the pointer and the underlying memory live until snapshot_destroy.
size_t nestty_snapshot_row_runs(const NesttySnapshot* snap, uint16_t row,
                                 const NesttyRun** out_runs);

// Borrowed pointer to the row's utf8 bytes; same lifetime.
const uint8_t* nestty_snapshot_row_utf8(const NesttySnapshot* snap, uint16_t row,
                                         size_t* out_len);

void nestty_snapshot_cursor(const NesttySnapshot* snap, NesttyCursor* out);

// Static string, no free required.
const char* nestty_term_version(void);

#endif
