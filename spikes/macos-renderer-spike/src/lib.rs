//! Throwaway spike for the Phase 0 ABI shape validation. NOT production:
//! no `alacritty_terminal` yet, no PTY, no real grid. Fixture snapshot
//! only — proves the FFI cell-attribute coverage, row-contiguous utf8
//! ownership, and dual-staticlib linking against `nestty-ffi` work.
//!
//! See `docs/macos-renderer-migration-plan.md` § Phase 0 for what this
//! is supposed to validate. Once Phase 0 is signed off, delete this
//! whole crate and start Phase 1's real `nestty-term/`.

use std::ffi::{CStr, c_char};
use std::ptr;

/// Mirrors §D3 of the migration plan. `#[repr(C)]` so Swift sees the
/// same layout. POD across the FFI boundary; per-cell allocation
/// avoided by referencing into the row's contiguous utf8 buffer via
/// offset/len.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NesttyRun {
    pub start_col: u16,
    pub end_col: u16,
    pub utf8_offset: u32,
    pub utf8_len: u32,
    pub fg_rgba: u32,
    pub bg_rgba: u32, // sentinel 0 = default-bg
    pub flags: u16,
    pub underline_style: u8,
    pub reserved: u8,
    pub underline_color_rgba: u32,
    pub hyperlink_id: u32,
}

pub mod flags {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const INVERSE: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    pub const STRIKE: u16 = 1 << 5;
    pub const BLINK: u16 = 1 << 6;
    pub const WIDE_LEADING: u16 = 1 << 7;
    pub const WIDE_TRAILING: u16 = 1 << 8;
}

struct Row {
    utf8: Vec<u8>,
    runs: Vec<NesttyRun>,
}

pub struct Snapshot {
    rows: Vec<Row>,
}

/// Heap-allocate a snapshot containing one fixture row that exercises
/// the rendering edge cases the plan calls out: red text, reverse-video
/// (the cursor-over-image proof case), wide CJK, ZWJ emoji, ligature
/// input. Returned as an opaque pointer; caller must
/// `nestty_snapshot_destroy` exactly once.
#[unsafe(no_mangle)]
pub extern "C" fn nestty_snapshot_create_fixture() -> *mut Snapshot {
    // Row utf8 byte layout (cells described in column comments):
    //   "R"        col 0          red on default bg
    //   "I"        col 1          inverse video (default fg/bg swapped)
    //   "한"       cols 2-3       cyan wide CJK
    //   "👨‍👩‍👧"   cols 4-5       ZWJ family emoji, wide
    //   "fi != !=" cols 6-13      ligature input
    let utf8: Vec<u8> = b"RI\xed\x95\x9c\xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7fi != !=".to_vec();

    // Offsets into utf8 (computed from above byte layout):
    let off_r = 0u32;
    let off_i = 1u32;
    let off_han = 2u32;
    let off_emoji = 5u32; // 한 = 3 bytes (ed 95 9c)
    let off_ligatures = 23u32; // emoji ZWJ = 18 bytes (f0 9f 91 a8 e2 80 8d f0 9f 91 a9 e2 80 8d f0 9f 91 a7)
    let total_len = utf8.len() as u32;

    let runs = vec![
        NesttyRun {
            start_col: 0,
            end_col: 1,
            utf8_offset: off_r,
            utf8_len: 1,
            fg_rgba: 0xff5555ff, // bright red, opaque
            bg_rgba: 0,          // default-bg sentinel — renderer materializes
            flags: 0,
            underline_style: 0,
            reserved: 0,
            underline_color_rgba: 0,
            hyperlink_id: 0,
        },
        NesttyRun {
            start_col: 1,
            end_col: 2,
            utf8_offset: off_i,
            utf8_len: 1,
            fg_rgba: 0xffffffff, // white
            bg_rgba: 0,          // default — but inverse flag swaps fg/bg post-materialize
            flags: flags::INVERSE,
            underline_style: 0,
            reserved: 0,
            underline_color_rgba: 0,
            hyperlink_id: 0,
        },
        NesttyRun {
            start_col: 2,
            end_col: 4, // wide CJK occupies 2 cells
            utf8_offset: off_han,
            utf8_len: 3,
            fg_rgba: 0x55ffffff, // cyan-ish
            bg_rgba: 0,
            flags: flags::WIDE_LEADING,
            underline_style: 0,
            reserved: 0,
            underline_color_rgba: 0,
            hyperlink_id: 0,
        },
        NesttyRun {
            start_col: 4,
            end_col: 6, // ZWJ emoji also 2 cells wide
            utf8_offset: off_emoji,
            utf8_len: 18,
            fg_rgba: 0xffffffff,
            bg_rgba: 0,
            flags: flags::WIDE_LEADING,
            underline_style: 0,
            reserved: 0,
            underline_color_rgba: 0,
            hyperlink_id: 0,
        },
        NesttyRun {
            start_col: 6,
            end_col: 14, // 8 ASCII cells
            utf8_offset: off_ligatures,
            utf8_len: total_len - off_ligatures,
            fg_rgba: 0xeeeeeeff,
            bg_rgba: 0,
            flags: 0,
            underline_style: 1, // single underline
            reserved: 0,
            underline_color_rgba: 0xff55aaff, // pink underline color override
            hyperlink_id: 0,
        },
    ];

    let snap = Box::new(Snapshot {
        rows: vec![Row { utf8, runs }],
    });
    Box::into_raw(snap)
}

