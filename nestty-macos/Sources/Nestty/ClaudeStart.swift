import AppKit
import Foundation

/// PR 8 — `claude.start` socket action, macOS port. 1:1 functional mirror of
/// `nestty-linux/src/socket.rs::handle_claude_start` plus its `_seeder` helper.
/// Spawns a tab whose terminal cwd is `workspace_path` and feeds
/// `tmux new-session -A -s <name> 'claude [--resume <id>]'` so the user
/// lands in either a freshly-created or already-attached tmux session
/// running claude. Optional `prompt` gets pasted into the REPL via
/// tmux's load-buffer + paste-buffer dance once the pane crosses two
/// readiness checks.
///
/// Why two readiness checks (matches Linux's design):
/// `tmux new-session -A` ATTACHES if the named session already exists,
/// ignoring our trailing `claude` command — a pre-existing shell session
/// that happens to share the name would receive the prompt as raw shell
/// input and execute it. Both gates below have to pass before paste:
///
/// 1. `tmux capture-pane` returns text containing claude-specific markers
///    (`Anthropic`, `Try "`, `claude --`, or "claude code"). Generic
///    `> ` / box-drawing markers are NOT enough — shells emit those too.
/// 2. `tmux display-message #{pane_current_command}` returns `claude` or
///    `node` (the binary claude-code is built on). The kernel's view —
///    survives capture-pane scrollback wraparound.
///
/// On either failure: stderr log, no paste. The `claude.start` reply has
/// already returned success at this point; the prompt is post-action
/// best-effort and any retry is the user's call.
enum ClaudeStart {
    // MARK: - Public entry — dispatched from AppDelegate.handleCommand

