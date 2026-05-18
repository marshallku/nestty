//! `nestctl recent` — recent bus events snapshot.
//!
//! Wraps the `event.history` socket action (Phase 19.X event ring
//! buffer in `nestty-core::event_bus`). Two filters:
//! - `--since 2h` (or `30m`, `1d`, or raw seconds) → `since_ms` cutoff
//! - `--kind jira.*` → event-kind glob
//!
//! Default human renderer: `<HH:MM:SS>  <kind>  <one-line payload>`.
//! `--json` dumps the raw action payload (matches the wire shape used
//! by `event.subscribe`).

use clap::Args;
use serde_json::{Value, json};

use crate::plugin_cmds::call_and_render;

#[derive(Args, Debug)]
pub struct RecentArgs {
    /// Lookback duration (`2h`, `30m`, `1d`, or raw seconds)
    #[arg(long, default_value = "1h")]
    pub since: String,
    /// Filter by event-kind glob (e.g. `jira.*`, `slack.dm`)
    #[arg(long)]
    pub kind: Option<String>,
}

pub fn dispatch(args: &RecentArgs, socket_path: &str, json_out: bool) -> i32 {
    let lookback_secs = match parse_duration_seconds(&args.since) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: invalid --since `{}`: {e}", args.since);
            return 1;
        }
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let since_ms = now_ms.saturating_sub(lookback_secs.saturating_mul(1000));

    let mut params = json!({ "since_ms": since_ms });
    if let Some(k) = &args.kind {
        params["kind"] = json!(k);
    }

    call_and_render(
        socket_path,
        "event.history",
        params,
        json_out,
        render_events,
    )
}

fn render_events(v: &Value) {
    let events = v
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if events.is_empty() {
        println!("(no events in window)");
        return;
    }
    let kind_w = events
        .iter()
        .map(|e| e.get("type").and_then(Value::as_str).unwrap_or("").len())
        .max()
        .unwrap_or(0)
        .max(4);
    for e in &events {
        let ts_ms = e.get("timestamp_ms").and_then(Value::as_u64).unwrap_or(0);
        let kind = e.get("type").and_then(Value::as_str).unwrap_or("?");
        let payload = e.get("data").map(payload_preview).unwrap_or_default();
        println!("{ts}  {kind:<kind_w$}  {payload}", ts = format_clock(ts_ms));
    }
}

fn format_clock(ts_ms: u64) -> String {
    // Just the HH:MM:SS of the local time. Lighter than pulling a date
    // formatter; consumer of full timestamp uses --json.
    let secs = (ts_ms / 1000) as i64;
    // Local-tz offset via libc-free approach: chrono is already a CLI
    // dep, reuse it.
    let dt = chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
    );
    dt.format("%H:%M:%S").to_string()
}

/// One-line summary of the payload: each top-level field as `key=val`,
/// truncated to fit a typical terminal width. Strings are quoted; full
/// fidelity available via `--json`.
fn payload_preview(payload: &Value) -> String {
    const MAX_LEN: usize = 120;
    let Some(obj) = payload.as_object() else {
        return short_value(payload, MAX_LEN);
    };
    if obj.is_empty() {
        return "{}".to_string();
    }
    let parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| format!("{k}={}", short_value(v, 40)))
        .collect();
    let s = parts.join(" ");
    // Truncate by char count, not byte count — `String::truncate` is
    // a byte index and panics on a non-char-boundary cut, which
    // happens whenever the 120th byte lands inside a multibyte
    // UTF-8 sequence (e.g. Korean / Japanese / emoji payloads).
    if s.chars().count() > MAX_LEN {
        let truncated: String = s.chars().take(MAX_LEN).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

fn short_value(v: &Value, max: usize) -> String {
    // `serde_json::to_string` JSON-escapes every variant uniformly,
    // including control bytes inside strings (`` rather than
    // a literal ESC). Without this, an event payload that captured
    // raw ANSI/OSC bytes (e.g. an OSC 11 `set background color`)
    // would print verbatim and reconfigure the host terminal — a
    // user-visible bug that turned the entire terminal background
    // black during `nestctl recent`.
    let raw = serde_json::to_string(v).unwrap_or_default();
    if raw.chars().count() > max {
        let truncated: String = raw.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        raw
    }
}

/// Accept `2h`, `30m`, `45s`, `1d`, or a bare integer (seconds).
/// Returns total seconds.
pub fn parse_duration_seconds(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty".into());
    }
    let (num_part, unit) = match s.chars().last().unwrap() {
        'h' | 'H' => (&s[..s.len() - 1], 3600u64),
        'm' | 'M' => (&s[..s.len() - 1], 60),
        's' | 'S' => (&s[..s.len() - 1], 1),
        'd' | 'D' => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("not a number: {num_part:?}"))?;
    n.checked_mul(unit).ok_or_else(|| "overflow".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_accepts_known_units() {
        assert_eq!(parse_duration_seconds("2h").unwrap(), 7200);
        assert_eq!(parse_duration_seconds("30m").unwrap(), 1800);
        assert_eq!(parse_duration_seconds("45s").unwrap(), 45);
        assert_eq!(parse_duration_seconds("1d").unwrap(), 86_400);
        assert_eq!(parse_duration_seconds("120").unwrap(), 120);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration_seconds("").is_err());
        assert!(parse_duration_seconds("twoh").is_err());
        assert!(parse_duration_seconds("h").is_err());
    }

    #[test]
    fn payload_preview_renders_flat_object() {
        let v = json!({"channel": "C123", "ts": "1700.500"});
        let s = payload_preview(&v);
        assert!(s.contains("channel=\"C123\""));
        assert!(s.contains("ts=\"1700.500\""));
    }

    #[test]
    fn payload_preview_truncates_long_strings() {
        let long = "x".repeat(500);
        let v = json!({"body": long});
        let s = payload_preview(&v);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn payload_preview_handles_non_object() {
        assert_eq!(payload_preview(&json!(null)), "null");
        assert_eq!(payload_preview(&json!(42)), "42");
    }

    #[test]
    fn payload_preview_escapes_ansi_control_bytes() {
        // Regression: raw `\x1b]11;rgb:0/0/0\x1b\\` in a payload
        // string reached stdout and reconfigured VTE's background.
        // The renderer must JSON-escape control bytes so the terminal
        // sees the literal sequence, not the active control code.
        let v = json!({"msg": "\u{1b}]11;rgb:0/0/0\u{1b}\\"});
        let rendered = payload_preview(&v);
        assert!(
            !rendered.contains('\u{1b}'),
            "ESC must not appear unescaped in output, got {rendered:?}"
        );
        assert!(
            rendered.contains("\\u001b") || rendered.contains("\\u001B"),
            "ESC must be JSON-escaped, got {rendered:?}"
        );
    }

    #[test]
    fn payload_preview_does_not_panic_on_multibyte_boundary() {
        // Codex C2 round 1: byte-index truncate panics if the 120th
        // byte lands inside a multibyte UTF-8 char. Construct a value
        // that fills the buffer with Korean text and assert we get a
        // non-panicking, ellipsis-terminated rendering.
        let body = "한글".repeat(100); // 600 bytes, 200 chars
        let v = json!({ "body": body });
        let s = payload_preview(&v);
        assert!(s.ends_with('…'), "got {s}");
    }
}
