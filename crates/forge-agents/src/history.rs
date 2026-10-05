//! Agent conversation history, one JSON file per session under
//! `<data>/agent-sessions/<project key>/`.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum RecordEntry {
    User { text: String, context: Vec<String> },
    Agent { text: String },
    Thought { text: String },
    Tool { title: String, kind: String, status: String },
    Plan { items: Vec<(String, String)> },
    System { text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The agent's session id (used for `session/load`).
    pub session_id: String,
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub project_root: PathBuf,
    pub title: String,
    pub started_at: u64,
    pub updated_at: u64,
    pub entries: Vec<RecordEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub agent_id: String,
    pub title: String,
    pub updated_at: u64,
    pub file: PathBuf,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// Sessions of one project live together; the key is stable across runs.
pub fn project_dir(base: &Path, project_root: &Path) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    project_root.hash(&mut h);
    let name = project_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    base.join(format!("{safe}-{:016x}", h.finish()))
}

fn file_name(session_id: &str) -> String {
    let safe: String = session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    format!("{safe}.json")
}

/// Title for the history list: the first user message, shortened.
pub fn title_for(entries: &[RecordEntry]) -> String {
    let first = entries.iter().find_map(|e| match e {
        RecordEntry::User { text, .. } => Some(text.as_str()),
        _ => None,
    });
    match first {
        Some(t) => {
            let line = t.lines().next().unwrap_or_default();
            if line.chars().count() > 80 { format!("{}…", line.chars().take(80).collect::<String>()) } else { line.to_string() }
        }
        None => "Empty session".into(),
    }
}

pub fn save(base: &Path, record: &SessionRecord) -> Result<PathBuf> {
    let dir = project_dir(base, &record.project_root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file_name(&record.session_id));
    // Write atomically so a crash never leaves a truncated file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Newest first. Unreadable files are skipped.
pub fn list(base: &Path, project_root: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(project_dir(base, project_root)) else { return vec![] };
    let mut out: Vec<SessionSummary> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|file| {
            let record = load(&file).ok()?;
            Some(SessionSummary { session_id: record.session_id, agent_id: record.agent_id, title: record.title, updated_at: record.updated_at, file })
        })
        .collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

pub fn load(file: &Path) -> Result<SessionRecord> {
    let bytes = std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid session file {}", file.display()))
}

pub fn delete(file: &Path) -> Result<()> {
    Ok(std::fs::remove_file(file)?)
}

/// "5 min ago", "yesterday", …
pub fn relative_time(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86400 => format!("{} h ago", d / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, root: &Path, updated_at: u64, first: &str) -> SessionRecord {
        let entries = vec![
            RecordEntry::User { text: first.into(), context: vec!["a.rs".into()] },
            RecordEntry::Agent { text: "**ok**".into() },
            RecordEntry::Tool { title: "Run ls".into(), kind: "execute".into(), status: "completed".into() },
        ];
        SessionRecord {
            session_id: id.into(),
            agent_id: "mock".into(),
            agent_name: None,
            project_root: root.into(),
            title: title_for(&entries),
            started_at: updated_at,
            updated_at,
            entries,
        }
    }

    #[test]
    fn save_list_load_delete_round_trip() {
        let base = tempfile::tempdir().unwrap();
        let (p1, p2) = (Path::new("/work/one"), Path::new("/work/two"));
        save(base.path(), &record("s-1", p1, 10, "first")).unwrap();
        let f2 = save(base.path(), &record("s/2", p1, 20, "second")).unwrap();
        save(base.path(), &record("s-3", p2, 30, "other project")).unwrap();

        let listed = list(base.path(), p1);
        assert_eq!(listed.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), vec!["second", "first"]);
        assert_eq!(load(&listed[0].file).unwrap(), record("s/2", p1, 20, "second"));

        // Saving again overwrites the same session file.
        let mut r = record("s-1", p1, 40, "first");
        r.entries.push(RecordEntry::System { text: "more".into() });
        save(base.path(), &r).unwrap();
        assert_eq!(list(base.path(), p1).len(), 2);
        assert_eq!(list(base.path(), p1)[0].session_id, "s-1");

        delete(&f2).unwrap();
        assert_eq!(list(base.path(), p1).len(), 1);
    }

    #[test]
    fn titles_and_times() {
        assert_eq!(title_for(&[]), "Empty session");
        let long = "x".repeat(100);
        assert_eq!(title_for(&[RecordEntry::User { text: format!("{long}\nmore"), context: vec![] }]).chars().count(), 81);
        assert_eq!(relative_time(100, 100), "just now");
        assert_eq!(relative_time(0, 7200), "2 h ago");
        assert_eq!(relative_time(0, 90000), "yesterday");
    }
}