/// Free a snapshot previously returned from `nestty_snapshot_create_fixture`.
/// Safe to pass NULL (no-op).
///
/// # Safety
///
/// Must be called exactly once per snapshot; calling twice is UB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_destroy(snap: *mut Snapshot) {
    if snap.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(snap) };
}

#[unsafe(no_mangle)]
pub extern "C" fn nestty_snapshot_rows(snap: *const Snapshot) -> u16 {
    if snap.is_null() {
        return 0;
    }
    let s = unsafe { &*snap };
    s.rows.len() as u16
}

/// Hand the caller a borrowed pointer to the row's run array. Pointer
/// is valid until `nestty_snapshot_destroy`. Returns 0 if row is out of
/// range (and leaves `*out_runs` untouched).
///
/// # Safety
///
/// `snap` must come from `nestty_snapshot_create_fixture`. `out_runs`
/// must point to writable storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_row_runs(
    snap: *const Snapshot,
    row: u16,
    out_runs: *mut *const NesttyRun,
) -> usize {
    if snap.is_null() || out_runs.is_null() {
        return 0;
    }
    let s = unsafe { &*snap };
    let Some(row_data) = s.rows.get(row as usize) else {
        return 0;
    };
    unsafe { *out_runs = row_data.runs.as_ptr() };
    row_data.runs.len()
}

/// Hand back a borrowed pointer to the row's utf8 buffer + its length.
/// Same lifetime contract as `nestty_snapshot_row_runs`.
///
/// # Safety
///
/// `snap` must come from `nestty_snapshot_create_fixture`. `out_len`
/// must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_row_utf8(
    snap: *const Snapshot,
    row: u16,
    out_len: *mut usize,
) -> *const u8 {
    if snap.is_null() || out_len.is_null() {
        unsafe {
            if !out_len.is_null() {
                *out_len = 0;
            }
        }
        return ptr::null();
    }
    let s = unsafe { &*snap };
    match s.rows.get(row as usize) {
        Some(row_data) => {
            unsafe { *out_len = row_data.utf8.len() };
            row_data.utf8.as_ptr()
        }
        None => {
            unsafe { *out_len = 0 };
            ptr::null()
        }
    }
}

/// Static version string (no allocation; pointer is valid for program
/// lifetime, no free required).
#[unsafe(no_mangle)]
pub extern "C" fn nestty_spike_version() -> *const c_char {
    static VERSION: &CStr = c"nestty-term-spike v0.0.1";
    VERSION.as_ptr()
}
