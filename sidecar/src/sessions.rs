use crate::protocol::ChatMessage;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSession {
    pub session_id: String,
    pub title: String,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub updated_at: u64,
    pub message_count: usize,
}

pub struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    pub fn default_store() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        Self {
            base_dir: PathBuf::from(home).join(".rursor").join("sessions"),
        }
    }

    #[cfg(test)]
    pub fn with_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn workspace_dir(&self, workspace_root: Option<&Path>) -> PathBuf {
        let key = match workspace_root {
            Some(p) => {
                let mut hasher = DefaultHasher::new();
                p.to_string_lossy().hash(&mut hasher);
                format!("ws_{:016x}", hasher.finish())
            }
            None => "global".to_string(),
        };
        self.base_dir.join(key)
    }

    fn session_file(&self, workspace_root: Option<&Path>, session_id: &str) -> Result<PathBuf, String> {
        let clean_id: String = session_id
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if clean_id.is_empty() {
            return Err("Invalid session ID.".into());
        }
        Ok(self.workspace_dir(workspace_root).join(format!("{clean_id}.json")))
    }

    pub fn save(
        &self,
        workspace_root: Option<&Path>,
        session_id: &str,
        title: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<SessionSummary, String> {
        let file_path = self.session_file(workspace_root, session_id)?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create session dir: {e}"))?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let stored = StoredSession {
            session_id: session_id.to_string(),
            title: title.to_string(),
            updated_at: now,
            messages,
        };
        let message_count = stored.messages.len();
        let json = serde_json::to_string_pretty(&stored)
            .map_err(|e| format!("Failed to serialize session: {e}"))?;
        fs::write(&file_path, json).map_err(|e| format!("Failed to write session: {e}"))?;

        Ok(SessionSummary {
            session_id: stored.session_id,
            title: stored.title,
            updated_at: stored.updated_at,
            message_count,
        })
    }

    pub fn load(&self, workspace_root: Option<&Path>, session_id: &str) -> Result<StoredSession, String> {
        let file_path = self.session_file(workspace_root, session_id)?;
        let raw = fs::read_to_string(&file_path)
            .map_err(|e| format!("Failed to read session '{session_id}': {e}"))?;
        serde_json::from_str(&raw).map_err(|e| format!("Corrupt session JSON: {e}"))
    }

    pub fn delete(&self, workspace_root: Option<&Path>, session_id: &str) -> Result<bool, String> {
        let file_path = self.session_file(workspace_root, session_id)?;
        if !file_path.exists() {
            return Ok(false);
        }
        fs::remove_file(&file_path).map_err(|e| format!("Failed to delete session: {e}"))?;
        Ok(true)
    }

    pub fn list(&self, workspace_root: Option<&Path>) -> Vec<SessionSummary> {
        let dir = self.workspace_dir(workspace_root);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&path) {
                if let Ok(sess) = serde_json::from_str::<StoredSession>(&raw) {
                    items.push(SessionSummary {
                        session_id: sess.session_id,
                        title: sess.title,
                        updated_at: sess.updated_at,
                        message_count: sess.messages.len(),
                    });
                }
            }
        }
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items.truncate(30);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use crate::protocol::ChatMessage;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn saves_lists_loads_and_deletes_session() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("rursor_sess_{nanos}"));
        let store = SessionStore::with_dir(dir.clone());
        let ws = PathBuf::from("/tmp/demo-ws");

        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "hello".into(),
            images: vec![],
        }];
        store.save(Some(&ws), "sess_1", "First chat", msgs).unwrap();

        let list = store.list(Some(&ws));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "First chat");

        let loaded = store.load(Some(&ws), "sess_1").unwrap();
        assert_eq!(loaded.messages.len(), 1);

        assert!(store.delete(Some(&ws), "sess_1").unwrap());
        assert!(store.list(Some(&ws)).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
