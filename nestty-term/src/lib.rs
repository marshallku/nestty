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
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Side;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::TermMode;
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
    /// 1-based index into the snapshot's `hyperlinks` vec; 0 means no
    /// OSC 8 link on this run. Renderer resolves the URI via
    /// `nestty_snapshot_hyperlink_uri`.
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

/// Active selection bounds reported by `nestty_snapshot_selection`.
/// Both end_row and end_col are INCLUSIVE — alacritty's
/// `SelectionRange` is inclusive on both ends, and the Swift renderer
/// needs to honor that when painting the highlight (otherwise the
/// last selected cell goes unhighlighted, visible on any single-line
/// drag or word selection).
///
/// When `present == 0`, the other fields are meaningless. `is_block`
/// is 1 for `SelectionType::Block` selections (deferred for v1 — only
/// Simple / Semantic / Lines wired today), 0 otherwise.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NesttySelectionRange {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub is_block: u8,
    pub present: u8,
    pub _reserved: u16,
}

/// Opaque heap-allocated UTF-8 byte buffer. Returned by FFI methods
/// that hand the caller a copy of terminal content (selection,
/// scrollback). Free with `nestty_string_destroy` exactly once.
/// Pairing the destroy function with the type avoids the "ptr+len
/// without capacity" UB trap of raw `Vec<u8>` round-tripping.
pub struct NesttyString {
    data: Box<[u8]>,
}

/// Custom `EventListener` for `alacritty_terminal::Term`. Captures
/// the events the renderer actually needs to react to (OSC 52
/// clipboard writes for now; OSC 52 reads, title changes, and bell
/// can land here later). Most events are dropped on purpose —
/// alacritty fires them frequently and our renderer doesn't need
/// most of them.
#[derive(Clone)]
struct NesttyListener {
    /// Most-recent OSC 52 clipboard-store request. Single-slot is
    /// fine: bursts of OSC 52 are rare, and the renderer polls every
    /// vsync. Older pending requests get coalesced — matches the
    /// "last write wins" semantics most emulators have.
    pending_clipboard: Arc<std::sync::Mutex<Option<String>>>,
}

