use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use subtle::ConstantTimeEq;

#[derive(Deserialize)]
struct UserEntry {
    token_hash: String,
    user_id: String,
    #[serde(default)]
    vip: bool,
    /// Allows the model-management endpoints (pull, delete, create, ...).
    #[serde(default)]
    admin: bool,
}

#[derive(Deserialize)]
struct UsersFile {
    users: Vec<UserEntry>,
}

/// Registry of authenticated users loaded from `users.yaml`.
///
/// Tokens are stored as their SHA-256 hex digest — plaintext tokens are
/// never persisted on disk.
#[derive(Clone)]
pub struct UserRegistry {
    /// sha256_hex -> (user_id, is_vip)
    map: Vec<(String, String, bool)>,
    /// user_ids allowed to call model-management endpoints
    admins: HashSet<String>,
}

impl UserRegistry {
    /// Load the registry from the given YAML file path.
    ///
    /// Returns an empty registry (not an error) when the file does not exist,
    /// which causes every request to be rejected with 401. A warning is logged
    /// by the caller.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let parsed: UsersFile = serde_yaml::from_str(&content)?;
        let admins = parsed
            .users
            .iter()
            .filter(|e| e.admin)
            .map(|e| e.user_id.clone())
            .collect();
        let map = parsed
            .users
            .into_iter()
            .map(|e| (e.token_hash.to_lowercase(), e.user_id, e.vip))
            .collect();
        Ok(Self { map, admins })
    }

    /// Return an empty registry (all requests will be rejected).
    pub fn empty() -> Self {
        Self {
            map: vec![],
            admins: HashSet::new(),
        }
    }

    /// Whether `user_id` may call model-management endpoints.
    pub fn is_admin(&self, user_id: &str) -> bool {
        self.admins.contains(user_id)
    }

    /// Hash `raw_token` with SHA-256 and look it up in the registry using
    /// constant-time comparison to prevent timing attacks.
    ///
    /// Returns `Some(user_id)` if the token is known, `None` otherwise.
    pub fn authenticate(&self, raw_token: &str) -> Option<&str> {
        let digest = Sha256::digest(raw_token.as_bytes());
        let hash: String = digest.iter().map(|b| format!("{:02x}", b)).collect();

        // We must check every entry regardless of whether a match occurs early,
        // so we use constant-time comparison and accumulate the result.
        let mut matched_user: Option<&str> = None;
        let mut matches = 0i32;

        for (stored_hash, user_id, _vip) in &self.map {
            if stored_hash.len() == hash.len()
                && hash.as_bytes().ct_eq(stored_hash.as_bytes()).into()
            {
                matches += 1;
                // We can't break here — that would reintroduce the timing
                // side-channel. The loop continues to scan the entire registry.
                matched_user = Some(user_id.as_str());
            }
        }

        if matches == 1 { matched_user } else { None }
    }

    /// Return a list of all VIP user IDs.
    pub fn get_vip_users(&self) -> Vec<String> {
        self.map
            .iter()
            .filter(|(_, _, is_vip)| *is_vip)
            .map(|(_, user_id, _)| user_id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry(users: Vec<(&str, &str, bool)>) -> UserRegistry {
        UserRegistry {
            map: users
                .into_iter()
                .map(|(h, u, v)| (h.to_lowercase(), u.to_string(), v))
                .collect(),
            admins: HashSet::new(),
        }
    }

    fn sha256_hex(s: &str) -> String {
        let digest = Sha256::digest(s.as_bytes());
        digest.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn known_token_matches() {
        let registry = make_registry(vec![(sha256_hex("").as_str(), "alice", false)]);
        let result = registry.authenticate("");
        assert_eq!(result, Some("alice"));
    }

    #[test]
    fn unknown_token_rejected() {
        let registry = make_registry(vec![(sha256_hex("").as_str(), "alice", false)]);
        let result = registry.authenticate("wrong-token");
        assert!(result.is_none());
    }

    #[test]
    fn case_insensitive_hash() {
        let registry = make_registry(vec![(
            sha256_hex("").to_uppercase().as_str(),
            "alice",
            false,
        )]);
        let result = registry.authenticate("");
        assert_eq!(result, Some("alice"));
    }

    #[test]
    fn empty_registry_rejects_all() {
        let registry = UserRegistry::empty();
        let result = registry.authenticate("any-token");
        assert!(result.is_none());
    }

    #[test]
    fn empty_list_rejects_all() {
        let registry = make_registry(vec![]);
        let result = registry.authenticate("any-token");
        assert!(result.is_none());
    }

    #[test]
    fn multiple_users_only_one_match() {
        let registry = make_registry(vec![
            (sha256_hex("alice-token").as_str(), "alice", false),
            (sha256_hex("bob-token").as_str(), "bob", false),
            (sha256_hex("charlie-token").as_str(), "charlie", true),
        ]);
        let result = registry.authenticate("bob-token");
        assert_eq!(result, Some("bob"));
    }

    #[test]
    fn vip_users_returned() {
        let registry = make_registry(vec![
            (sha256_hex("alice-token").as_str(), "alice", false),
            (sha256_hex("bob-token").as_str(), "bob", true),
            (sha256_hex("charlie-token").as_str(), "charlie", true),
        ]);
        let vip = registry.get_vip_users();
        assert_eq!(vip.len(), 2);
        assert!(vip.contains(&"bob".to_string()));
        assert!(vip.contains(&"charlie".to_string()));
    }

    #[test]
    fn admin_flag_defaults_to_false() {
        let path = std::env::temp_dir().join(format!("alp-users-{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            "users:\n  - token_hash: aa\n    user_id: alice\n  - token_hash: bb\n    user_id: root\n    admin: true\n",
        )
        .unwrap();
        let registry = UserRegistry::load(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!registry.is_admin("alice"));
        assert!(registry.is_admin("root"));
        assert!(!registry.is_admin("nobody"));
    }

    #[test]
    fn no_users_vip_returns_empty() {
        let registry = make_registry(vec![]);
        let vip = registry.get_vip_users();
        assert!(vip.is_empty());
    }
}
