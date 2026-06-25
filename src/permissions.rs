//! Who may use the bridge.
//!
//! A default-deny permission map ([ADR-0010](../docs/adr/0010-permission-model.md))
//! keyed — most- to least-specific — by full MXID (`@you:example.com`), homeserver
//! domain (`example.com`), or `*` (everyone), each granting a [`Level`]. A sender
//! matching no entry is denied (cannot even `login`). When the map is empty the
//! bridge synthesizes one entry — its own domain at [`Level::User`] — so existing
//! single-homeserver installs keep working while foreign federated senders are
//! refused.

use std::collections::HashMap;

use thiserror::Error;

/// What a [permitted](Permissions) sender is allowed to do. Ordered: `Admin`
/// outranks `User`, so `sender_level >= command.min_level()` is the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// May `login`, operate their own JMAP account, and run non-destructive
    /// commands.
    User,
    /// Everything a `User` can do, plus destructive/global commands.
    Admin,
}

/// Error parsing a permission spec or level string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PermissionError {
    /// A `--permission` spec was missing its `key=level` separator.
    #[error("permission spec `{0}` must be of the form `key=level`")]
    MalformedSpec(String),
    /// The level token was neither `user` nor `admin`.
    #[error("unknown permission level `{0}` (expected `user` or `admin`)")]
    UnknownLevel(String),
    /// The key (MXID / domain / `*`) was empty.
    #[error("permission spec `{0}` has an empty key")]
    EmptyKey(String),
}

impl std::str::FromStr for Level {
    type Err = PermissionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "admin" => Ok(Self::Admin),
            other => Err(PermissionError::UnknownLevel(other.to_owned())),
        }
    }
}

/// The resolved access-control map. Cheap to clone? No — wrap in an `Arc` where
/// shared (see `routes::AppState`).
#[derive(Debug, Clone)]
pub struct Permissions {
    /// Keys are exact MXIDs, bare domains, or the literal `*`.
    map: HashMap<String, Level>,
}

impl Permissions {
    /// Build from `--permission`/`PERMISSION` specs (`key=level`). When `specs`
    /// is empty, default to granting [`Level::User`] to the bridge's own
    /// `bridge_domain` and nobody else — the backward-compatible default-deny.
    ///
    /// # Errors
    /// Returns a [`PermissionError`] if any spec is malformed or names an unknown
    /// level.
    pub fn from_specs(specs: &[String], bridge_domain: &str) -> Result<Self, PermissionError> {
        let mut map = HashMap::new();
        for spec in specs {
            let (key, level) = spec
                .split_once('=')
                .ok_or_else(|| PermissionError::MalformedSpec(spec.clone()))?;
            let key = key.trim();
            if key.is_empty() {
                return Err(PermissionError::EmptyKey(spec.clone()));
            }
            map.insert(key.to_owned(), level.parse()?);
        }
        if map.is_empty() {
            map.insert(bridge_domain.to_owned(), Level::User);
        }
        Ok(Self { map })
    }

    /// A permissive map granting [`Level::Admin`] to everyone (`*`). For tests and
    /// fixtures only — never the production default.
    #[must_use]
    pub fn allow_all() -> Self {
        let mut map = HashMap::new();
        map.insert("*".to_owned(), Level::Admin);
        Self { map }
    }

    /// Resolve a sender's level, most-specific match first: exact MXID, then the
    /// MXID's homeserver domain, then `*`. `None` means denied.
    #[must_use]
    pub fn level_for(&self, mxid: &str) -> Option<Level> {
        if let Some(level) = self.map.get(mxid) {
            return Some(*level);
        }
        if let Some((_, domain)) = mxid.split_once(':')
            && let Some(level) = self.map.get(domain)
        {
            return Some(*level);
        }
        self.map.get("*").copied()
    }

    /// Whether `mxid` may run a command requiring at least `required`.
    #[must_use]
    pub fn permits(&self, mxid: &str, required: Level) -> bool {
        self.level_for(mxid).is_some_and(|have| have >= required)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_specs_default_deny_with_local_domain_user() {
        let perms = Permissions::from_specs(&[], "example.com").unwrap();
        assert_eq!(perms.level_for("@alice:example.com"), Some(Level::User));
        // A foreign homeserver is denied outright.
        assert_eq!(perms.level_for("@mallory:evil.com"), None);
        // Local domain is not admin by default.
        assert!(!perms.permits("@alice:example.com", Level::Admin));
    }

    #[test]
    fn explicit_mxid_grant_overrides_domain() {
        let perms = Permissions::from_specs(
            &[
                "example.com=user".to_owned(),
                "@you:example.com=admin".to_owned(),
            ],
            "example.com",
        )
        .unwrap();
        // Most-specific wins: the explicit MXID is admin...
        assert_eq!(perms.level_for("@you:example.com"), Some(Level::Admin));
        // ...while another local user falls back to the domain grant.
        assert_eq!(perms.level_for("@her:example.com"), Some(Level::User));
    }

    #[test]
    fn wildcard_grants_everyone() {
        let perms = Permissions::from_specs(&["*=user".to_owned()], "example.com").unwrap();
        assert_eq!(perms.level_for("@anyone:anywhere.net"), Some(Level::User));
        assert!(!perms.permits("@anyone:anywhere.net", Level::Admin));
    }

    #[test]
    fn level_ordering_admin_outranks_user() {
        assert!(Level::Admin > Level::User);
        let perms = Permissions::from_specs(&["@boss:example.com=admin".to_owned()], "example.com")
            .unwrap();
        assert!(perms.permits("@boss:example.com", Level::User));
        assert!(perms.permits("@boss:example.com", Level::Admin));
    }

    #[test]
    fn allow_all_grants_admin_to_everyone() {
        let perms = Permissions::allow_all();
        assert_eq!(perms.level_for("@anyone:anywhere.net"), Some(Level::Admin));
    }

    #[test]
    fn malformed_specs_are_rejected() {
        assert_eq!(
            Permissions::from_specs(&["nope".to_owned()], "example.com").unwrap_err(),
            PermissionError::MalformedSpec("nope".to_owned())
        );
        assert_eq!(
            Permissions::from_specs(&["example.com=root".to_owned()], "example.com").unwrap_err(),
            PermissionError::UnknownLevel("root".to_owned())
        );
        assert_eq!(
            Permissions::from_specs(&["=user".to_owned()], "example.com").unwrap_err(),
            PermissionError::EmptyKey("=user".to_owned())
        );
    }
}