impl NesttyListener {
    fn new() -> Self {
        Self {
            pending_clipboard: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl EventListener for NesttyListener {
    fn send_event(&self, event: Event) {
        if let Event::ClipboardStore(_kind, text) = event {
            // Drop the previous pending request if any (last write
            // wins). The renderer takes it on the next tick via
            // `nestty_term_take_clipboard_request`.
            *self.pending_clipboard.lock().unwrap() = Some(text);
        }
        // Other events (Title, Bell, MouseCursorDirty, …) are
        // intentionally dropped — the renderer doesn't react to them.
    }
}

struct Row {
    utf8: Vec<u8>,
    runs: Vec<NesttyRun>,
}

pub struct NesttyHandle {
    /// Shared between the PTY reader thread (alacritty's EventLoop)
    /// and snapshot callers. Lock duration must stay short on the
    /// snapshot path so the reader thread isn't starved.
    term: Arc<FairMutex<Term<NesttyListener>>>,
    /// Listener clone we keep here so the FFI can poll for pending
    /// OSC 52 / future events without having to lock the term.
    listener: NesttyListener,
    /// Sender into the event loop's mpsc — drives input writes,
    /// resize, and shutdown.
    sender: EventLoopSender,
    /// Reader thread that owns the PTY + parser loop. Joined in
    /// `nestty_term_destroy` after sending `Msg::Shutdown`.
    io_thread: Option<JoinHandle<(EventLoop<Pty, NesttyListener>, State)>>,
    /// Last observed hash of the cursor row's renderable content plus
    /// cursor metadata (style/blink/show). Used by
    /// `nestty_term_take_damage` to catch three classes of changes the
    /// line-bounds filter would otherwise drop: (1) cursor-cell
    /// mutation collapsing to `(line, col, col)` damage — same shape
    /// as alacritty's unconditional `damage_cursor` hint; (2) zero-
    /// width combining marks (alacritty's `Term::input` skips
    /// `damage_point` on that branch); (3) DECSCUSR / `\e[?25l/h`
    /// cursor metadata transitions that produce zero grid damage.
    last_redraw_state_hash: AtomicU64,
}

pub struct NesttySnapshot {
    cols: u16,
    rows: Vec<Row>,
    cursor: NesttyCursor,
    selection: NesttySelectionRange,
    /// OSC 8 hyperlink URIs visible in this snapshot. The per-run
    /// `hyperlink_id` is 1-based index into this vec (0 = no link).
    /// Deduped by alacritty's `Hyperlink::id` so a hyperlink spanning
    /// many cells only stores its URI once.
    hyperlinks: Vec<String>,
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
    let listener = NesttyListener::new();
    let term = Term::new(Config::default(), &term_size, listener.clone());
    let term = Arc::new(FairMutex::new(term));

    let event_loop = match EventLoop::new(Arc::clone(&term), listener.clone(), pty, false, false) {
        Ok(el) => el,
        Err(_) => return ptr::null_mut(),
    };
    let sender = event_loop.channel();
    let io_thread = event_loop.spawn();

    Box::into_raw(Box::new(NesttyHandle {
        term,
        listener,
        sender,
        io_thread: Some(io_thread),
        last_redraw_state_hash: AtomicU64::new(0),
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

/// Query whether the terminal grid has been damaged since the last
/// call. Returns `true` if any cell changed; `false` if the grid is
/// byte-for-byte what the renderer already drew. Always resets the
/// internal damage state so the next call only sees what changed
/// AFTER this one.
///
/// The renderer's intended loop:
///   CADisplayLink tick → `nestty_term_take_damage` → if false, skip;
///   if true, `nestty_term_snapshot` + redraw.
///
/// Cursor-only filter: `alacritty_terminal::Term::damage` unconditionally
/// marks the current cursor cell on every call (a hint for renderers
/// that paint a blinking cursor). That makes the raw signal "always
/// damaged," which defeats the gate. We instead treat damage as "real"
/// only when it covers ANY cell other than exactly the cursor's
/// single-cell point — cursor *movement* still counts because the
/// damage region then includes the previous cursor cell, widening
/// beyond left==right==cursor.col.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_take_damage(handle: *mut NesttyHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    let mut term = h.term.lock();
    let cursor_point = term.grid().cursor.point;
    let cursor_line = cursor_point.line.0.max(0) as usize;
    let cursor_col = cursor_point.column.0;

    // Hash everything `nestty_term_snapshot` will expose for the
    // current cursor row + the cursor + active selection bounds.
    // Catches (1) cursor-cell mutations whose damage bounds collapse
    // to `(line, col, col)` (same shape as the unconditional
    // `damage_cursor` hint the line-bounds filter discards),
    // (2) combining marks pushed onto a non-cursor cell (alacritty's
    // `Term::input` zero-width branch updates `cell.zerowidth`
    // without calling `damage_point`), (3) DECSCUSR / `\e[?25l/h`
    // transitions that change cursor metadata without grid damage,
    // and (4) selection start/extend/clear that doesn't touch any
    // cell content.
    let state_hash = hash_redraw_state(&term, cursor_point);
    let prev_hash = h.last_redraw_state_hash.swap(state_hash, Ordering::Relaxed);
    let state_changed = state_hash != prev_hash;

    let real_damage = match term.damage() {
        alacritty_terminal::term::TermDamage::Full => true,
        alacritty_terminal::term::TermDamage::Partial(mut iter) => {
            state_changed
                || iter.any(|d| {
                    !(d.line == cursor_line && d.left == cursor_col && d.right == cursor_col)
                })
        }
    };
    term.reset_damage();
    real_damage
}

/// Hash every renderable field the snapshot path will expose: cursor
/// row contents, cursor metadata (pos/style/blink/visibility), and
/// active selection bounds. Used by `nestty_term_take_damage` to
/// catch grid-invisible state transitions (DECSCUSR, selection
/// start/extend/clear) and the cursor-row content cases the line-
/// bounds filter would otherwise collapse with the unconditional
/// `damage_cursor` hint.
///
/// Caller already holds the term lock — no extra synchronization
/// needed. Out-of-range cursor returns a fixed sentinel so the
/// comparison is still stable.
fn hash_redraw_state(term: &Term<NesttyListener>, cursor: Point) -> u64 {
    let line = cursor.line;
    if line.0 < 0 || (line.0 as usize) >= term.screen_lines() {
        return 0;
    }
    let cols = term.columns();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    {
        let grid = term.grid();
        for c in 0..cols {
            let cell = &grid[Point::new(line, Column(c))];
            cell.c.hash(&mut hasher);
            cell.flags.bits().hash(&mut hasher);
            // AnsiColor isn't Hash, so round-trip through our tagged-u32
            // encoding (which already collapses every variant to a
            // stable value).
            color_to_rgba(cell.fg).hash(&mut hasher);
            color_to_rgba(cell.bg).hash(&mut hasher);
            if let Some(extras) = cell.zerowidth() {
                for ch in extras {
                    ch.hash(&mut hasher);
                }
            }
            if let Some(uc) = cell.underline_color() {
                color_to_rgba(uc).hash(&mut hasher);
            }
            // Hyperlink: hash (id, uri) — id alone isn't enough
            // because OSC 8's explicit `id=` parameter survives
            // unchanged across distinct URIs, so a same-id-new-uri
            // transition would slip past the gate.
            if let Some(h) = cell.hyperlink() {
                h.id().hash(&mut hasher);
                h.uri().hash(&mut hasher);
            } else {
                0u8.hash(&mut hasher);
            }
        }
    }
    // Cursor metadata. Movement is technically caught by the line-bounds
    // filter (prev+current cursor damage widens the line bounds), but
    // style/blink/visibility changes leave no grid damage at all — they
    // only matter once they appear in the next snapshot.
    cursor.line.0.hash(&mut hasher);
    cursor.column.0.hash(&mut hasher);
    let cs = term.cursor_style();
    (cs.shape as u8).hash(&mut hasher);
    cs.blinking.hash(&mut hasher);
    term.mode()
        .contains(TermMode::SHOW_CURSOR)
        .hash(&mut hasher);
    // Selection bounds — start/extend/clear don't necessarily damage
    // any cell content, but the highlight overlay needs to redraw.
    let sel = selection_range_for_ffi(term);
    sel.present.hash(&mut hasher);
    if sel.present == 1 {
        sel.start_row.hash(&mut hasher);
        sel.start_col.hash(&mut hasher);
        sel.end_row.hash(&mut hasher);
        sel.end_col.hash(&mut hasher);
        sel.is_block.hash(&mut hasher);
    }
    // Scrollback offset: scrolling into history doesn't damage the
    // live grid (alacritty's damage tracking only fires on writes to
    // the live region) but changes what the viewport DISPLAYS, so
    // every row visible to the user is different. Including the
    // offset here forces a redraw on scroll without needing alacritty
    // to surface a "display_offset changed" event.
    term.grid().display_offset().hash(&mut hasher);
    hasher.finish()
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

    // Hyperlink dedup map: `(id, uri)` → 1-based index into
    // `hyperlinks`. Keying by id alone is unsafe because explicit
    // OSC 8 ids ARE preserved by alacritty (only missing ids get
    // auto-generated unique values), so two distinct URIs can share
    // an id. Pairing them in the key keeps each URI its own slot.
    let mut hyperlinks: Vec<String> = Vec::new();
    let mut hyperlink_index_by_key: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();

    // Viewport mapping: when the user has scrolled into history,
    // `display_offset > 0` and viewport row 0 maps to live line
    // `-display_offset`. alacritty's `Grid: Index<Line>` walks into
    // scrollback for negative line values directly, so the snapshot
    // ends up describing whatever the user is currently looking at.
    let display_offset = grid.display_offset() as i32;

    let mut snapshot_rows = Vec::with_capacity(rows_count as usize);
    for line_idx in 0..rows_count as i32 {
        let line = Line(line_idx - display_offset);
        let row = walk_row(
            grid,
            line,
            cols,
            &mut hyperlinks,
            &mut hyperlink_index_by_key,
        );
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
    // Cursor row needs the same display_offset mapping as the snapshot
    // rows. When the user has scrolled into history, the cursor may
    // sit outside the visible viewport — clamp the displayed style to
    // `0` (hidden) for those cases so we don't draw a stray block on
    // top of scrollback content. Position is still emitted (renderer
    // can clip however it likes); `style = 0` is the canonical
    // "don't draw" signal.
    let cursor_viewport_row = cursor_point.line.0 + display_offset;
    let cursor_visible = cursor_viewport_row >= 0 && (cursor_viewport_row as u16) < rows_count;
    let cursor = NesttyCursor {
        row: cursor_viewport_row.max(0) as u16,
        col: cursor_point.column.0 as u16,
        style: if cursor_visible { style } else { 0 },
        blink: if cs.blinking { 1 } else { 0 },
        _reserved: 0,
    };

    let selection = selection_range_for_ffi(&term);

    drop(term);

    Box::into_raw(Box::new(NesttySnapshot {
        cols,
        rows: snapshot_rows,
        cursor,
        selection,
        hyperlinks,
    }))
}

/// Project `term.selection` (if any) into the FFI-friendly inclusive-
/// bounds struct the renderer paints from. Returns the default
/// (present=0) when there's no selection or it doesn't resolve to a
/// range (e.g. empty drag, viewport scrolled past the selection).
fn selection_range_for_ffi(term: &Term<NesttyListener>) -> NesttySelectionRange {
    let Some(sel) = term.selection.as_ref() else {
        return NesttySelectionRange::default();
    };
    let Some(range): Option<SelectionRange> = sel.to_range(term) else {
        return NesttySelectionRange::default();
    };
    // Map absolute line coordinates → viewport rows by adding
    // display_offset (same mapping as the snapshot row walk + cursor).
    let display_offset = term.grid().display_offset() as i32;
    let last_row = term.screen_lines().saturating_sub(1) as i32;
    let last_col = term.columns().saturating_sub(1) as u16;
    let start_view = range.start.line.0 + display_offset;
    let end_view = range.end.line.0 + display_offset;

    // Intersection with the visible viewport. If the entire selection
    // sits above or below the visible rows, hide the overlay (the
    // selection still exists logically — Cmd+C will still grab it,
    // because `selection_to_string` operates on the absolute range —
    // we just don't paint a misleading row-0 sliver).
    if end_view < 0 || start_view > last_row {
        return NesttySelectionRange::default();
    }

    // Clip the off-viewport endpoint columns: when the selection
    // extends BEFORE row 0, the visible start logically begins at
    // column 0 of row 0 (the "before viewport" portion is invisible).
    // Likewise when it extends past `last_row`, the visible end is the
    // last column of `last_row`. Without this, a multi-line selection
    // scrolled partway out would paint with the off-screen endpoint's
    // column index, producing wrong clip widths on the boundary row.
    let (start_row, start_col) = if start_view < 0 {
        (0u16, 0u16)
    } else {
        (start_view as u16, range.start.column.0 as u16)
    };
    let (end_row, end_col) = if end_view > last_row {
        (last_row as u16, last_col)
    } else {
        (end_view as u16, range.end.column.0 as u16)
    };

    NesttySelectionRange {
        start_row,
        start_col,
        end_row,
        end_col,
        is_block: u8::from(range.is_block),
        present: 1,
        _reserved: 0,
    }
}

/// Walk a single display line into a `Row`. Groups consecutive cells
/// with identical attributes AND identical single-byte ASCII char into
/// one run so the renderer makes one CTLine per span instead of per
/// cell — the dominant cost on idle/scrollback frames where most cells
/// are spaces. The aggregation is intentionally conservative:
/// uniform-ASCII only (so the cursor-cell glyph re-render picks any
/// byte and gets the right char), no wide chars, no combining marks.
fn walk_row(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: Line,
    cols: u16,
    hyperlinks: &mut Vec<String>,
    hyperlink_index_by_key: &mut std::collections::HashMap<(String, String), u32>,
) -> Row {
    let mut utf8: Vec<u8> = Vec::new();
    let mut runs: Vec<NesttyRun> = Vec::new();
    // Side-channel: for each pushed run, the ASCII byte every cell in
    // that run shares — or None if the run is non-uniform (multi-byte
    // char, combining marks, wide char, or mixed contents). Only
    // Some-valued entries can be extended by `try_extend_last_run`.
    let mut run_uniform: Vec<Option<u8>> = Vec::new();
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
        let hyperlink_id = cell
            .hyperlink()
            .map(|h| {
                let key = (h.id().to_owned(), h.uri().to_owned());
                if let Some(idx) = hyperlink_index_by_key.get(&key) {
                    return *idx;
                }
                hyperlinks.push(key.1.clone());
                let new_idx = hyperlinks.len() as u32; // 1-based
                hyperlink_index_by_key.insert(key, new_idx);
                new_idx
            })
            .unwrap_or(0);

        // Aggregation eligibility: single-cell, ASCII char, no
        // combining marks. Multi-byte chars, wide chars, and cells
        // with combining marks each get their own run so cursor-cell
        // glyph extraction stays a simple "pick the run's bytes".
        let has_zw = cell.zerowidth().is_some_and(|z| !z.is_empty());
        let cell_byte: Option<u8> = if span_cols == 1 && !has_zw && cell.c.is_ascii() {
            Some(cell.c as u8)
        } else {
            None
        };

        if let Some(b) = cell_byte
            && let (Some(last), Some(last_uniform)) = (runs.last_mut(), run_uniform.last_mut())
            && *last_uniform == Some(b)
            && last.fg_rgba == fg
            && last.bg_rgba == bg
            && last.flags == run_flags
            && last.underline_color_rgba == underline_color
            && last.underline_style == underline_style
            && last.hyperlink_id == hyperlink_id
        {
            // Extend the previous run by one column. utf8 stays
            // uniform because we appended the same byte.
            utf8.push(b);
            last.utf8_len += 1;
            last.end_col += 1;
            col += 1;
            continue;
        }

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
            hyperlink_id,
        });
        run_uniform.push(cell_byte);

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

/// Fill `*out` with the snapshot's active selection bounds. Renderer
/// checks `out.present` to decide whether to paint a highlight.
///
/// # Safety
///
/// `out` must point to writable storage for one `NesttySelectionRange`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_selection(
    snap: *const NesttySnapshot,
    out: *mut NesttySelectionRange,
) {
    if out.is_null() {
        return;
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return;
    };
    unsafe { *out = s.selection };
}

// ---------- Selection control ----------

/// Selection kind discriminator for `nestty_term_selection_start`.
/// Kept as named constants so Swift / future C consumers can mirror
/// the contract; the SIMPLE variant is consumed via the match's
/// fallback arm.
#[allow(dead_code)]
const SELECTION_SIMPLE: u8 = 0;
const SELECTION_SEMANTIC: u8 = 1;
const SELECTION_LINES: u8 = 2;

/// Side discriminator: 0 = Left of cell, 1 = Right. Mirrors
/// alacritty's `Side` enum so the renderer can compute it from the
/// pixel offset within the cell (left half → Left, right half → Right)
/// without bringing the enum across FFI.
const SIDE_LEFT: u8 = 0;

fn parse_side(side: u8) -> Side {
    if side == SIDE_LEFT {
        Side::Left
    } else {
        Side::Right
    }
}

fn selection_point(term: &Term<NesttyListener>, row: u16, col: u16) -> Point {
    // Renderer passes viewport-relative row coordinates (row 0 = top
    // of what's currently visible, regardless of scrollback). Convert
    // to alacritty's absolute Line by subtracting display_offset —
    // when scrolled back, viewport row 0 sits at Line(-display_offset).
    // Without this, drag/click on scrolled-back content selects the
    // live grid at the same row instead of what the user sees.
    let display_offset = term.grid().display_offset() as i32;
    let line = Line(row as i32 - display_offset);
    let cols = term.columns();
    let column = Column((col as usize).min(cols.saturating_sub(1)));
    Point::new(line, column)
}

/// Start a new selection at (`row`, `col`). Replaces any existing
/// selection. `kind` is `SELECTION_SIMPLE` / `SEMANTIC` / `LINES`;
/// anything else falls back to simple.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_selection_start(
    handle: *mut NesttyHandle,
    row: u16,
    col: u16,
    side: u8,
    kind: u8,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    let point = selection_point(&term, row, col);
    let ty = match kind {
        SELECTION_SEMANTIC => SelectionType::Semantic,
        SELECTION_LINES => SelectionType::Lines,
        _ => SelectionType::Simple,
    };
    term.selection = Some(Selection::new(ty, point, parse_side(side)));
}

/// Extend the current selection to `(row, col, side)`. No-op if there
/// isn't a selection in progress.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_selection_update(
    handle: *mut NesttyHandle,
    row: u16,
    col: u16,
    side: u8,
) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    let point = selection_point(&term, row, col);
    if let Some(sel) = term.selection.as_mut() {
        sel.update(point, parse_side(side));
    }
}

/// Clear the active selection.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_selection_clear(handle: *mut NesttyHandle) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    h.term.lock().selection = None;
}

/// Select the entire visible viewport (Cmd+A). Uses a Simple
/// selection from (0, 0) to (last_line, last_col).
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_selection_all(handle: *mut NesttyHandle) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    let mut term = h.term.lock();
    // Select the currently-visible viewport — which, when the user is
    // scrolled into history, means scrollback lines. Top of viewport is
    // `Line(-display_offset)`; bottom is that plus `screen_lines - 1`.
    // (Selecting ALL of scrollback regardless of scroll position is a
    // different feature; matching iTerm2 / Terminal.app's Cmd+A: only
    // what the user can see.)
    let display_offset = term.grid().display_offset() as i32;
    let screen_lines = term.screen_lines() as i32;
    let top = Line(-display_offset);
    let bottom = Line(screen_lines - 1 - display_offset);
    let last_col = Column(term.columns().saturating_sub(1));
    let start = Point::new(top, Column(0));
    let end = Point::new(bottom, last_col);
    let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
    sel.update(end, Side::Right);
    term.selection = Some(sel);
}

