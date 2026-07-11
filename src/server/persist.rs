//! Session persistence: automatic JSON snapshots of every session's
//! layout and each tab's working directory, restored at server startup
//! unless the config disables it. Deliberately narrow: no scrollback
//! replay, and agent-session resume covers only Claude Code, the one
//! agent lux detects.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::server::layout::{Node, Split, SplitKind, WindowId};

/// Everything the server persists: one entry per session, in the order
/// `ls` and the switcher present them.
#[derive(Serialize, Deserialize)]
pub struct StateSnapshot {
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub name: String,
    pub tree: NodeSnapshot,
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// The leaf id this window occupies in `tree`.
    pub id: WindowId,
    /// Index of the active tab.
    pub active: usize,
    pub tabs: Vec<TabSnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnapshot {
    pub cwd: PathBuf,
    /// Claude Code session id to resume, present when the tab was
    /// identified as running Claude Code at save time.
    pub claude_session: Option<String>,
}

/// The layout tree, decoupled from the in-memory `Node` so the on-disk
/// format doesn't shift under refactors.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub enum NodeSnapshot {
    Leaf(WindowId),
    Split {
        kind: SplitKindSnapshot,
        ratio: f64,
        first: Box<NodeSnapshot>,
        second: Box<NodeSnapshot>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitKindSnapshot {
    SideBySide,
    Stacked,
}

pub fn capture_node(node: &Node) -> NodeSnapshot {
    match node {
        Node::Leaf(id) => NodeSnapshot::Leaf(*id),
        Node::Split(s) => NodeSnapshot::Split {
            kind: match s.kind {
                SplitKind::SideBySide => SplitKindSnapshot::SideBySide,
                SplitKind::Stacked => SplitKindSnapshot::Stacked,
            },
            ratio: s.ratio,
            first: Box::new(capture_node(&s.first)),
            second: Box::new(capture_node(&s.second)),
        },
    }
}

pub fn restore_node(snap: &NodeSnapshot) -> Node {
    match snap {
        NodeSnapshot::Leaf(id) => Node::Leaf(*id),
        NodeSnapshot::Split {
            kind,
            ratio,
            first,
            second,
        } => Node::Split(Split {
            kind: match kind {
                SplitKindSnapshot::SideBySide => SplitKind::SideBySide,
                SplitKindSnapshot::Stacked => SplitKind::Stacked,
            },
            ratio: *ratio,
            first: Box::new(restore_node(first)),
            second: Box::new(restore_node(second)),
        }),
    }
}

/// `$XDG_STATE_HOME/lux/session.json`, falling back to
/// `~/.local/state/lux/session.json`.
fn state_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("lux").join("session.json"))
}

/// Write the serialized snapshot, atomically via a temp-file rename so a
/// crash mid-write never leaves a truncated file.
pub fn save(json: &str) {
    let Some(path) = state_path() else {
        return;
    };
    if let Err(err) = save_to(&path, json) {
        eprintln!("lux: save {}: {err}", path.display());
    }
}

fn save_to(path: &Path, json: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Load the persisted snapshot; any failure (no file, unreadable,
/// unparsable) means starting fresh.
pub fn load() -> Option<StateSnapshot> {
    let path = state_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            eprintln!("lux: read {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            eprintln!("lux: parse {}: {err}", path.display());
            None
        }
    }
}

/// The Claude Code session transcripts recorded for `cwd`, as (session
/// id, transcript creation time), oldest first. Claude Code stores each
/// session's transcript as
/// `~/.claude/projects/<encoded cwd>/<session id>.jsonl`, created when
/// the session starts and kept through resumes, so a transcript's birth
/// time identifies which running instance created it. Lux reads these
/// files because it has no channel over which Claude Code reports its
/// session id directly.
pub fn claude_sessions(cwd: &Path) -> Vec<(String, std::time::SystemTime)> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encode_project_dir(cwd));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // Filesystems without birth times fall back to mtime, degrading
        // toward the old newest-file guess rather than dropping the tab.
        let Ok(created) = entry
            .metadata()
            .and_then(|meta| meta.created().or_else(|_| meta.modified()))
        else {
            continue;
        };
        sessions.push((stem.to_string(), created));
    }
    sessions.sort_by_key(|(_, created)| *created);
    sessions
}

/// Slack for comparing a transcript's creation time against when lux
/// first saw the tab running Claude Code: the transcript can appear
/// slightly before detection (Claude writes it at startup, detection
/// waits on the next frame).
const CLAUDE_MATCH_SLACK: std::time::Duration = std::time::Duration::from_secs(2);

