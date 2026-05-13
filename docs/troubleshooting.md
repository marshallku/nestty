# Troubleshooting

## Build Issues

### Missing vte4 system library

```
error: could not find system library 'vte-2.91-gtk4'
```

**Fix:** `sudo pacman -S vte4`

### Missing gtk4 system library

**Fix:** `sudo pacman -S gtk4`

### `load_from_string` not found on CssProvider

The method is gated behind a feature flag.
**Fix:** Add `features = ["gnome_46"]` to gtk4 dependency in Cargo.toml.

### Cargo binary name collision

```
warning: output filename collision at target/debug/nestty
```

nestty-linux and nestty-cli both output `nestty`.
**Fix:** CLI binary renamed to `nestctl` in nestty-cli/Cargo.toml.

## Runtime Issues

### Wayland protocol error (Error 71)

```
Gdk-Message: Error 71 (Protocol error)
```

**Fix:** Set `GDK_BACKEND=x11` in environment or in main.rs.

### GBM buffer error

```
Failed to create GBM buffer of size 841x1352: Invalid argument
```

**Fix:** Set `WEBKIT_DISABLE_DMABUF_RENDERER=1` (only relevant if using WebKit components).

### Terminal shows in light mode

**Cause:** Transparent VTE background with no image loaded shows the system theme underneath.
**Fix:**

1. Force dark theme: `settings.set_gtk_application_prefer_dark_theme(true)` in `app.rs`
2. Window CSS `window { background-color: <theme.background> }` provides the solid fallback color now that VTE is permanently transparent (no more conditional opaque bg).

### Background images not showing (solid color only)

Multiple possible causes:

1. **Config `directory` is commented out**: Check `~/.config/nestty/config.toml`. The `directory` field must be uncommented. A `#` before the key comments it out.

2. **Surface is opaque**: the window-level `BackgroundLayer` paints behind everything, so any opaque widget above it hides the image. Required transparent surfaces: VTE (`set_clear_background(false)` + `RGBA(0,0,0,0)`), WebKit (`webview.set_background_color(RGBA(0,0,0,0))`), notebook header / statusbar / `html, body` in plugin CSS — all transparent. If you add a new chrome widget and the image disappears under it, that widget needs the same treatment.

3. **Image loading fails silently**: The original `GtkPicture::set_file()` loads asynchronously and fails silently. Fixed by using `gdk::Texture::from_file()` for synchronous loading with error reporting.

4. **Tint too opaque**: Tint at 0.9 makes images nearly invisible (90% opaque dark overlay). Lower to 0.85 or less.

5. **GTK single-instance**: If an old nestty is running, new launches activate the old instance and exit immediately (exit code 0, no output). Kill all instances first: `killall nestty`.

### App exits immediately with no error

**Cause:** GTK single-instance behavior. Another nestty instance already owns the GTK app ID `com.marshall.nestty`.
**Fix:** `killall nestty` then relaunch.

### log:: messages not visible

**Cause (resolved Step 5a):** `nestty-linux` used to skip `env_logger::init()`, so `log::info!` / `log::warn!` were silent. We now initialize `env_logger` with `default_filter_or("warn")` in `main()`. Set `RUST_LOG=info` to surface gui_client register/reconnect, or `RUST_LOG=debug` for the full reconnect cadence. GTK does not capture stderr on console launches, so the diagnostics appear when running `nestty` from a terminal. Desktop-entry launches may still hide stderr depending on the session — use `journalctl --user -f` if needed.

### Terminal shows only one line (collapsed height)

**Cause:** `GtkOverlay` sizes based on its child widget. The window-level root overlay's child is the (hideable) `bg_picture` from `BackgroundLayer`, so when no image is set the base child has zero natural size and the overlay collapses unless an overlay is marked as size-driver.
**Fix:** Call `root_overlay.set_measure_overlay(&layout, true)` so the actual UI layout drives the overlay's measurement regardless of bg image state. The `TerminalPanel`'s own (search-bar) overlay is already measured by its terminal child.