/// Heap-allocated UTF-8 buffer of the current selection. Returns NULL
/// when nothing is selected. Caller must free with
/// `nestty_string_destroy` exactly once.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_selection_string(
    handle: *mut NesttyHandle,
) -> *mut NesttyString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    let term = h.term.lock();
    let Some(s) = term.selection_to_string() else {
        return ptr::null_mut();
    };
    if s.is_empty() {
        return ptr::null_mut();
    }
    Box::into_raw(Box::new(NesttyString {
        data: s.into_bytes().into_boxed_slice(),
    }))
}

/// Borrowed pointer to the string's bytes (NOT NUL-terminated).
/// `*out_len` receives the byte length. Both are valid until
/// `nestty_string_destroy`.
///
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_string_bytes(
    s: *const NesttyString,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { s.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    unsafe { *out_len = s.data.len() };
    s.data.as_ptr()
}

/// Free a `NesttyString`. NULL-safe.
///
/// # Safety
///
/// Must be called exactly once per pointer returned by an FFI method
/// that hands out `*mut NesttyString`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_string_destroy(s: *mut NesttyString) {
    if s.is_null() {
        return;
    }
    let _ = unsafe { Box::from_raw(s) };
}

/// True if any of alacritty's mouse-reporting modes is active. Used
/// by the renderer to defer to TUI mouse handlers (vim, less, htop,
/// tmux) instead of consuming the drag for selection — the renderer
/// only takes mouse events when this returns false OR the user holds
/// Shift to explicitly override.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_mouse_mode_active(handle: *mut NesttyHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    let term = h.term.lock();
    use alacritty_terminal::term::TermMode as M;
    term.mode()
        .intersects(M::MOUSE_REPORT_CLICK | M::MOUSE_DRAG | M::MOUSE_MOTION)
}

