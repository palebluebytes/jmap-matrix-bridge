//! In-memory login state machine for the Matrix ↔ JMAP bridge.
//!
//! [`StateStore`] tracks which step of the multi-step login flow each Matrix
//! user is currently in.  Entries expire automatically after
//! [`LOGIN_STATE_TTL_SECS`] seconds of inactivity.

use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::Mutex;

#[cfg(test)]
const LOGIN_STATE_TTL_SECS: u64 = 1; // 1 second for tests
#[cfg(not(test))]
const LOGIN_STATE_TTL_SECS: u64 = 300; // 5 minutes

// ─── Types ────────────────────────────────────────────────────────────────────

/// The current step in the multi-step JMAP login flow for a single Matrix user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    None,
    WaitingForEmail,
    WaitingForPassword { email: String },
    WaitingForUrl { email: String, password: String },
}

#[derive(Debug)]
struct LoginStateEntry {
    state: LoginState,
    expires_at: Instant,
}

// ─── StateStore ───────────────────────────────────────────────────────────────

/// Thread-safe, TTL-based store for per-user login flow state.
#[derive(Debug)]
pub struct StateStore {
    login_states: Mutex<HashMap<String, LoginStateEntry>>,
}

impl StateStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            login_states: Mutex::new(HashMap::new()),
        }
    }

    /// Return the current login state for `user_id`, evicting expired entries.
    pub async fn get_login_state(&self, user_id: &str) -> LoginState {
        let mut states = self.login_states.lock().await;
        let now = Instant::now();
        match states.get(user_id) {
            Some(entry) if entry.expires_at > now => entry.state.clone(),
            Some(_) => {
                states.remove(user_id);
                LoginState::None
            }
            None => LoginState::None,
        }
    }

    /// Set the login state for `user_id`, resetting the TTL.
    pub async fn set_login_state(&self, user_id: &str, state: LoginState) {
        let expires_at = Instant::now() + std::time::Duration::from_secs(LOGIN_STATE_TTL_SECS);
        self.login_states
            .lock()
            .await
            .insert(user_id.to_owned(), LoginStateEntry { state, expires_at });
    }

    /// Remove the login state for `user_id` (e.g. after a successful login).
    pub async fn clear_login_state(&self, user_id: &str) {
        self.login_states.lock().await.remove(user_id);
    }

    /// Purge all expired entries.  Call periodically to avoid unbounded growth.
    pub async fn cleanup_expired(&self) {
        let now = Instant::now();
        self.login_states
            .lock()
            .await
            .retain(|_, entry| entry.expires_at > now);
    }
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_state_store_flow() {
        let store = StateStore::new();
        let user = "@alice:example.com";

        // Default state
        assert_eq!(store.get_login_state(user).await, LoginState::None);

        // Transition to WaitingForEmail
        store
            .set_login_state(user, LoginState::WaitingForEmail)
            .await;
        assert_eq!(
            store.get_login_state(user).await,
            LoginState::WaitingForEmail
        );

        // Transition to WaitingForPassword
        store
            .set_login_state(
                user,
                LoginState::WaitingForPassword {
                    email: "alice@example.com".to_owned(),
                },
            )
            .await;
        assert_eq!(
            store.get_login_state(user).await,
            LoginState::WaitingForPassword {
                email: "alice@example.com".to_owned()
            }
        );

        // Clear state
        store.clear_login_state(user).await;
        assert_eq!(store.get_login_state(user).await, LoginState::None);
    }

    #[tokio::test]
    async fn test_state_store_expiration_and_cleanup() {
        let store = StateStore::new();
        let user_expired = "@bob:example.com";
        let user_valid = "@charlie:example.com";

        // Set state for bob
        store
            .set_login_state(user_expired, LoginState::WaitingForEmail)
            .await;

        // Wait for expiration (1.1s > 1s TTL)
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Set state for charlie (so it is valid now)
        store
            .set_login_state(user_valid, LoginState::WaitingForEmail)
            .await;

        // Getting bob's state should auto-evict it and return None
        assert_eq!(store.get_login_state(user_expired).await, LoginState::None);

        // Charlie's state should still be valid
        assert_eq!(
            store.get_login_state(user_valid).await,
            LoginState::WaitingForEmail
        );

        // Reset bob's state to expire again
        store
            .set_login_state(user_expired, LoginState::WaitingForEmail)
            .await;

        // Wait for expiration again
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Explicitly clean up expired entries
        store.cleanup_expired().await;

        // Verify bob's entry is gone from the underlying map (gets None)
        assert_eq!(store.get_login_state(user_expired).await, LoginState::None);

        // Charlie's state should have also expired by now if we sleep a bit longer
        tokio::time::sleep(Duration::from_millis(1100)).await;
        store.cleanup_expired().await;
        assert_eq!(store.get_login_state(user_valid).await, LoginState::None);
    }
}