### WebKit web process crashes on many sites

```
GStreamer element autoaudiosink not found. Please install it
GLib-GObject-CRITICAL: invalid (NULL) pointer instance
WebProcess CRASHED
```

**Cause:** Missing GStreamer plugins. WebKitGTK requires GStreamer for media handling, and crashes when the plugins are absent — even on pages that don't play media.
**Fix:** `sudo pacman -S gst-plugins-good gst-plugins-bad`

### D-Bus: `register_object` API mismatch

**Cause:** gio 0.20 uses builder pattern, not positional args.
**Fix:** Use `connection.register_object(path, &interface_info).method_call(closure).build()`.

### Plugin/webview panel frozen on last frame after Hyprland workspace switch — known upstream limitation

**Symptom:** Plugin panel (or any `webkit6::WebView`) renders fine on first show. User switches to a different Hyprland workspace, then comes back. Panel is stuck on the last frame — appears alive (backend healthy, WebProcess alive, IPC responsive) but doesn't repaint. Right-click → "Inspect Element" revives instantly. Focusing another window and coming back also revives it.

**Status: known upstream limitation in WebKitGTK 6.0 ↔ Hyprland interaction. Not fixable in nestty-side code.**

**Reproduction outside nestty:** Spawn the official WebKitGTK reference browser:
```
/usr/lib/webkitgtk-6.0/MiniBrowser https://www.google.com
```
on Hyprland and switch workspaces. Same freeze. This is zero nestty code, so the bug is upstream.

**What was ruled out empirically (rounds 1–5, all reverted):**
- Round 1 — `webview.connect_map(|wv| wv.evaluate_javascript("0"))`: signal never fires; Hyprland uses scene-graph hide without `wl_surface.unmap`.
- Round 2 — toplevel `is-active` notify + `evaluate_javascript("0")`: hook fires correctly per stderr capture; nudge insufficient.
- Round 3 — `is-active` + `GdkToplevel:state` notify with `queue_draw()` on both: hooks fire, queue_draw runs (verified via stderr); panel still freezes.
- Round 4 — same hooks + `set_visible(false); set_visible(true)` on `GDK_TOPLEVEL_STATE_SUSPENDED` rising-edge clear: hook fires, toggle runs; freeze persists.
- Round 5 — same hooks + full `webview.reload()` on suspended-clear: freeze persists, AND reload destroys panel state on every workspace return (bad UX, net negative).
- Environment variables that did NOT help: `WEBKIT_DISABLE_DMABUF_RENDERER=1`, `GSK_RENDERER=cairo`, `WEBKIT_DISABLE_COMPOSITING_MODE=1`, `__EGL_VENDOR_LIBRARY_FILENAMES=…/50_mesa.json` (forcing Mesa EGL on NVIDIA).
- Hardware: reproduces on NVIDIA RTX 3060 Ti (driver 595.71.05) AND on a separate integrated-graphics laptop. Not GPU-vendor-specific.
- Compositor versions: Hyprland 0.54.3 (no longer wlroots-based) + WebKitGTK 2.52.3.

**Why no application-level fix worked:** The freeze is in WebKit's compositor frame-production path after the wl_surface gets the SUSPENDED bit and then has it cleared. The bit DOES toggle on Hyprland (verified via `connect_state_notify` logs), but WebKit's render scheduler doesn't resume pushing frames on bit-clear unless an actual input event (pointer, dev-tools attach via JS pump from inspector init) drives it. There is no public WebKitGTK 6.0 API to tell the WebProcess "visibility changed, resume rendering."

**User-facing workaround:** Click anywhere in the panel after coming back from a workspace, OR focus another window then refocus nestty, OR right-click → Inspect Element. All three paths cause WebKit's compositor to resume.

**Automated cure on Hyprland — `window.restored` + `system.spawn` trigger (Phase WR-1/WR-2):**