/// True if the terminal has bracketed paste mode enabled (`\e[?2004h`).
/// Renderer wraps Cmd+V'd text in `\e[200~ … \e[201~` when this is
/// true so paste-aware programs (zsh, neovim with `set paste`, etc.)
/// can distinguish pasted bytes from typed bytes.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_bracketed_paste_active(handle: *mut NesttyHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.term
        .lock()
        .mode()
        .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
}

/// Scroll-direction discriminator for `nestty_term_scroll`. Mirrors
/// `alacritty_terminal::grid::Scroll` so the renderer doesn't have to
/// bring the enum across the FFI.
#[allow(dead_code)]
const SCROLL_DELTA: u8 = 0;
const SCROLL_PAGE_UP: u8 = 1;
const SCROLL_PAGE_DOWN: u8 = 2;
const SCROLL_TOP: u8 = 3;
const SCROLL_BOTTOM: u8 = 4;

/// Scroll the visible viewport. `kind` is one of `SCROLL_*`; `delta`
/// is only used for `SCROLL_DELTA` (lines; positive = older content
/// scrolls into view, negative = newer). Page / Top / Bottom ignore
/// `delta`.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_scroll(handle: *mut NesttyHandle, kind: u8, delta: i32) {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return;
    };
    use alacritty_terminal::grid::Scroll;
    let scroll = match kind {
        SCROLL_PAGE_UP => Scroll::PageUp,
        SCROLL_PAGE_DOWN => Scroll::PageDown,
        SCROLL_TOP => Scroll::Top,
        SCROLL_BOTTOM => Scroll::Bottom,
        _ => Scroll::Delta(delta),
    };
    h.term.lock().scroll_display(scroll);
}

