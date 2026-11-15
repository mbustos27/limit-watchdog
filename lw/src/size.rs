//! How big an agent's transcript is, from file metadata only. The file is never opened.
//!
//! Transcripts live at `%USERPROFILE%\.claude\projects\<project>\<session_id>\subagents\agent-<id>.jsonl`.
//! `LW_PROJECTS_DIR` points somewhere else (tests).
use std::path::{Path, PathBuf};

use crate::ledger::{Agent, Kind, safe_id};

pub const PROJECTS_ENV: &str = "LW_PROJECTS_DIR";

pub fn projects_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(PROJECTS_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// The transcript's path and size in bytes, if there is one. Reads directory entries and file metadata, never contents.
pub fn find(root: &Path, session_id: &str, agent_id: &str) -> Option<(PathBuf, u64)> {
    if !safe_id(session_id) || !safe_id(agent_id) {
        return None;
    }
    let file = format!("agent-{agent_id}.jsonl");
    let mut found: Option<(PathBuf, u64)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let p = entry.path().join(session_id).join("subagents").join(&file);
        if let Ok(md) = std::fs::metadata(&p)
            && md.is_file()
            && found.as_ref().is_none_or(|(_, b)| md.len() > *b)
        {
            found = Some((p, md.len()));
        }
    }
    found
}

/// Resume by id, or relaunch fresh from the brief and handoff note.
/// Re-dispatch when the agent is a redispatch kind (Explore, Plan), when its transcript is over the threshold,
/// or when no transcript was found (there is nothing to resume from).
pub fn mode(agent: Option<&Agent>, bytes: Option<u64>, over: u64) -> (&'static str, &'static str) {
    if agent.is_some_and(|a| a.kind == Kind::Redispatch) {
        return ("redispatch", "kind redispatch");
    }
    match bytes {
        None => ("redispatch", "no transcript found"),
        Some(b) if b > over => ("redispatch", "transcript over the size limit"),
        Some(_) => ("resume", "transcript within the size limit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_largest_match_by_metadata_and_refuses_unsafe_ids() {
        let dir = tempfile::tempdir().unwrap();
        for (proj, len) in [("p1", 10usize), ("p2", 30)] {
            let d = dir.path().join(proj).join("sess").join("subagents");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("agent-a1.jsonl"), vec![b'x'; len]).unwrap();
        }
        let (p, b) = find(dir.path(), "sess", "a1").unwrap();
        assert_eq!(b, 30);
        assert!(p.ends_with("p2/sess/subagents/agent-a1.jsonl") || p.ends_with("p2\\sess\\subagents\\agent-a1.jsonl"));
        assert!(find(dir.path(), "sess", "a2").is_none());
        assert!(find(dir.path(), "..", "a1").is_none());
        assert!(find(dir.path(), "sess", "../a1").is_none());
    }

    #[test]
    fn mode_rules() {
        assert_eq!(mode(None, Some(10), 100).0, "resume");
        assert_eq!(mode(None, Some(101), 100).0, "redispatch");
        assert_eq!(mode(None, Some(100), 100).0, "resume");
        assert_eq!(mode(None, None, 100).0, "redispatch");
    }
}
