//! C-ABI bridge wrapping `alacritty_terminal::Term` + its PTY event
//! loop. Consumers are `nestty-macos`'s renderer for now; the FFI is
//! deliberately host-agnostic so other UIs can attach later.
//!
//! See `docs/macos-renderer-migration-plan.md` §D3 for the ABI
//! contract and §Phase 2 for what's wired here.
//!
//! Pointer ownership:
//!
//! - `*mut NesttyHandle` / `*mut NesttySnapshot` — heap allocations
//!   owned by Rust; free with the matching `_destroy` function
//!   exactly once. Passing NULL to `_destroy` is a no-op.
//! - Borrowed `*const NesttyRun` / `*const u8` from snapshot
//!   accessors — valid until `nestty_snapshot_destroy`.
//! - Static strings (`nestty_term_version`) — valid for program
//!   lifetime, no free required.
//!
//! Threading: `Arc<FairMutex<Term>>` is shared between the PTY reader
//! thread (alacritty's `EventLoop`) and snapshot callers. Snapshots
//! lock briefly, copy out the visible rows, then release; renderers
//! consume them without holding the lock.

use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::thread::JoinHandle;

use alacritty_terminal::event::{VoidListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};

/// Mirrors §D3 of the migration plan. `#[repr(C)]` so the layout is
/// stable across the FFI boundary. Per-cell allocation is avoided by
/// referencing into the row's contiguous utf8 buffer via
/// `utf8_offset` + `utf8_len`.
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

/// Cursor position + style reported by `nestty_snapshot_cursor`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NesttyCursor {
    pub row: u16,
    pub col: u16,
    pub style: u8, // 0=hidden 1=block 2=bar 3=underline
    pub blink: u8,
    pub _reserved: u16,
}

struct Row {
    utf8: Vec<u8>,
    runs: Vec<NesttyRun>,
}

pub struct NesttyHandle {
    /// Shared between the PTY reader thread (alacritty's EventLoop)
    /// and snapshot callers. Lock duration must stay short on the
    /// snapshot path so the reader thread isn't starved.
    term: Arc<FairMutex<Term<VoidListener>>>,
    /// Sender into the event loop's mpsc — drives input writes,
    /// resize, and shutdown.
    sender: EventLoopSender,
    /// Reader thread that owns the PTY + parser loop. Joined in
    /// `nestty_term_destroy` after sending `Msg::Shutdown`.
    io_thread: Option<JoinHandle<(EventLoop<Pty, VoidListener>, State)>>,
}

pub struct NesttySnapshot {
    cols: u16,
    rows: Vec<Row>,
    cursor: NesttyCursor,
}

/// Create a terminal handle: spawn a PTY running the requested shell
/// (or the user's `$SHELL`), construct an `alacritty_terminal::Term`
/// at the given size, hand both to an `EventLoop` running in a
/// dedicated thread. Returns NULL on shell-spawn failure (e.g. shell
/// path missing).
///
/// # Safety
///
/// `shell` and `cwd` may be NULL or point to valid C strings. They
/// are copied; caller retains ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_create(
    cols: u16,
    rows: u16,
    shell: *const c_char,
    cwd: *const c_char,
) -> *mut NesttyHandle {
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);

    let mut tty_opts = TtyOptions::default();
    if !shell.is_null() {
        // SAFETY: caller contract — non-null pointer is a NUL-terminated C string.
        if let Ok(s) = unsafe { CStr::from_ptr(shell) }.to_str() {
            tty_opts.shell = Some(Shell::new(s.to_owned(), Vec::new()));
        }
    }
    if !cwd.is_null()
        && let Ok(s) = unsafe { CStr::from_ptr(cwd) }.to_str()
    {
        tty_opts.working_directory = Some(PathBuf::from(s));
    }

    let window_size = WindowSize {
        num_lines: safe_rows,
        num_cols: safe_cols,
        // Cell pixel dims are only used by programs that query
        // `TIOCGWINSZ` for pixel dimensions (mostly image protocols
        // like sixel/kitty). 1×1 is safe for the headless scaffold;
        // the renderer will resize with real values once it's drawing.
        cell_width: 1,
        cell_height: 1,
    };

    let pty = match tty::new(&tty_opts, window_size, 0) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };

    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    let term = Term::new(Config::default(), &term_size, VoidListener);
    let term = Arc::new(FairMutex::new(term));

    let event_loop = match EventLoop::new(Arc::clone(&term), VoidListener, pty, false, false) {
        Ok(el) => el,
        Err(_) => return ptr::null_mut(),
    };
    let sender = event_loop.channel();
    let io_thread = event_loop.spawn();

    Box::into_raw(Box::new(NesttyHandle {
        term,
        sender,
        io_thread: Some(io_thread),
    }))
}

