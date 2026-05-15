use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use vte4::prelude::*;

// VTE rejects the regex unless PCRE2_MULTILINE is set. The Rust binding
// doesn't re-export PCRE2 constants from `pcre2.h`, but the values are
// stable across PCRE2 versions. Caseless matching is expressed inline
// with `(?i:...)` rather than via `PCRE2_CASELESS`: empirically, mixing
// the compile-time flag with a non-trivial pattern made VTE 0.84's
// `check_match_at` return no matches on otherwise valid hits (gnome-
// console also uses bare `PCRE2_MULTILINE` and inline case flags).
const PCRE2_MULTILINE: u32 = 0x0000_0400;

const URL_PATTERN: &str = r"(?i:https?://[^\s<>'\x22]+)";

// Characters that commonly trail a URL in prose ("see https://x.com.")
// and almost never appear at the end of a real URL. Stripped post-match
// so the regex stays a simple greedy run.
const TRAIL_PUNCT: &str = ".,;:!?)]}>";

pub fn normalize_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches(|c: char| TRAIL_PUNCT.contains(c));
    if trimmed.is_empty() {
        return None;
    }
    let lower_head: String = trimmed
        .chars()
        .take(8)
        .flat_map(|c| c.to_lowercase())
        .collect();
    if lower_head.starts_with("http://") || lower_head.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

pub fn install(terminal: &vte4::Terminal, window: &gtk4::ApplicationWindow) {
    terminal.set_allow_hyperlink(true);

    match vte4::Regex::for_match(URL_PATTERN, PCRE2_MULTILINE) {
        Ok(regex) => {
            let tag = terminal.match_add_regex(&regex, 0);
            terminal.match_set_cursor_name(tag, "pointer");
        }
        Err(e) => {
            eprintln!("[nestty] url regex compile failed: {e}");
        }
    }

    let click = gtk4::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let term = terminal.clone();
    let win = window.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        let state = gesture.current_event_state();
        if !state.contains(gdk::ModifierType::CONTROL_MASK) {
            return;
        }
        let Some(url) = check_url_at(&term, x, y) else {
            return;
        };
        let Some(safe) = normalize_url(&url) else {
            return;
        };
        let launcher = gtk4::UriLauncher::new(&safe);
        launcher.launch(
            Some(&win),
            gtk4::gio::Cancellable::NONE,
            |result: Result<(), glib::Error>| {
                if let Err(e) = result {
                    eprintln!("[nestty] url launch failed: {e}");
                }
            },
        );
        gesture.set_state(gtk4::EventSequenceState::Claimed);
    });
    terminal.add_controller(click);
}

fn check_url_at(term: &vte4::Terminal, x: f64, y: f64) -> Option<String> {
    // OSC 8 hyperlinks beat the regex tag because their visible label
    // can differ from the target — the regex would only see the label
    // ("click here"), so the explicit hyperlink target is the only
    // trustworthy source.
    if let Some(h) = term.check_hyperlink_at(x, y) {
        return Some(h.to_string());
    }
    let (m, _tag) = term.check_match_at(x, y);
    m.map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_http() {
        assert_eq!(
            normalize_url("http://example.com/path"),
            Some("http://example.com/path".to_string())
        );
    }

    #[test]
    fn normalize_accepts_https_with_port_and_fragment() {
        assert_eq!(
            normalize_url("https://example.com:8080/p?q=1#frag"),
            Some("https://example.com:8080/p?q=1#frag".to_string())
        );
    }

    #[test]
    fn normalize_strips_trailing_punctuation() {
        assert_eq!(
            normalize_url("https://example.com."),
            Some("https://example.com".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com/path)."),
            Some("https://example.com/path".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com?x=1,"),
            Some("https://example.com?x=1".to_string())
        );
    }

    #[test]
    fn normalize_rejects_non_http_schemes() {
        assert_eq!(normalize_url("file:///etc/passwd"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("ftp://example.com"), None);
        assert_eq!(normalize_url("mailto:a@b.com"), None);
    }

    #[test]
    fn normalize_rejects_plain_text() {
        assert_eq!(normalize_url("not a url"), None);
        assert_eq!(normalize_url(""), None);
        assert_eq!(normalize_url("."), None);
    }

    #[test]
    fn normalize_handles_uppercase_scheme() {
        assert_eq!(
            normalize_url("HTTPS://Example.com"),
            Some("HTTPS://Example.com".to_string())
        );
    }
}