If you're on Hyprland specifically, two separate `hyprctl dispatch resizewindowpixel` calls (a 1px nudge) empirically unfreeze the panel — the underlying mechanism by which this works where `--batch` doesn't is not fully characterized; the behavior reproduces reliably across cycles. On the dual-monitor setup we tested, only same-monitor workspace cycles trigger the freeze; cross-monitor switches did not. nestty exposes the building blocks:

- `window.restored` event fires when the toplevel's `GDK_TOPLEVEL_STATE_SUSPENDED` bit clears — i.e. you're returning to the workspace nestty lives on.
- `system.spawn` is a trigger-only action (NOT reachable from `nestctl call`, by design) that exec's an argv vector fire-and-forget.

Drop this into `~/.config/nestty/config.toml`:

```toml
[[triggers]]
name = "hyprland-webkit-cure"
action = "system.spawn"
params = { argv = ["sh", "-c", "hyprctl dispatch resizewindowpixel '1 0,class:com.marshall.nestty' && hyprctl dispatch -- resizewindowpixel '-1 0,class:com.marshall.nestty'"] }

[triggers.when]
event_kind = "window.restored"
```

**Two empirical decisions baked into that snippet** (observed behaviors on Hyprland 0.54.3 — the underlying mechanism for the second one is not fully characterized, but the behavior reproduced reliably across dozens of cycles):

- `resizewindowpixel` with `class:com.marshall.nestty` selector, NOT `resizeactive`. The trigger fires on workspace return regardless of which window has focus on that workspace — the user often returns with focus on whatever they were last using on that workspace, not nestty. `resizeactive` would resize that other window and the freeze stays put. `resizewindowpixel,class:` is focus-agnostic.
- Two separate `hyprctl dispatch` calls chained with `&&`, NOT `hyprctl --batch "...; ..."`. `--batch` consistently fails to cure; two separate IPC calls consistently do.

The second `hyprctl` invocation uses `hyprctl dispatch -- resizewindowpixel '-1 0,class:com.marshall.nestty'` because `-1 0,...` begins with `-` and hyprctl's CLI parser would otherwise treat `-1` as a flag — the `--` is what forces end-of-options.

**Why `sh -c` here is safe — and when it would NOT be:** `system.spawn` doesn't auto-wrap argv in a shell, so by default `{event.*}` and `{context.*}` interpolations land as literal argv elements where shell metacharacters can't be re-parsed. That default safety is what protects the bare-argv form. Once the user EXPLICITLY chooses `["sh", "-c", "<string>"]`, every interpolated value spliced into that string IS shell-evaluated, so the bare-argv guarantee no longer applies — every interpolation source must be audited individually. The snippet above is safe only because it satisfies BOTH (a) the trigger doesn't interpolate any `{event.X}` or `{context.X}` value into the shell string (every argv element is a literal) AND (b) `window.restored` itself emits an empty `{}` payload, so even a typo'd `{event.X}` would resolve to a literal token rather than attacker-controlled data. Do NOT copy this `sh -c` pattern to triggers that interpolate ANY field (event payload OR context fields like `{context.active_cwd}`) into the shell string — a trigger on e.g. `slack.mention` carrying a user-controlled `text` field, or even one referencing a directory path the user happens to have, would let a Slack message or a malicious dir name run arbitrary code. Use the bare argv form (`argv = ["program", "arg1", ...]`) whenever the trigger interpolates anything.

A ready-to-copy snippet lives at [`examples/triggers/hyprland-webkit-fix.toml`](../examples/triggers/hyprland-webkit-fix.toml).

This is a workaround that papers over the upstream bug — if you're not on Hyprland, the trigger no-ops (other compositors don't toggle SUSPENDED on workspace switch the same way), and there's no nestty-side state to roll back when WebKit/Hyprland publish a real fix.

**Possible future paths (not pursued):**
- File upstream issue at `bugs.webkit.org` and `github.com/hyprwm/Hyprland` with the MiniBrowser reproducer.
- Wait for an upstream fix in WebKitGTK or Hyprland.
- Replace the panel rendering layer (move away from WebKit) — large scope.