/// Free a handle. Sends `Msg::Shutdown`, joins the reader thread,
/// drops the Term. Safe to pass NULL.
///
/// # Safety
///
/// Must be called exactly once per handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_destroy(handle: *mut NesttyHandle) {
    if handle.is_null() {
        return;
    }
    let mut handle = unsafe { Box::from_raw(handle) };
    // Best-effort shutdown — if the reader already exited (e.g. PTY
    // child died), the send fails but join still cleans up.
    let _ = handle.sender.send(Msg::Shutdown);
    if let Some(jh) = handle.io_thread.take() {
        let _ = jh.join();
    }
}

/// Feed input bytes to the PTY. The reader thread picks them up via
/// the event-loop channel.
///
/// # Safety
///
/// `bytes` must point to `len` readable bytes (or be NULL when len=0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_input(
    handle: *mut NesttyHandle,
    bytes: *const u8,
    len: usize,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    if len == 0 || bytes.is_null() {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(bytes, len) };
    let _ = h.sender.send(Msg::Input(slice.to_vec().into()));
}

/// Resize the PTY + Term grid. `cell_width`/`cell_height` left at 1
/// since this FFI is headless; real pixel sizes land when a renderer
/// attaches.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_resize(handle: *mut NesttyHandle, cols: u16, rows: u16) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let safe_cols = cols.max(1);
    let safe_rows = rows.max(1);

    let ws = WindowSize {
        num_lines: safe_rows,
        num_cols: safe_cols,
        cell_width: 1,
        cell_height: 1,
    };
    // `Msg::Resize` only forwards to `pty.on_resize` (so the child
    // process sees SIGWINCH). The Term grid is a separate resize and
    // must be done explicitly under the term lock — alacritty's own
    // app does this in `WindowContext::on_resize`.
    let _ = h.sender.send(Msg::Resize(ws));
    let term_size = TermSize::new(safe_cols as usize, safe_rows as usize);
    h.term.lock().resize(term_size);
}

/// Take a snapshot of the visible viewport. Lock duration is bounded
/// by the time it takes to walk `rows × cols` cells and copy them
/// into the snapshot's owned buffers.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_snapshot(handle: *mut NesttyHandle) -> *mut NesttySnapshot {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };

    let term = h.term.lock();
    let cols = term.columns() as u16;
    let rows_count = term.screen_lines() as u16;
    let grid = term.grid();

    let mut snapshot_rows = Vec::with_capacity(rows_count as usize);
    for line_idx in 0..rows_count as i32 {
        let line = Line(line_idx);
        let row = walk_row(grid, line, cols);
        snapshot_rows.push(row);
    }

    let cursor_point = term.grid().cursor.point;
    // `cursor_style()` honors DECSCUSR + vi-mode overrides; SHOW_CURSOR
    // gates whether anything renders. HollowBlock collapses to Block
    // here — the renderer draws hollow-on-blur as a separate concern
    // (window focus state, not a TUI request).
    let cs = term.cursor_style();
    let show_cursor = term
        .mode()
        .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR);
    let style = if !show_cursor {
        0
    } else {
        match cs.shape {
            CursorShape::Hidden => 0,
            CursorShape::Block | CursorShape::HollowBlock => 1,
            CursorShape::Beam => 2,
            CursorShape::Underline => 3,
        }
    };
    let cursor = NesttyCursor {
        row: cursor_point.line.0.max(0) as u16,
        col: cursor_point.column.0 as u16,
        style,
        blink: if cs.blinking { 1 } else { 0 },
        _reserved: 0,
    };

    drop(term);

    Box::into_raw(Box::new(NesttySnapshot {
        cols,
        rows: snapshot_rows,
        cursor,
    }))
}

