import AppKit
import Foundation

/// Tier 1.2 — custom keybindings parsed from `[keybindings]` in `config.toml`.
///
/// TOML shape (matches Linux verbatim):
///
/// ```toml
/// [keybindings]
/// "cmd+shift+g" = "spawn:~/scripts/grep-something.sh"
/// "ctrl+shift+m" = "spawn:~/scripts/toggle-mute.sh"
/// "cmd+e" = "action:webview.open url=https://example.com"
/// ```
///
/// Two value syntaxes:
///
/// - **`spawn:<cmd>`** — runs `<cmd>` via `sh -c` in a detached background
///   `Process` with `NESTTY_SOCKET` injected so the script can call back via
///   `nestctl`. Mirrors Linux's `spawn_command`.
/// - **`action:<method> [k=v ...]`** — dispatches `<method>` through the
///   `ActionRegistry` with `params` parsed from the `k=v` tail. Lets the
///   user wire keybindings into the same surface plugins + triggers
///   already use (`webview.open`, `git.list_workspaces`, etc.). Plain
///   strings only — no nested params; serializes to JSON via `[String: Any]`.
///
/// Modifier syntax supports both Linux convention (`ctrl`) and macOS-native
/// (`cmd`), plus `shift`/`alt`/`option`. Order doesn't matter (`cmd+shift+g`
/// == `shift+cmd+g`). Key is the unshifted character (e.g. `g`, not `G`).
///
/// Resolution order at keyDown time: built-in shortcuts (Cmd+T, Cmd+W, etc.)
/// fire FIRST via the menu bar, then this monitor sees the key event. So a
/// user binding `cmd+t` to a custom action gets shadowed by the built-in
/// "New Tab" — that's intentional (don't let users break the standard menu).
/// To override, change the menu's keyEquivalent or use a different combo.
enum Keybindings {
    /// Compiled binding ready to match against `NSEvent`.
    struct Binding {
        let modifiers: NSEvent.ModifierFlags
        let key: String // lowercased character, e.g. "g"
        let command: String // raw value from config: "spawn:..." or "action:..."
    }

    /// Parse the raw TOML dict into compiled bindings. Invalid combos
    /// (unknown modifier, empty key) are dropped with a stderr warning so
    /// one typo doesn't disable the whole map.
    static func compile(_ raw: [String: String]) -> [Binding] {
        var out: [Binding] = []
        for (combo, command) in raw {
            guard let binding = parseCombo(combo, command: command) else {
                continue
            }
            out.append(binding)
        }
        return out
    }

