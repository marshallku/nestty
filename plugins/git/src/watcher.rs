//! Polling file watcher for `.git/` state. Emits `git.worktree_created`,
//! `git.worktree_removed`, `git.branch_created`, `git.branch_deleted`,
//! and `git.checkout` per workspace.
//!
//! Polling (not `notify`) keeps this dependency-free and works
//! identically on Linux and macOS. For status-bar / live-indicator
//! use cases the ~poll-interval lag is fine. `git.worktree_add` and
//! friends still publish their own `.completed` event directly from
//! the action handler (Phase 14.1's registry fan-out), so chained
//! triggers don't pay the polling cost.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Config, Workspace};

const DEFAULT_POLL_MS: u64 = 2000;
const MIN_POLL_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherSnapshot {
    pub head: Option<String>,
    pub branches: HashSet<String>,
    pub worktrees: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Checkout { head: String },
    BranchCreated { name: String },
    BranchDeleted { name: String },
    WorktreeCreated { name: String },
    WorktreeRemoved { name: String },
}

/// Per-worktree gitdir (where HEAD lives) and shared common-gitdir
/// (where refs/heads/ and worktrees/ live). For a primary checkout the
/// two are the same `<path>/.git`. For a secondary worktree configured
/// directly as a workspace, `.git` is a file pointing at
/// `<primary>/.git/worktrees/<name>` — and refs are still shared with
/// the primary. We delegate to `git rev-parse` so all repo shapes
/// (primary, secondary worktree, submodule with gitlink, custom
/// `core.worktree`) resolve to the right pair without our own parser.
#[derive(Debug, Clone)]
struct GitDirs {
    gitdir: std::path::PathBuf,
    common: std::path::PathBuf,
}

fn resolve_git_dirs(workspace_path: &Path) -> Option<GitDirs> {
    let gitdir = run_rev_parse(workspace_path, "--git-dir")?;
    // `--git-common-dir` is supported since git 2.5. On older gits it
    // returns the same value as `--git-dir`, which is wrong for
    // secondary worktrees but degrades cleanly (we miss refs/worktrees
    // events instead of crashing).
    let common =
        run_rev_parse(workspace_path, "--git-common-dir").unwrap_or_else(|| gitdir.clone());
    Some(GitDirs { gitdir, common })
}

fn run_rev_parse(workspace_path: &Path, flag: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_path)
        .arg("rev-parse")
        .arg(flag)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(trimmed);
    // git returns relative paths when -C points at the worktree root;
    // resolve them against the workspace so subsequent file reads
    // don't depend on cwd at snapshot time.
    if p.is_absolute() {
        Some(p)
    } else {
        Some(workspace_path.join(p))
    }
}

pub fn snapshot(workspace_path: &Path) -> WatcherSnapshot {
    let Some(dirs) = resolve_git_dirs(workspace_path) else {
        return WatcherSnapshot {
            head: None,
            branches: HashSet::new(),
            worktrees: HashSet::new(),
        };
    };
    snapshot_with_dirs(&dirs)
}

fn snapshot_with_dirs(dirs: &GitDirs) -> WatcherSnapshot {
    let head = std::fs::read_to_string(dirs.gitdir.join("HEAD"))
        .ok()
        .map(|s| s.trim().to_string());

    let mut branches = HashSet::new();
    let refs_root = dirs.common.join("refs/heads");
    collect_refs(&refs_root, &refs_root, &mut branches);

    let mut worktrees = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dirs.common.join("worktrees")) {
        for ent in rd.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Some(name) = ent.file_name().to_str() {
                worktrees.insert(name.to_string());
            }
        }
    }
    WatcherSnapshot {
        head,
        branches,
        worktrees,
    }
}

/// Recursive walk of `refs/heads/` — branch names with slashes
/// (`feat/foo`) live as nested files, so a flat readdir would miss
/// them. Packed refs (`.git/packed-refs`) are intentionally NOT
/// scanned: branches that exist only in packed-refs are
/// pre-established as of `git gc` time and don't represent
/// user-initiated changes within the watching window.
fn collect_refs(base: &Path, dir: &Path, out: &mut HashSet<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let ft = ent.file_type();
        if let Ok(ft) = ft {
            if ft.is_dir() {
                collect_refs(base, &path, out);
            } else if ft.is_file()
                && let Ok(rel) = path.strip_prefix(base)
            {
                out.insert(rel.to_string_lossy().to_string());
            }
        }
    }
}

pub fn diff(prev: &WatcherSnapshot, curr: &WatcherSnapshot) -> Vec<WatchEvent> {
    let mut events = Vec::new();
    if prev.head != curr.head
        && let Some(h) = &curr.head
    {
        events.push(WatchEvent::Checkout { head: h.clone() });
    }
    let mut created_branches: Vec<&String> = curr.branches.difference(&prev.branches).collect();
    created_branches.sort();
    for name in created_branches {
        events.push(WatchEvent::BranchCreated { name: name.clone() });
    }
    let mut deleted_branches: Vec<&String> = prev.branches.difference(&curr.branches).collect();
    deleted_branches.sort();
    for name in deleted_branches {
        events.push(WatchEvent::BranchDeleted { name: name.clone() });
    }
    let mut created_worktrees: Vec<&String> = curr.worktrees.difference(&prev.worktrees).collect();
    created_worktrees.sort();
    for name in created_worktrees {
        events.push(WatchEvent::WorktreeCreated { name: name.clone() });
    }
    let mut deleted_worktrees: Vec<&String> = prev.worktrees.difference(&curr.worktrees).collect();
    deleted_worktrees.sort();
    for name in deleted_worktrees {
        events.push(WatchEvent::WorktreeRemoved { name: name.clone() });
    }
    events
}