/// Walk a single display line into a `Row`. Groups consecutive cells
/// with identical attributes into runs; appends each cell's char + any
/// zero-width combining marks into the row's utf8 buffer.
fn walk_row(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: Line,
    cols: u16,
) -> Row {
    let mut utf8: Vec<u8> = Vec::new();
    let mut runs: Vec<NesttyRun> = Vec::new();
    let mut col: u16 = 0;

    while col < cols {
        let point = Point::new(line, Column(col as usize));
        let cell = &grid[point];

        // Wide-char trailing cells (the "right half" of a CJK glyph
        // emitted alongside the leading half) carry no glyph and
        // shouldn't generate a run of their own — they're absorbed
        // by the leading half's run.
        if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            col += 1;
            continue;
        }

        let span_cols = if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let utf8_offset = utf8.len() as u32;

        let mut buf = [0u8; 4];
        utf8.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
        // Combining marks live in CellExtra.zerowidth — fold them
        // into the same run's utf8 so CoreText shapes them with
        // their base glyph.
        for combine in cell.zerowidth().unwrap_or(&[]) {
            utf8.extend_from_slice(combine.encode_utf8(&mut buf).as_bytes());
        }

        let utf8_len = utf8.len() as u32 - utf8_offset;

        let mut run_flags = cell_flags_to_ffi(cell.flags);
        if span_cols == 2 {
            run_flags |= flags::WIDE_LEADING;
        }

        let (fg, bg) = (color_to_rgba(cell.fg), color_to_rgba(cell.bg));
        // `cell.underline_color()` can return any AnsiColor variant
        // (Spec from `\e[58;2;…m`, Indexed from `\e[58;5;Nm`, or a
        // named palette color), so route it through the same encoder
        // as fg/bg instead of dropping non-Spec values.
        let underline_color = cell.underline_color().map(color_to_rgba).unwrap_or(0);
        let underline_style = if cell.flags.intersects(CellFlags::ALL_UNDERLINES) {
            // Phase 2 just exposes "1 = some underline"; richer
            // undercurl/dotted decoding lands with the renderer.
            1
        } else {
            0
        };

        runs.push(NesttyRun {
            start_col: col,
            end_col: col + span_cols as u16,
            utf8_offset,
            utf8_len,
            fg_rgba: fg,
            bg_rgba: bg,
            flags: run_flags,
            underline_style,
            reserved: 0,
            underline_color_rgba: underline_color,
            hyperlink_id: cell.hyperlink().map_or(0, |_| 1),
        });

        col += span_cols as u16;
    }

    Row { utf8, runs }
}

fn cell_flags_to_ffi(f: CellFlags) -> u16 {
    let mut out = 0u16;
    if f.contains(CellFlags::BOLD) {
        out |= flags::BOLD;
    }
    if f.contains(CellFlags::ITALIC) {
        out |= flags::ITALIC;
    }
    if f.contains(CellFlags::INVERSE) {
        out |= flags::INVERSE;
    }
    if f.contains(CellFlags::DIM) {
        out |= flags::DIM;
    }
    if f.contains(CellFlags::STRIKEOUT) {
        out |= flags::STRIKE;
    }
    if f.contains(CellFlags::HIDDEN) { /* nothing in our flag set; renderer can decide */ }
    // ALL_UNDERLINES covers single/double/curly/dotted/dashed; we
    // collapse to the UNDERLINE bit for Phase 2 (style enum at
    // `NesttyRun::underline_style` carries the variant).
    if f.intersects(CellFlags::ALL_UNDERLINES) {
        out |= flags::UNDERLINE;
    }
    out
}