    private static func parseCombo(_ combo: String, command: String) -> Binding? {
        let parts = combo.split(separator: "+").map { p in
            p.trimmingCharacters(in: .whitespaces).lowercased()
        }
        guard !parts.isEmpty else { return nil }

        var mods: NSEvent.ModifierFlags = []
        var key: String?
        for part in parts {
            switch part {
            case "cmd", "command", "meta": mods.insert(.command)
            case "ctrl", "control": mods.insert(.control)
            case "shift": mods.insert(.shift)
            case "alt", "option": mods.insert(.option)
            case "":
                continue
            default:
                if key != nil {
                    let msg = "[nestty] keybinding '\(combo)' has multiple non-modifier keys (\(key!) and \(part)) — skipping\n"
                    FileHandle.standardError.write(Data(msg.utf8))
                    return nil
                }
                key = part
            }
        }
        guard let key, !key.isEmpty else {
            let msg = "[nestty] keybinding '\(combo)' has no key — skipping\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
        return Binding(modifiers: mods, key: key, command: command)
    }

    /// Compare an NSEvent against a binding. Modifiers must match exactly
    /// (so `cmd+g` doesn't fire on `cmd+shift+g` — that'd be surprising).
    /// Key compared against `charactersIgnoringModifiers` lowercased so
    /// `shift+g` config matches the underlying `g` key with shift held.
    static func matches(_ event: NSEvent, _ binding: Binding) -> Bool {
        // Mask out caps lock / numpad noise — only the four real modifier
        // flags are part of the binding contract.
        let interesting: NSEvent.ModifierFlags = [.command, .control, .shift, .option]
        let actualMods = event.modifierFlags.intersection(interesting)
        guard actualMods == binding.modifiers else { return false }
        let keyChar = (event.charactersIgnoringModifiers ?? "").lowercased()
        return keyChar == binding.key
    }

    /// Dispatch a binding's command. Called from the NSEvent local monitor
    /// after a match; runs on the main thread so action dispatch can hit
    /// the @MainActor `ActionRegistry` without hopping.
    @MainActor
    static func dispatch(_ binding: Binding, registry: ActionRegistry, socketPath: String) {
        let cmd = binding.command
        if let payload = cmd.stripPrefixIfMatches("spawn:") {
            spawn(payload, socketPath: socketPath)
        } else if let payload = cmd.stripPrefixIfMatches("action:") {
            invokeAction(payload, registry: registry)
        } else {
            let msg = "[nestty] keybinding command '\(cmd)' has no spawn:/action: prefix — skipping\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// `spawn:` handler — mirrors Linux's `spawn_command`. Tilde-expand,
    /// run through `sh -c` so users can use shell features (pipes,
    /// redirects) without quoting headaches, detach (no stdin/out),
    /// inject `NESTTY_SOCKET` so the spawned process can call back via
    /// `nestctl --socket $NESTTY_SOCKET ...`.
    private static func spawn(_ rawCmd: String, socketPath: String) {
        let cmd = rawCmd.replacingOccurrences(of: "~", with: NSHomeDirectory(), options: .anchored)
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", cmd]
        var env = ProcessInfo.processInfo.environment
        env["NESTTY_SOCKET"] = socketPath
        process.environment = env
        // Detach from our stdio so the child doesn't pipe to our terminal.
        // FileHandle(forUpdatingAtPath: "/dev/null") would be the fully
        // correct equivalent of Linux's Stdio::null, but Process accepts nil
        // and treats it as "inherit", which means the child inherits our
        // stderr — cheap visibility into spawn failures during dev.
        do {
            try process.run()
        } catch {
            let msg = "[nestty] keybinding spawn failed for '\(rawCmd)': \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// `action:` handler — parses `<method> [k=v ...]` and dispatches via
    /// the registry. Plain string values only; the syntax is intentionally
    /// minimal because keybindings are a quick-launch surface, not a
    /// general-purpose RPC client. For complex calls users should
    /// `spawn:nestctl call <method> --params '<json>'` instead.
    @MainActor
    private static func invokeAction(_ tail: String, registry: ActionRegistry) {
        let trimmed = tail.trimmingCharacters(in: .whitespaces)
        let parts = trimmed.split(separator: " ", omittingEmptySubsequences: true)
        guard let methodSub = parts.first else {
            FileHandle.standardError.write(Data("[nestty] keybinding action: missing method\n".utf8))
            return
        }
        let method = String(methodSub)
        var params: [String: Any] = [:]
        for kv in parts.dropFirst() {
            let pair = kv.split(separator: "=", maxSplits: 1)
            guard pair.count == 2 else { continue }
            params[String(pair[0])] = String(pair[1])
        }
        let dispatched = registry.tryDispatch(method, params: params) { result in
            // Fire-and-forget. Surface errors in stderr so a misconfigured
            // binding gets debugged easily.
            if let err = result as? RPCError {
                let msg = "[nestty] keybinding action \(method) failed: \(err.code) — \(err.message)\n"
                FileHandle.standardError.write(Data(msg.utf8))
            }
        }
        if !dispatched {
            let msg = "[nestty] keybinding action \(method) not registered\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }
}

private extension String {
    /// Like `removingPrefix` but returns nil when the prefix doesn't match —
    /// lets `spawn:`/`action:` dispatch use a single guard ladder.
    func stripPrefixIfMatches(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }
}
