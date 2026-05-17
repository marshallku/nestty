//! Phase 2 acceptance test: spawn a real shell, type a printf, wait
//! for output to land in the grid, snapshot, assert the row contains
//! the expected bytes. Resize round-trip too.
//!
//! Runs `/bin/sh` (POSIX-mandated path) with a controlled command via
//! the `-c` shell mechanism. Avoids depending on the user's `$SHELL`
//! so the test is reproducible across machines.
//!
//! Marked `#[ignore]` by default because it actually spawns a child
//! process and blocks on PTY I/O — too costly for `cargo test` happy
//! path. Run explicitly with `cargo test -p nestty-term -- --ignored`.

use std::ffi::CString;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nestty_term::*;

/// Drive the PTY until `predicate` returns true OR the deadline
/// expires. Snapshots in a tight-ish loop (10ms) so the test stays
/// fast when output arrives quickly but doesn't busy-spin.
fn wait_for<F: Fn(&str) -> bool>(handle: *mut NesttyHandle, predicate: F, deadline_ms: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    loop {
        let snap = nestty_term_snapshot(handle);
        if snap.is_null() {
            return None;
        }
        let row0 = unsafe { row_text(snap, 0) };
        unsafe { nestty_snapshot_destroy(snap) };
        if predicate(&row0) {
            return Some(row0);
        }
        if Instant::now() >= deadline {
            return Some(row0);
        }
        sleep(Duration::from_millis(10));
    }
}

unsafe fn row_text(snap: *mut NesttySnapshot, row: u16) -> String {
    let mut len: usize = 0;
    let bytes_ptr = unsafe { nestty_snapshot_row_utf8(snap, row, &mut len) };
    if bytes_ptr.is_null() || len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
#[ignore]
fn printf_round_trip() {
    let shell = CString::new("/bin/sh").unwrap();
    let handle = unsafe { nestty_term_create(80, 24, shell.as_ptr(), std::ptr::null()) };
    assert!(!handle.is_null(), "shell spawn failed");

    let cmd = b"printf 'nestty-phase2-marker'\n";
    unsafe { nestty_term_input(handle, cmd.as_ptr(), cmd.len()) };

    let result = wait_for(handle, |row| row.contains("nestty-phase2-marker"), 2_000);
    unsafe { nestty_term_destroy(handle) };

    let row = result.expect("snapshot returned NULL");
    assert!(
        row.contains("nestty-phase2-marker"),
        "expected marker in row 0; got: {row:?}"
    );
}

#[test]
#[ignore]
fn resize_round_trip() {
    let shell = CString::new("/bin/sh").unwrap();
    let handle = unsafe { nestty_term_create(80, 24, shell.as_ptr(), std::ptr::null()) };
    assert!(!handle.is_null());

    // Initial geometry visible in snapshot.
    let snap = nestty_term_snapshot(handle);
    assert_eq!(nestty_snapshot_cols(snap), 80);
    assert_eq!(nestty_snapshot_rows(snap), 24);
    unsafe { nestty_snapshot_destroy(snap) };

    nestty_term_resize(handle, 100, 40);

    // The reader thread processes the Resize message asynchronously;
    // poll briefly for the snapshot to reflect the new dims.
    let deadline = Instant::now() + Duration::from_millis(1_000);
    let (cols, rows) = loop {
        let snap = nestty_term_snapshot(handle);
        let cols = nestty_snapshot_cols(snap);
        let rows = nestty_snapshot_rows(snap);
        unsafe { nestty_snapshot_destroy(snap) };
        if (cols, rows) == (100, 40) || Instant::now() >= deadline {
            break (cols, rows);
        }
        sleep(Duration::from_millis(10));
    };

    unsafe { nestty_term_destroy(handle) };
    assert_eq!((cols, rows), (100, 40), "resize never reflected in snapshot");
}

#[test]
fn null_destroy_no_op() {
    unsafe {
        nestty_term_destroy(std::ptr::null_mut());
        nestty_snapshot_destroy(std::ptr::null_mut());
    }
}