/// Take the most-recent pending OSC 52 clipboard-store request (the
/// `\e]52;c;<base64>\a` sequence programs use to push text into the
/// system clipboard). Returns NULL if nothing is pending. Caller
/// frees the returned string with `nestty_string_destroy` and gates
/// the actual NSPasteboard write on the user's `[security] osc52`
/// policy. Single-slot semantics: bursts coalesce to "last write
/// wins" — matches how VTE and iTerm2 handle the same case.
///
/// # Safety
///
/// `handle` must be NULL or a valid pointer returned by
/// `nestty_term_create` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_term_take_clipboard_request(
    handle: *mut NesttyHandle,
) -> *mut NesttyString {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return ptr::null_mut();
    };
    let Some(text) = h.listener.pending_clipboard.lock().unwrap().take() else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(NesttyString {
        data: text.into_bytes().into_boxed_slice(),
    }))
}

/// Number of distinct OSC 8 hyperlink URIs visible in this snapshot.
/// IDs handed back to the renderer in `NesttyRun.hyperlink_id` are
/// 1-based indices in `[1, count]`; 0 means "no hyperlink".
///
/// # Safety
///
/// `snap` must be NULL or a valid pointer returned by
/// `nestty_term_snapshot` and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_hyperlink_count(snap: *const NesttySnapshot) -> u32 {
    let Some(s) = (unsafe { snap.as_ref() }) else {
        return 0;
    };
    s.hyperlinks.len() as u32
}