**Distinct from cold-boot blank panel** (different mechanism — see commit `bb9c1f1` prewarm).

The diagnostic signal hooks (`load_changed` / `load_failed` / `web_process_terminated`) added in commit `78ebdb1` remain in `plugin_panel.rs` because they are general-purpose, not specific to this freeze.

---

## macOS App Issues

### SwiftTerm: `processTerminated` never called after shell exits

**Cause:** SwiftTerm's `LocalProcess.childProcessRead` detects PTY EOF and calls `childStopped()`, which cancels the internal `childMonitor` DispatchSource before it can fire. The `processTerminated` call in the EOF handler is commented out in SwiftTerm source.

**Fix:** Install a separate `DispatchSource.makeProcessSource` after `startProcess()` returns (in `NesttyTerminalView.installExitMonitor()`). This source is not affected by `childStopped()` and fires independently when the process exits.

```swift
func installExitMonitor() {
    let pid = process.shellPid
    guard pid > 0 else { return }
    let src = DispatchSource.makeProcessSource(identifier: pid, eventMask: .exit, queue: .main)
    src.setEventHandler { [weak self, weak src] in
        src?.cancel()
        guard let self else { return }
        processDelegate?.processTerminated(source: self, exitCode: nil)
    }
    exitMonitor = src
    src.activate()
}
```

### macOS split panes: new pane gets wrong initial size

**Cause 1 (`layout()` approach):** NSSplitView calls `resizeSubviews` (which sets subview frames) before calling `layout()`. By the time `layout()` fires, the wrong frames are already committed. Calling `setPosition` in `layout()` fires too late — if the terminal view already has a large frame from before the rebuild, NSSplitView uses that as the basis for proportional sizing.

**Cause 2 (`asyncAfter` approach):** The 50ms delay is unreliable — layout may not have resolved yet, or a subsequent split may have started before the timer fires, applying stale positions.

**Fix:** Use `NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:)`. This delegate method is called by NSSplitView at the exact moment it needs to determine subview frames. Set frames directly here and set `initialSizeSet = true` after the first call to fall back to `adjustSubviews()` for subsequent resizes (preserving user drag behaviour).

### macOS: `becomeFirstResponder` cannot be overridden in SwiftTerm subclass

**Cause:** `MacTerminalView.becomeFirstResponder` is declared `public` but not `open`, so it cannot be overridden by code outside the SwiftTerm module.

**Fix:** Use `NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown)` in `PaneManager` to detect which pane was clicked and update `activePane` accordingly.

### macOS: `terminal.output` event not implementable

**Cause:** SwiftTerm's `feed(byteArray:)` is declared in an extension of `TerminalView` (not `open`), so it cannot be overridden by subclasses outside the module. There is no other public hook for intercepting raw PTY output bytes.

**Status:** Not implemented. Shell integration signals (`terminal.shell_precmd` / `terminal.shell_preexec`) are sent via socket commands from the shell script directly instead of OSC 133 parsing.

### macOS: OSC 52 clipboard write was unconditional (security regression)

**Cause:** SwiftTerm's `LocalProcessTerminalView.clipboardCopy(source:content:)` is declared `public` (not `open`) and unconditionally writes the OSC 52 payload to `NSPasteboard.general`. Because the method is `public`, subclasses outside the SwiftTerm module cannot override it. Pre-fix, any program in a pane could silently overwrite the user's clipboard.

**Fix:** `NesttyTerminalView` installs a custom `NesttyTerminalDelegate` proxy into SwiftTerm's `terminalDelegate` slot. The proxy forwards `sizeChanged` / `setTerminalTitle` / `hostCurrentDirectoryUpdate` / `send` / `scrolled` / `rangeChanged` to the host's public methods (so PTY winsize, title updates, OSC 7, key input, etc. continue to work) and applies an `OSC52Policy` gate on `clipboardCopy`. `requestOpenLink` / `bell` / `iTermContent` are left to the protocol-extension defaults — overriding them would change behavior with no benefit.