/// Encoding scheme for the `fg_rgba` / `bg_rgba` u32 fields. The high
/// byte is a tag that disambiguates three color kinds without growing
/// the ABI:
///
/// - `0x00_00_00_00` — default (renderer materializes to theme fg/bg).
/// - `0x01_00_00_NN` — indexed palette color (N in 0..255). Swift
///   resolves 0-15 from `theme.palette`, 16-231 from the 6×6×6 xterm
///   color cube, 232-255 from the 24-step grayscale ramp.
/// - `0xFF_RR_GG_BB` — direct RGB. Always opaque (alpha forced to 1
///   on decode) because terminal cells don't have a meaningful alpha.
///
/// Tag-based discrimination is required because the older "alpha=0
/// means indexed" trick ambiguated against RGB colors whose R channel
/// is 0 (`\\e[38;2;0;200;255m` and similar), which silently routed
/// them through the indexed path. Other tag values are reserved.
const TAG_INDEXED: u32 = 0x01_00_00_00;
const TAG_DIRECT: u32 = 0xFF_00_00_00;

fn color_to_rgba(color: AnsiColor) -> u32 {
    match color {
        AnsiColor::Named(NamedColor::Foreground) | AnsiColor::Named(NamedColor::Background) => 0,
        AnsiColor::Named(named) => match named_to_indexed(named) {
            Some(idx) => TAG_INDEXED | idx as u32,
            None => 0,
        },
        AnsiColor::Indexed(idx) => TAG_INDEXED | idx as u32,
        AnsiColor::Spec(rgb) => {
            TAG_DIRECT | ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | (rgb.b as u32)
        }
    }
}

/// Map `NamedColor` variants the SGR parser hands us into ANSI
/// palette indices the Swift side already knows how to resolve. Keeps
/// the bright/dim variants honest (bright red is index 9, not 1) so
/// `printf '\033[91mhi'` actually renders bright. Returns `None` for
/// non-palette named colors (DimFg, Cursor, …) so the caller can fall
/// back to the default sentinel.
fn named_to_indexed(named: NamedColor) -> Option<u8> {
    let idx: u8 = match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    };
    Some(idx)
}

/// Free a snapshot.
///
/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `nestty_term_snapshot` and not yet destroyed. Calling twice is UB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_destroy(snap: *mut NesttySnapshot) {
    if snap.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(snap) };
}

/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `nestty_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_rows(snap: *const NesttySnapshot) -> u16 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.rows.len() as u16
}

/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `nestty_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_cols(snap: *const NesttySnapshot) -> u16 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.cols
}

/// Borrowed pointer to the row's run array. Valid until
/// `nestty_snapshot_destroy`. Returns 0 if row is out of range;
/// `*out_runs` set to NULL in that case.
///
/// # Safety
///
/// `out_runs` must point to writable storage for one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_row_runs(
    snap: *const NesttySnapshot,
    row: u16,
    out_runs: *mut *const NesttyRun,
) -> usize {
    if out_runs.is_null() {
        return 0;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_runs = ptr::null() };
        return 0;
    };
    let Some(row_data) = s.rows.get(row as usize) else {
        unsafe { *out_runs = ptr::null() };
        return 0;
    };
    unsafe { *out_runs = row_data.runs.as_ptr() };
    row_data.runs.len()
}

/// Borrowed pointer to the row's utf8 bytes + length. Same lifetime.
///
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_row_utf8(
    snap: *const NesttySnapshot,
    row: u16,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
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

/// Fill `*out` with the snapshot's cursor state.
///
/// # Safety
///
/// `out` must point to writable storage for one `NesttyCursor`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_cursor(
    snap: *const NesttySnapshot,
    out: *mut NesttyCursor,
) {
    if out.is_null() {
        return;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return;
    };
    unsafe { *out = s.cursor };
}