pub fn to_frame(event: &WatchEvent, workspace: &str) -> Value {
    let (kind, extra) = match event {
        WatchEvent::Checkout { head } => ("git.checkout", json!({"head": head})),
        WatchEvent::BranchCreated { name } => ("git.branch_created", json!({"name": name})),
        WatchEvent::BranchDeleted { name } => ("git.branch_deleted", json!({"name": name})),
        WatchEvent::WorktreeCreated { name } => ("git.worktree_created", json!({"name": name})),
        WatchEvent::WorktreeRemoved { name } => ("git.worktree_removed", json!({"name": name})),
    };
    let mut payload = json!({"workspace": workspace});
    if let (Value::Object(p), Value::Object(e)) = (&mut payload, &extra) {
        for (k, v) in e {
            p.insert(k.clone(), v.clone());
        }
    }
    json!({
        "method": "event.publish",
        "params": {
            "kind": kind,
            "payload": payload,
        }
    })
}

fn poll_interval() -> Duration {
    let raw = std::env::var("NESTTY_GIT_POLL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_POLL_MS);
    Duration::from_millis(raw.max(MIN_POLL_MS))
}

/// Spawn one polling thread per workspace. Threads are detached
/// (`thread::spawn`) and exit on process termination. The watchers
/// emit events via `event_tx`; the writer thread reading that
/// channel is part of the plugin's main wire (`main.rs`).
pub fn spawn(
    config: Arc<Config>,
    event_tx: Sender<String>,
    stop: Arc<AtomicBool>,
    initialized: Arc<AtomicBool>,
) {
    let interval = poll_interval();
    for ws in &config.workspaces {
        let workspace = ws.clone();
        let tx = event_tx.clone();
        let stop_flag = stop.clone();
        let init = initialized.clone();
        thread::spawn(move || {
            run_one(&workspace, tx, stop_flag, init, interval);
        });
    }
}

fn run_one(
    workspace: &Workspace,
    tx: Sender<String>,
    stop: Arc<AtomicBool>,
    initialized: Arc<AtomicBool>,
    interval: Duration,
) {
    // Hold events back until the host completes the `initialize` →
    // `initialized` handshake. Publishing earlier risks the supervisor
    // dropping events as out-of-protocol.
    while !initialized.load(Ordering::SeqCst) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let mut prev = snapshot(&workspace.path);
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(interval);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let curr = snapshot(&workspace.path);
        let events = diff(&prev, &curr);
        for ev in events {
            let frame = to_frame(&ev, &workspace.name);
            if tx.send(frame.to_string()).is_err() {
                return;
            }
        }
        prev = curr;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(head: Option<&str>, branches: &[&str], worktrees: &[&str]) -> WatcherSnapshot {
        WatcherSnapshot {
            head: head.map(|s| s.to_string()),
            branches: branches.iter().map(|s| s.to_string()).collect(),
            worktrees: worktrees.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn diff_no_change_returns_empty() {
        let a = snap(Some("ref: refs/heads/main"), &["main"], &["feat-x"]);
        let b = a.clone();
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_head_change_emits_checkout() {
        let a = snap(Some("ref: refs/heads/main"), &["main"], &[]);
        let b = snap(Some("ref: refs/heads/feat-x"), &["main"], &[]);
        let events = diff(&a, &b);
        assert_eq!(
            events,
            vec![WatchEvent::Checkout {
                head: "ref: refs/heads/feat-x".into()
            }]
        );
    }

    #[test]
    fn diff_branch_create_and_delete() {
        let a = snap(Some("ref: refs/heads/main"), &["main", "old"], &[]);
        let b = snap(Some("ref: refs/heads/main"), &["main", "new"], &[]);
        let events = diff(&a, &b);
        assert_eq!(
            events,
            vec![
                WatchEvent::BranchCreated { name: "new".into() },
                WatchEvent::BranchDeleted { name: "old".into() },
            ]
        );
    }

    #[test]
    fn diff_worktree_create_and_delete() {
        let a = snap(Some("ref: refs/heads/main"), &["main"], &["old-wt"]);
        let b = snap(Some("ref: refs/heads/main"), &["main"], &["new-wt"]);
        let events = diff(&a, &b);
        assert_eq!(
            events,
            vec![
                WatchEvent::WorktreeCreated {
                    name: "new-wt".into()
                },
                WatchEvent::WorktreeRemoved {
                    name: "old-wt".into()
                },
            ]
        );
    }

    #[test]
    fn diff_no_checkout_when_head_cleared() {
        // .git/HEAD missing (or unreadable) leaves curr.head=None.
        // We don't emit a checkout for "branch went away" — that's
        // the branch_deleted signal's job. Avoids a noisy null head
        // during transient races.
        let a = snap(Some("ref: refs/heads/main"), &["main"], &[]);
        let b = snap(None, &["main"], &[]);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_multiple_changes_at_once_are_all_emitted() {
        let a = snap(Some("ref: refs/heads/main"), &["main"], &["wt-a"]);
        let b = snap(
            Some("ref: refs/heads/feat-x"),
            &["main", "feat-x"],
            &["wt-a", "wt-b"],
        );
        let events = diff(&a, &b);
        // Ordering: HEAD first, then sorted-name creates/deletes.
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], WatchEvent::Checkout { .. }));
        assert!(events.contains(&WatchEvent::BranchCreated {
            name: "feat-x".into()
        }));
        assert!(events.contains(&WatchEvent::WorktreeCreated {
            name: "wt-b".into()
        }));
    }

    #[test]
    fn to_frame_branch_created_payload_shape() {
        let f = to_frame(
            &WatchEvent::BranchCreated {
                name: "feat-x".into(),
            },
            "nestty",
        );
        assert_eq!(f["method"], "event.publish");
        assert_eq!(f["params"]["kind"], "git.branch_created");
        assert_eq!(f["params"]["payload"]["workspace"], "nestty");
        assert_eq!(f["params"]["payload"]["name"], "feat-x");
    }

    #[test]
    fn snapshot_secondary_worktree_resolves_via_git_rev_parse() {
        // Codex C1 round 1: `.git` in a secondary worktree is a FILE
        // (gitlink), not a directory. The naive `<path>/.git/HEAD`
        // read fails silently. We must resolve via `git rev-parse
        // --git-dir` (per-worktree HEAD) + `--git-common-dir`
        // (shared refs/heads + worktrees).
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        for cmd in [
            vec!["init", "--initial-branch=main", "."],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
            vec!["commit", "--allow-empty", "-m", "i"],
            vec!["branch", "feat/x"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&primary)
                .args(&cmd)
                .status()
                .unwrap();
            assert!(status.success(), "git {cmd:?} failed");
        }
        let secondary = dir.path().join("primary-worktrees/feat-x");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&primary)
            .args(["worktree", "add", secondary.to_str().unwrap(), "feat/x"])
            .status()
            .unwrap();
        assert!(status.success(), "git worktree add failed");

        // Configure the SECONDARY as the workspace. `.git` here is a
        // file with `gitdir: ...` — not a directory. snapshot() must
        // still find HEAD (per-worktree, on feat/x) and the shared
        // refs/heads (main + feat/x) and worktrees registry.
        assert!(
            secondary.join(".git").is_file(),
            ".git should be a gitlink file in a secondary worktree"
        );
        let snap = snapshot(&secondary);
        assert_eq!(
            snap.head.as_deref(),
            Some("ref: refs/heads/feat/x"),
            "secondary worktree's HEAD must come from per-worktree gitdir"
        );
        assert!(
            snap.branches.contains("main"),
            "main (shared) must appear via common-gitdir"
        );
        assert!(
            snap.branches.contains("feat/x"),
            "feat/x (shared) must appear via common-gitdir"
        );
        assert!(
            snap.worktrees.contains("feat-x"),
            "worktrees registry comes from common-gitdir"
        );
    }

    #[test]
    fn snapshot_real_repo_picks_up_head_branches_and_worktrees() {
        // E2E sanity over a real `git init`: confirms our parser
        // shape matches what git actually writes. Tests use the
        // same tempdir-and-shell-out pattern as the rest of this
        // crate.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for cmd in [
            vec!["init", "--initial-branch=main", "."],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
            vec!["commit", "--allow-empty", "-m", "i"],
            vec!["branch", "feat/x"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&cmd)
                .status()
                .unwrap();
            assert!(status.success(), "git {cmd:?} failed");
        }
        let wt_root = dir.path().join("repo-worktrees");
        let wt_path = wt_root.join("feat-x");
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", wt_path.to_str().unwrap(), "feat/x"])
            .status()
            .unwrap();
        assert!(status.success(), "git worktree add failed");

        let snap = snapshot(&repo);
        assert_eq!(snap.head.as_deref(), Some("ref: refs/heads/main"));
        assert!(
            snap.branches.contains("main"),
            "expected main in {:?}",
            snap.branches
        );
        assert!(
            snap.branches.contains("feat/x"),
            "expected feat/x (nested ref) in {:?}",
            snap.branches
        );
        assert_eq!(snap.worktrees.len(), 1);
        assert!(snap.worktrees.contains("feat-x"));
    }

    #[test]
    fn to_frame_checkout_payload_shape() {
        let f = to_frame(
            &WatchEvent::Checkout {
                head: "ref: refs/heads/feat-x".into(),
            },
            "ws-a",
        );
        assert_eq!(f["params"]["kind"], "git.checkout");
        assert_eq!(f["params"]["payload"]["head"], "ref: refs/heads/feat-x");
    }
}
