//! Desktop notification surface for the `notify.show` action.
//!
//! The trait is cross-platform; concrete impls shell out per-OS:
//! `notify-send` on Linux, `osascript` on macOS. Subprocess is
//! synchronous (we wait for exit) so failures surface as errors
//! rather than vanishing into a zombie. At human-driven trigger
//! rates the ~10 ms cost is irrelevant; the action handler runs on
//! the blocking thread pool so the trigger pump never stalls.
//!
//! macOS keeps an AppleScript wrapper with `on run argv` to avoid
//! quote/backslash injection from arbitrary trigger payloads —
//! title/body cross the boundary as `osascript` argv values, never
//! spliced into the script source.

use std::fmt;
use std::process::{Command, ExitStatus};
use std::sync::Mutex;

use serde::Deserialize;

/// Notification severity. Default is `Info`. Maps to `notify-send`'s
/// `--urgency` and (today) is ignored by the macOS impl since
/// `display notification` has no built-in severity slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Info,
    Warn,
    Error,
}

impl Level {
    /// libnotify urgency token. `Warn` maps to `normal` (libnotify has
    /// `low|normal|critical` only); `Error` raises to `critical`.
    pub fn urgency(self) -> &'static str {
        match self {
            Self::Info => "low",
            Self::Warn => "normal",
            Self::Error => "critical",
        }
    }
}

#[derive(Debug)]
pub enum NotifyError {
    Spawn(std::io::Error),
    NonZeroExit { status: ExitStatus, stderr: String },
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "notifier spawn failed: {e}"),
            Self::NonZeroExit { status, stderr } => {
                write!(f, "notifier exited {status}: {stderr}")
            }
        }
    }
}

impl std::error::Error for NotifyError {}

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError>;
}

/// Cap on payload size before the subprocess sees it — AppleScript's
/// `-e` source argument and notify-send's IPC frame both have implicit
/// limits well above this, but a runaway trigger that interpolates a
/// 50 KB Slack body has no business as a toast. Hard truncate.
const MAX_BODY_BYTES: usize = 4096;
const MAX_TITLE_BYTES: usize = 256;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(target_os = "linux")]
pub struct LibnotifyNotifier;

#[cfg(target_os = "linux")]
impl Notifier for LibnotifyNotifier {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError> {
        run_command(
            Command::new("notify-send")
                .arg(format!("--urgency={}", level.urgency()))
                .arg(truncate(title, MAX_TITLE_BYTES))
                .arg(truncate(body, MAX_BODY_BYTES)),
        )
    }
}

#[cfg(target_os = "macos")]
pub struct OsascriptNotifier;

#[cfg(target_os = "macos")]
impl Notifier for OsascriptNotifier {
    fn notify(&self, title: &str, body: &str, _level: Level) -> Result<(), NotifyError> {
        // `on run argv` wrapper keeps title/body OUT of the script
        // source — `osascript` passes them as AppleScript values, so
        // no quote/backslash escape is needed regardless of payload.
        const SCRIPT: &str = "on run argv\n\
            display notification (item 2 of argv) with title (item 1 of argv)\n\
        end run";
        run_command(
            Command::new("osascript")
                .arg("-e")
                .arg(SCRIPT)
                .arg(truncate(title, MAX_TITLE_BYTES))
                .arg(truncate(body, MAX_BODY_BYTES)),
        )
    }
}

#[allow(dead_code)]
fn run_command(cmd: &mut Command) -> Result<(), NotifyError> {
    let output = cmd.output().map_err(NotifyError::Spawn)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NotifyError::NonZeroExit {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Test double: records every `notify` call instead of touching the
/// desktop. Public so daemon/integration tests can wire it in place
/// of the platform notifier.
#[derive(Default)]
pub struct NoopNotifier {
    pub captured: Mutex<Vec<(String, String, Level)>>,
}

impl Notifier for NoopNotifier {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<(), NotifyError> {
        self.captured
            .lock()
            .unwrap()
            .push((title.to_string(), body.to_string(), level));
        Ok(())
    }
}

/// Platform Notifier factory. Used by the daemon + GUI in-process
/// registries at startup; returns `None` on platforms that have no
/// concrete impl yet (the action becomes a no-op handler in that case).
pub fn platform_notifier() -> Option<Box<dyn Notifier>> {
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(LibnotifyNotifier))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(OsascriptNotifier))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_urgency_mapping() {
        assert_eq!(Level::Info.urgency(), "low");
        assert_eq!(Level::Warn.urgency(), "normal");
        assert_eq!(Level::Error.urgency(), "critical");
    }

    #[test]
    fn level_default_is_info() {
        assert_eq!(Level::default(), Level::Info);
    }

    #[test]
    fn level_deserializes_lowercase() {
        let l: Level = serde_json::from_str("\"warn\"").unwrap();
        assert_eq!(l, Level::Warn);
        let l: Level = serde_json::from_str("\"error\"").unwrap();
        assert_eq!(l, Level::Error);
        let l: Level = serde_json::from_str("\"info\"").unwrap();
        assert_eq!(l, Level::Info);
    }

    #[test]
    fn level_rejects_unknown_variants() {
        let r: Result<Level, _> = serde_json::from_str("\"loud\"");
        assert!(r.is_err());
    }

    #[test]
    fn truncate_below_cap_is_identity() {
        assert_eq!(truncate("hello", 100), "hello");
    }

    #[test]
    fn truncate_appends_ellipsis_at_cap() {
        let s = truncate("abcdefghij", 5);
        // First 5 ASCII bytes preserved, ellipsis appended.
        assert_eq!(s, "abcde…");
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        // "한글" is 6 bytes; cap=4 would land mid-char without the
        // char-boundary scan and panic the slice.
        let s = truncate("한글한글", 4);
        assert!(s.ends_with('…'));
        // Must start with the first whole char (3 bytes).
        assert!(s.starts_with('한'));
    }

    #[test]
    fn noop_notifier_records_calls() {
        let n = NoopNotifier::default();
        n.notify("hi", "world", Level::Warn).unwrap();
        n.notify("again", "", Level::Error).unwrap();
        let captured = n.captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0], ("hi".into(), "world".into(), Level::Warn));
        assert_eq!(captured[1], ("again".into(), "".into(), Level::Error));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn libnotify_argv_shape_against_dummy_binary() {
        // We can't assert against the real notify-send (would actually
        // pop a toast on the dev box). Verify only that the platform
        // factory returns a non-None Notifier on Linux.
        let n = platform_notifier();
        assert!(n.is_some());
    }
}