    /// Run synchronously on the main actor. Validates params, derives
    /// session name, builds the tmux command, asks `TabViewController`
    /// for a new terminal tab with `cwd` + `initialInput`, then kicks
    /// off the prompt seeder if a prompt was provided. Reply payload
    /// matches Linux byte-for-byte: `{panel_id, tab, tmux_session,
    /// workspace_path}`.
    @MainActor
    static func dispatch(
        params: [String: Any],
        tabVC: TabViewController?,
        completion: @escaping (Any?) -> Void,
    ) {
        guard let tabVC else {
            completion(RPCError(code: "internal_error", message: "claude.start: TabViewController missing"))
            return
        }

        // workspace_path: required, non-empty string, must canonicalize
        // to an existing directory.
        guard let raw = params["workspace_path"] as? String, !raw.isEmpty else {
            if params["workspace_path"] != nil {
                completion(RPCError(code: "invalid_params", message: "'workspace_path' must be a non-empty string"))
            } else {
                completion(RPCError(code: "invalid_params", message: "Missing 'workspace_path' param"))
            }
            return
        }
        let canonURL: URL
        do {
            // resolvingSymlinksInPath + standardizedFileURL approximates
            // realpath(3); we then check `.isDirectory` separately so
            // symlinks-to-files surface as `invalid_params` like Linux.
            let resolved = URL(fileURLWithPath: raw).resolvingSymlinksInPath().standardizedFileURL
            var isDir: ObjCBool = false
            guard FileManager.default.fileExists(atPath: resolved.path, isDirectory: &isDir) else {
                completion(RPCError(code: "not_found", message: "workspace_path \(raw): no such file or directory"))
                return
            }
            if !isDir.boolValue {
                completion(RPCError(code: "invalid_params", message: "workspace_path \(resolved.path) is not a directory"))
                return
            }
            canonURL = resolved
        }

        // session_name: explicit (validated) or derived from canonURL's
        // last 1-2 path components.
        let sessionName: String
        switch params["session_name"] {
        case let s as String where !s.isEmpty:
            if let err = validateTmuxSessionName(s) {
                completion(RPCError(code: "invalid_params", message: "session_name: \(err)"))
                return
            }
            sessionName = s
        case nil, is NSNull:
            sessionName = deriveSessionName(from: canonURL)
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'session_name' must be a string, got \(type(of: other))",
            ))
            return
        }

        // resume_session: optional non-empty string with no control chars.
        let resumeSession: String?
        switch params["resume_session"] {
        case let s as String where !s.isEmpty:
            for scalar in s.unicodeScalars {
                if scalar.value < 0x20 || scalar.value == 0x7F {
                    completion(RPCError(
                        code: "invalid_params",
                        message: "resume_session contains control characters",
                    ))
                    return
                }
            }
            resumeSession = s
        case nil, is NSNull, is String:
            // Empty string falls in here too — treat as absent like Linux.
            resumeSession = nil
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'resume_session' must be a string, got \(type(of: other))",
            ))
            return
        }

        // prompt: optional non-empty string. Mutually exclusive with
        // resume_session — `--resume` restores existing context, seeding
        // new text on top would just confuse claude.
        let promptToSeed: String?
        switch params["prompt"] {
        case let s as String where !s.isEmpty:
            promptToSeed = s
        case nil, is NSNull, is String:
            promptToSeed = nil
        case let other:
            completion(RPCError(
                code: "invalid_params",
                message: "'prompt' must be a string, got \(type(of: other))",
            ))
            return
        }
        if promptToSeed != nil, resumeSession != nil {
            completion(RPCError(
                code: "invalid_params",
                message: "'prompt' and 'resume_session' are mutually exclusive — "
                    + "resume restores existing context; prompt seeds a new conversation",
            ))
            return
        }

        // Build the tmux command we feed into the new terminal.
        let claudeCmd = if let id = resumeSession {
            "claude --resume \(shellSingleQuote(id))"
        } else {
            "claude"
        }
        let tmuxCommand = "tmux new-session -A -s \(shellSingleQuote(sessionName)) "
            + "\(shellSingleQuote(claudeCmd))\n"

        // Spawn the tab. Returns (panelID, tabIndex) atomically so we
        // can reply with both before the seeder background work fires.
        let (panelID, tabIndex) = tabVC.newTerminalTab(
            cwd: canonURL.path,
            initialInput: tmuxCommand,
        )

        // Seeder runs on a background thread so claude.start returns
        // immediately — caller isn't blocked on capture-pane polling.
        if let prompt = promptToSeed {
            spawnPromptSeeder(sessionName: sessionName, prompt: prompt)
        }

        completion([
            "panel_id": panelID,
            "tab": tabIndex,
            "tmux_session": sessionName,
            "workspace_path": canonURL.path,
        ])
    }

    // MARK: - Helpers (1:1 ports of Linux versions)

    /// POSIX-safe single-quote escape: wrap in `'…'`, replace embedded
    /// `'` with `'\''`. Result is a single shell token. Mirrors Linux's
    /// `shell_single_quote` byte-for-byte.
    static func shellSingleQuote(_ s: String) -> String {
        var out = "'"
        out.reserveCapacity(s.count + 2)
        for c in s {
            if c == "'" {
                out += "'\\''"
            } else {
                out.append(c)
            }
        }
        out.append("'")
        return out
    }

    /// Last 1-2 path components, joined with `-`, lowercased + sanitized.
    /// Two components rather than one because layouts like
    /// `<worktree_root>/feature/foo` would otherwise collapse to `foo`,
    /// colliding with sibling worktrees on the same leaf name.
    static func deriveSessionName(from url: URL) -> String {
        // `pathComponents` includes "/" as the first element on absolute
        // paths plus a trailing "/" placeholder for directories — filter
        // both out, then take the last 2.
        let parts = url.pathComponents.filter { $0 != "/" && !$0.isEmpty }
        let tail = parts.suffix(2)
        let joined = tail.joined(separator: "-")
        return sanitizeSessionName(joined)
    }

    /// ASCII alphanumeric + `-_` only, lowercased. Anything else becomes
    /// `-`. Trim leading/trailing `-`. Empty result falls back to
    /// `"claude"`. Same shape as Linux `sanitize_session_name`.
    static func sanitizeSessionName(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.count)
        for scalar in s.unicodeScalars {
            let ascii = scalar.isASCII ? Character(scalar) : "-"
            if ascii.isASCII, ascii.isLetter || ascii.isNumber || ascii == "-" || ascii == "_" {
                out.append(Character(ascii.lowercased()))
            } else {
                out.append("-")
            }
        }
        let trimmed = out.trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        return trimmed.isEmpty ? "claude" : trimmed
    }

    /// Returns nil on valid name, error message otherwise. Mirrors Linux's
    /// `validate_tmux_session_name`.
    static func validateTmuxSessionName(_ s: String) -> String? {
        if s.isEmpty { return "cannot be empty" }
        if s.hasPrefix("-") { return "cannot start with '-' (would look like a flag)" }
        for c in s {
            let isASCIIAlphanum = c.isASCII && (c.isLetter || c.isNumber)
            if !isASCIIAlphanum, c != "-", c != "_" {
                return "invalid character '\(c)' (allowed: ASCII alphanumeric and - _)"
            }
        }
        return nil
    }

    // MARK: - Prompt seeder (background thread)

    /// Two-stage readiness check + tmux load-buffer/paste-buffer/double-Enter
    /// to deliver `prompt` to claude's REPL once the pane has crossed both
    /// gates. Failures log to stderr but never propagate — `claude.start`
    /// has already returned success. See file-level docs for the gate
    /// rationale.
    static func spawnPromptSeeder(sessionName: String, prompt: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            // Initial settle so capture-pane has SOMETHING to inspect.
            Thread.sleep(forTimeInterval: 0.4)

            let deadline = Date().addingTimeInterval(8.0)
            var sawClaudeMarker = false
            while Date() < deadline {
                if let captured = runTmux(["capture-pane", "-p", "-t", sessionName]) {
                    let lower = captured.lowercased()
                    if captured.contains("Anthropic")
                        || captured.contains("Try \"")
                        || captured.contains("claude --")
                        || lower.contains("claude code")
                    {
                        sawClaudeMarker = true
                        break
                    }
                }
                Thread.sleep(forTimeInterval: 0.2)
            }

            // Cross-check: kernel-side foreground command in the pane.
            // Survives even if claude's banner has scrolled off.
            let rawCmd = runTmux([
                "display-message", "-p", "-t", sessionName, "#{pane_current_command}",
            ]) ?? ""
            let currentCmd = rawCmd
                .trimmingCharacters(in: .whitespacesAndNewlines)
                .lowercased()
            let paneIsClaude = currentCmd == "claude" || currentCmd == "node"

            guard sawClaudeMarker, paneIsClaude else {
                let msg = "[claude.start] refusing to paste prompt into session "
                    + "\(sessionName.debugDescription): saw_claude_marker=\(sawClaudeMarker), "
                    + "pane_current_command=\(currentCmd.debugDescription). "
                    + "Pre-existing tmux session may be a shell or a non-claude process; "
                    + "user can paste the prompt manually.\n"
                FileHandle.standardError.write(Data(msg.utf8))
                return
            }

            // Write prompt to a temp file → load-buffer → paste-buffer.
            // Going through a buffer is what makes multi-line + special-char
            // payloads safe; send-keys -l would also work but each special
            // character needs care.
            let tmpURL = FileManager.default.temporaryDirectory.appendingPathComponent(
                "nestty-claude-\(UUID().uuidString).txt",
            )
            do {
                try prompt.data(using: .utf8)?.write(to: tmpURL, options: .atomic)
            } catch {
                FileHandle.standardError.write(Data(
                    "[claude.start] tempfile failed: \(error)\n".utf8,
                ))
                return
            }
            defer { try? FileManager.default.removeItem(at: tmpURL) }

            let bufName = "nestty-claude-\(UUID().uuidString)"
            guard runTmuxStatus(["load-buffer", "-b", bufName, tmpURL.path]) else {
                FileHandle.standardError.write(Data(
                    "[claude.start] tmux load-buffer failed\n".utf8,
                ))
                return
            }
            // `-p` activates bracketed-paste mode — wraps the paste in
            // ESC [ 200 ~ … ESC [ 201 ~ so claude's terminal sees the
            // entire buffer as one paste rather than treating each
            // embedded `\n` as a separate Enter. Without `-p`, a
            // multi-line prompt arrives at claude as N separate user
            // turns (each Line gets its own ⏺ echo). Linux's path
            // works either because of differing tmux defaults or
            // because the Linux desktop terminal happens to forward
            // bracketed-paste through VTE+claude — adding `-p`
            // explicitly here matches the documented intent without
            // depending on environment quirks.
            //
            // `-d` drops the buffer after pasting (cleanup, not
            // semantically required).
            guard runTmuxStatus(["paste-buffer", "-t", sessionName, "-b", bufName, "-p", "-d"]) else {
                FileHandle.standardError.write(Data(
                    "[claude.start] tmux paste-buffer failed\n".utf8,
                ))
                return
            }

            // Submit. claude's REPL needs TWO Enters for long pastes:
            // bracketed-paste-collapse mode renders the input as
            // `[Pasted text #1 +N lines]` once a paste exceeds claude's
            // inline threshold, so the first Enter commits/expands the
            // paste placeholder and the second sends it to the model.
            // Short pastes that don't collapse get submitted by the
            // first Enter; the second hits an already-empty input and
            // claude no-ops on it. Two-Enter covers both cases without
            // inspecting claude's UI state.
            _ = runTmuxStatus(["send-keys", "-t", sessionName, "Enter"])
            Thread.sleep(forTimeInterval: 0.2)
            _ = runTmuxStatus(["send-keys", "-t", sessionName, "Enter"])
        }
    }

    /// Run `tmux <args>`, return stdout as String. nil on launch error or
    /// non-zero exit. PATH lookup goes through `/usr/bin/env` so a
    /// homebrew-installed tmux at `/opt/homebrew/bin/tmux` works without
    /// hard-coding the path.
    private static func runTmux(_ args: [String]) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["tmux"] + args
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = Pipe() // discard
        do {
            try process.run()
        } catch {
            return nil
        }
        process.waitUntilExit()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        guard process.terminationStatus == 0 else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Same as `runTmux` but only reports success/failure — used for
    /// fire-and-forget commands like load-buffer / paste-buffer / send-keys.
    private static func runTmuxStatus(_ args: [String]) -> Bool {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["tmux"] + args
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        do {
            try process.run()
        } catch {
            return false
        }
        process.waitUntilExit()
        return process.terminationStatus == 0
    }
}