/// Match Claude Code tabs to session transcripts within one project
/// directory: for each tab (by the time it was first seen running Claude
/// Code, which must be ascending), the oldest unclaimed transcript
/// created no earlier than the tab appeared. Each transcript is claimed
/// at most once, transcripts predating every tab are never claimed, and
/// a tab whose transcript hasn't appeared yet gets `None`. `claimed`
/// holds ids already owned elsewhere (resumed tabs, earlier matches) and
/// grows with each match.
pub fn match_claude_sessions(
    since: &[std::time::SystemTime],
    transcripts: &[(String, std::time::SystemTime)],
    claimed: &mut std::collections::HashSet<String>,
) -> Vec<Option<String>> {
    since
        .iter()
        .map(|seen| {
            let earliest = seen
                .checked_sub(CLAUDE_MATCH_SLACK)
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let id = transcripts
                .iter()
                .find(|(id, created)| *created >= earliest && !claimed.contains(id))
                .map(|(id, _)| id.clone())?;
            claimed.insert(id.clone());
            Some(id)
        })
        .collect()
}

/// Claude Code's project-directory encoding: every character outside
/// [A-Za-z0-9] becomes `-`.
fn encode_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::layout;

    #[test]
    fn node_conversion_round_trips() {
        let mut tree = Node::Leaf(1);
        layout::split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        layout::split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        let snap = capture_node(&tree);
        let restored = restore_node(&snap);
        assert_eq!(layout::leaves(&restored), layout::leaves(&tree));
        assert_eq!(capture_node(&restored), snap);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = StateSnapshot {
            sessions: vec![SessionSnapshot {
                name: "work".into(),
                tree: NodeSnapshot::Split {
                    kind: SplitKindSnapshot::SideBySide,
                    ratio: 0.6,
                    first: Box::new(NodeSnapshot::Leaf(0)),
                    second: Box::new(NodeSnapshot::Leaf(1)),
                },
                windows: vec![
                    WindowSnapshot {
                        id: 0,
                        active: 1,
                        tabs: vec![
                            TabSnapshot {
                                cwd: "/tmp".into(),
                                claude_session: None,
                            },
                            TabSnapshot {
                                cwd: "/home".into(),
                                claude_session: Some("abc-123".into()),
                            },
                        ],
                    },
                    WindowSnapshot {
                        id: 1,
                        active: 0,
                        tabs: vec![TabSnapshot {
                            cwd: "/".into(),
                            claude_session: None,
                        }],
                    },
                ],
            }],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.sessions.len(), 1);
        let session = &restored.sessions[0];
        assert_eq!(session.name, "work");
        assert_eq!(session.tree, snapshot.sessions[0].tree);
        assert_eq!(session.windows[0].active, 1);
        assert_eq!(
            session.windows[0].tabs[1].claude_session.as_deref(),
            Some("abc-123")
        );
        assert_eq!(session.windows[1].tabs[0].cwd, PathBuf::from("/"));
    }

    #[test]
    fn transcript_matching_keeps_concurrent_tabs_distinct() {
        use std::time::{Duration, SystemTime};
        let t = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        // Three transcripts in one project: one from a long-closed
        // session, then one per running claude tab, oldest first.
        let transcripts = vec![
            ("old".to_string(), t(100)),
            ("first".to_string(), t(1000)),
            ("second".to_string(), t(2000)),
        ];
        let mut claimed = std::collections::HashSet::new();
        // Two tabs, first seen shortly after their transcripts appeared.
        let ids = match_claude_sessions(&[t(1001), t(2001)], &transcripts, &mut claimed);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_deref(), Some("first"));
        assert_eq!(ids[1].as_deref(), Some("second"));
        // The stale transcript predates both tabs and is never claimed.
        assert!(!claimed.contains("old"));

        // An id already owned (a resumed tab) is skipped, and a tab
        // whose transcript hasn't appeared yet gets nothing.
        let mut claimed = std::collections::HashSet::from(["first".to_string()]);
        let ids = match_claude_sessions(&[t(1001), t(9000)], &transcripts, &mut claimed);
        assert_eq!(ids[0].as_deref(), Some("second"));
        assert_eq!(ids[1], None);

        // A transcript created moments before detection (claude writes
        // it at startup; detection waits on the next frame) still
        // matches through the slack.
        let mut claimed = std::collections::HashSet::new();
        let ids = match_claude_sessions(&[t(1001)], &[("first".into(), t(1000))], &mut claimed);
        assert_eq!(ids[0].as_deref(), Some("first"));
    }

    #[test]
    fn project_dir_encoding_matches_claude_code() {
        assert_eq!(
            encode_project_dir(Path::new("/home/user/src/lux")),
            "-home-user-src-lux"
        );
        assert_eq!(
            encode_project_dir(Path::new("/tmp/my_dir.v2")),
            "-tmp-my-dir-v2"
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "lux-persist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("session.json");
        save_to(&path, r#"{"sessions":[]}"#).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let snapshot: StateSnapshot = serde_json::from_str(&text).unwrap();
        assert!(snapshot.sessions.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