#[unsafe(no_mangle)]
pub extern "C" fn nestty_term_version() -> *const c_char {
    static VERSION: &CStr = c"nestty-term 0.2.0 (Phase 2 — PTY + grid)";
    VERSION.as_ptr()
}

#[cfg(test)]
mod color_encoding_tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb;

    #[test]
    fn default_named_colors_use_sentinel_zero() {
        assert_eq!(color_to_rgba(AnsiColor::Named(NamedColor::Foreground)), 0);
        assert_eq!(color_to_rgba(AnsiColor::Named(NamedColor::Background)), 0);
    }

    #[test]
    fn named_palette_colors_carry_indexed_tag() {
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Red)),
            TAG_INDEXED | 1
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Yellow)),
            TAG_INDEXED | 3
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::BrightRed)),
            TAG_INDEXED | 9
        );
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::BrightWhite)),
            TAG_INDEXED | 15
        );
    }

    #[test]
    fn indexed_256_carries_indexed_tag() {
        assert_eq!(color_to_rgba(AnsiColor::Indexed(0)), TAG_INDEXED);
        assert_eq!(color_to_rgba(AnsiColor::Indexed(245)), TAG_INDEXED | 245);
        assert_eq!(color_to_rgba(AnsiColor::Indexed(255)), TAG_INDEXED | 255);
    }

    /// Regression test for the original bug: RGB colors with R=0 used
    /// to be mis-decoded as indexed (the high byte was 0, which the old
    /// Swift decoder read as "indexed palette"). Now they carry the
    /// 0xFF direct tag so the decoder can disambiguate.
    #[test]
    fn rgb_with_zero_red_does_not_collide_with_indexed() {
        let skyblue = color_to_rgba(AnsiColor::Spec(Rgb {
            r: 0,
            g: 200,
            b: 255,
        }));
        assert_eq!(skyblue >> 24, 0xFF, "direct-color tag must be set");
        assert_eq!((skyblue >> 16) & 0xFF, 0);
        assert_eq!((skyblue >> 8) & 0xFF, 200);
        assert_eq!(skyblue & 0xFF, 255);

        let pure_green = color_to_rgba(AnsiColor::Spec(Rgb { r: 0, g: 255, b: 0 }));
        assert_eq!(pure_green >> 24, 0xFF);
        assert_eq!(pure_green, TAG_DIRECT | (255 << 8));
    }

    #[test]
    fn rgb_round_trip_preserves_channels() {
        let red = color_to_rgba(AnsiColor::Spec(Rgb { r: 255, g: 0, b: 0 }));
        assert_eq!(red, TAG_DIRECT | (255 << 16));

        let black = color_to_rgba(AnsiColor::Spec(Rgb { r: 0, g: 0, b: 0 }));
        // Pure-black RGB stays distinguishable from "default" via the tag.
        assert_eq!(black, TAG_DIRECT);
        assert_ne!(black, 0);
    }

    #[test]
    fn named_unmappable_falls_back_to_default() {
        // DimFg, Cursor, etc. aren't in the 16-color palette; the
        // encoder collapses them to the default sentinel so the
        // renderer picks the theme foreground.
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::DimForeground)),
            0
        );
    }

    /// Underline color goes through the same encoder as fg/bg now, so
    /// `\e[58;5;Nm` (indexed) and `\e[58;2;…m` (direct) both round-trip
    /// through the renderer instead of the indexed branch silently
    /// becoming "use fg".
    #[test]
    fn underline_color_uses_same_encoding_as_fg() {
        assert_eq!(
            color_to_rgba(AnsiColor::Spec(Rgb { r: 10, g: 20, b: 30 })),
            TAG_DIRECT | (10 << 16) | (20 << 8) | 30,
        );
        assert_eq!(color_to_rgba(AnsiColor::Indexed(5)), TAG_INDEXED | 5);
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Red)),
            TAG_INDEXED | 1,
        );
    }
}