/// Borrowed pointer to the URI bytes for the given 1-based hyperlink
/// id. Returns NULL + sets `*out_len = 0` when the id is out of
/// range. Lifetime matches the snapshot — copy out before calling
/// `nestty_snapshot_destroy`.
///
/// # Safety
///
/// `out_len` must point to writable storage for one `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nestty_snapshot_hyperlink_uri(
    snap: *const NesttySnapshot,
    hyperlink_id: u32,
    out_len: *mut usize,
) -> *const u8 {
    if out_len.is_null() {
        return ptr::null();
    }
    let Some(s) = (unsafe { snap.as_ref() }) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    if hyperlink_id == 0 {
        unsafe { *out_len = 0 };
        return ptr::null();
    }
    let idx = (hyperlink_id as usize).saturating_sub(1);
    let Some(uri) = s.hyperlinks.get(idx) else {
        unsafe { *out_len = 0 };
        return ptr::null();
    };
    unsafe { *out_len = uri.len() };
    uri.as_ptr()
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
            color_to_rgba(AnsiColor::Spec(Rgb {
                r: 10,
                g: 20,
                b: 30
            })),
            TAG_DIRECT | (10 << 16) | (20 << 8) | 30,
        );
        assert_eq!(color_to_rgba(AnsiColor::Indexed(5)), TAG_INDEXED | 5);
        assert_eq!(
            color_to_rgba(AnsiColor::Named(NamedColor::Red)),
            TAG_INDEXED | 1,
        );
    }
}