The policy is read from `[security] osc52` in config (`"deny"` default, `"allow"` opts back into legacy behavior). Hot-reload propagates through `applyConfig` → `paneManager.applyOSC52Policy` so live panes pick up the change without restart.

VTE on Linux already disables OSC 52 by default, so this fix is macOS-only.

### macOS: Nerd Font icons show as boxes or render broken

**Cause 1 — Font not found by family name:** `NSFont(name:size:)` only accepts PostScript names and full names (e.g. `JetBrains Mono Regular`), not bare family names like `JetBrainsMono Nerd Font Mono`. When the lookup fails, the terminal falls back to the system monospace font which has no Nerd Font PUA glyphs.

**Fix:** Font resolution now uses a multi-step strategy: PostScript name → `NSFontManager` exact family lookup → case-insensitive family lookup → `NSFontDescriptor` → system fallback. Both PostScript names and family names now work reliably.

**Cause 2 — Using non-Mono Nerd Font variant:** Standard Nerd Font variants (e.g. `JetBrainsMono Nerd Font`) render icons as 2-column wide glyphs. SwiftTerm's Unicode width table does not include PUA codepoints (U+E000–U+F8FF), so it treats them as 1-column, causing icons to overflow into the adjacent cell.

**Fix:** Use the **Mono** variant of your Nerd Font (e.g. `JetBrainsMono Nerd Font Mono`). Mono variants explicitly set all icons to 1-column width.

```toml
[terminal]
font_family = "JetBrainsMono Nerd Font Mono"
```

### macOS: Background `opacity` config change not reflected at runtime

**Cause:** `Config.swift` only parsed `path` and `tint` from the `[background]` section. The `opacity` field was silently ignored, and the `applyBackground` signature only accepted `path` and `tint`. Hot-reload therefore never changed the image layer's alpha.

**Fix:** Added `backgroundOpacity: Double` to `NesttyConfig`, parse `("background", "opacity")` in `Config.parse`, and propagated an `opacity` parameter through the full call chain: `NesttyPanel.applyBackground(path:tint:opacity:)` → `TerminalViewController` (sets `backgroundView?.alphaValue`) → `WebViewController` (no-op) → `PaneManager` → `TabViewController` (stores `currentBackgroundOpacity`) → `AppDelegate` initial apply and `background.set` socket command.

Also added `("background", "image")` as an alias for `("background", "path")` to match the documented config key.

### macOS: OSC 7 CWD URI includes hostname

**Cause:** OSC 7 delivers a `file://hostname/path` URI (e.g. `file://Marshalls-MacBook-Pro.local/Users/marshallku`). Simply stripping `file://` leaves the hostname in the path.

**Fix:** Use `URL(string: directory).path` to correctly extract only the POSIX path component, discarding the scheme and hostname.

### macOS: Web tab opens with no URL bar — only "Open a URL to get started"

**Cause:** `WebViewController.loadView` set `view = wv` (the bare `WKWebView`), so the only way to navigate was the `webview.navigate` socket command. Linux's `WebViewPanel` ships a Catppuccin-themed toolbar (back / forward / reload / URL entry / devtools) above the webview; macOS lacked the entire toolbar.

**Fix:** Wrap the `WKWebView` in an `NSView` container with an `NSStackView` toolbar above it. Toolbar buttons use SF Symbols (`chevron.left`, `chevron.right`, `arrow.clockwise`, `wrench.and.screwdriver`) and call existing `goBack` / `goForward` / `reload` / `toggleDevTools`. URL `NSTextField` fires its action on Enter and routes through `navigate(to:)`, which already handles the `https://` prefixing.

Back/forward enabled state and URL field text sync via KVO on `WKWebView.canGoBack` / `canGoForward` / `url` — `WKWebView` is KVO-compliant for these. The URL sync skips updates while the field's editor is the first responder so it doesn't clobber what the user is typing. On a blank tab (no `startURL`), `viewDidAppear` focuses the URL field so the user can type immediately.
