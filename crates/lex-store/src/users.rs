//! `<store>/users.json` — actor identity (lex-tea v3d, #172).
//!
//! Tightens the v3a–v3c `LEX_TEA_USER` env var auth: the env var
//! (and the `--actor` flag) still nominate who took an action,
//! but when `users.json` exists the nominated name must be in
//! the file. Anonymous overrides aren't a regression we want.
//! When the file is absent the surfaces fall back to the v3a–v3c
//! "anyone with LEX_TEA_USER" behaviour so existing dev setups
//! keep working.
//!
//! File schema (deliberately minimal):
//!
//! ```json
//! {
//!   "users": [
//!     {"name": "alice", "role": "human"},
//!     {"name": "lexbot", "role": "agent"}
//!   ]
//! }
//! ```
//!
//! `role` is recorded but not enforced in v3d — it gives later
//! slices a place to gate "only humans can pin" or similar
//! without a schema migration.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Human,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsersFile {
    #[serde(default)]
    pub users: Vec<User>,
}

/// Load `<root>/users.json`. Returns `Ok(None)` when the file is
/// absent (the v3a–v3c "no auth wired up, use env-var fallback"
/// regime); `Ok(Some(_))` when present, even if the user list is
/// empty (an empty file is "auth wired up, nobody allowed").
pub fn load(root: &Path) -> io::Result<Option<UsersFile>> {
    let path = root.join("users.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let file: UsersFile = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("parsing {}: {e}", path.display())))?;
    Ok(Some(file))
}

impl UsersFile {
    /// Look up a user by name. Names are case-sensitive — the file
    /// is the spec.
    pub fn find(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|u| u.name == name)
    }

    /// Whether this name is recognized. Convenience over `find` for
    /// callers that just want a yes/no gate.
    pub fn knows(&self, name: &str) -> bool {
        self.find(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_absent_returns_none() {
        let tmp = tempdir().unwrap();
        let got = load(tmp.path()).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn load_empty_file_returns_some_empty() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("users.json"), r#"{"users":[]}"#).unwrap();
        let got = load(tmp.path()).unwrap().unwrap();
        assert_eq!(got.users.len(), 0);
        assert!(!got.knows("alice"));
    }

    #[test]
    fn round_trip_through_disk() {
        let tmp = tempdir().unwrap();
        let f = UsersFile {
            users: vec![
                User { name: "alice".into(), role: UserRole::Human },
                User { name: "lexbot".into(), role: UserRole::Agent },
            ],
        };
        std::fs::write(
            tmp.path().join("users.json"),
            serde_json::to_vec_pretty(&f).unwrap(),
        ).unwrap();
        let got = load(tmp.path()).unwrap().unwrap();
        assert_eq!(got, f);
        assert_eq!(got.find("alice").unwrap().role, UserRole::Human);
        assert_eq!(got.find("lexbot").unwrap().role, UserRole::Agent);
        assert!(!got.knows("eve"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("users.json"), "not json").unwrap();
        let err = load(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
