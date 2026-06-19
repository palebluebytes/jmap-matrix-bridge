//! Matrix "double-puppet" auto-join.
//!
//! An appservice can only invite a real Matrix user to a room, never force them
//! to join — so without help the user must manually accept ("Start chatting")
//! every email room the bridge creates. This module logs in as the real user
//! (declaratively via a configured password, or interactively via a token the
//! user provides with `login-matrix`) and runs a lightweight `/sync` loop that
//! auto-accepts invites sent by the bridge bot.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// State key under which a user's double-puppet access token is stored
/// (in the generic `jmap_state` kv table, so no schema migration is needed).
pub const PUPPET_TOKEN_KEY: &str = "matrix_puppet_token";

/// Extract the localpart from a full Matrix id (`@name:server` -> `name`).
fn localpart(mxid: &str) -> &str {
    mxid.trim_start_matches('@')
        .split_once(':')
        .map_or(mxid, |(lp, _)| lp)
}

/// Password-login as `mxid` and return a fresh access token.
pub async fn login_password(homeserver: &str, mxid: &str, password: &str) -> Result<String> {
    let body = serde_json::json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": localpart(mxid) },
        "password": password,
        // Reuse a fixed device each startup so repeated logins don't pile up
        // devices in the user's account.
        "device_id": "JMAP_BRIDGE_PUPPET",
        "initial_device_display_name": "JMAP Bridge (double puppet)"
    });
    let resp = crate::net::client_with_timeouts()
        .post(format!("{homeserver}/_matrix/client/v3/login"))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await?;
    anyhow::ensure!(status.is_success(), "login failed ({status}): {json}");
    json["access_token"]
        .as_str()
        .map(str::to_owned)
        .context("login response missing access_token")
}

/// Validate `token` and return the Matrix id it belongs to.
pub async fn whoami(homeserver: &str, token: &str) -> Result<String> {
    let resp = crate::net::client_with_timeouts()
        .get(format!("{homeserver}/_matrix/client/v3/account/whoami"))
        .bearer_auth(token)
        .send()
        .await?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await?;
    anyhow::ensure!(status.is_success(), "whoami failed ({status}): {json}");
    json["user_id"]
        .as_str()
        .map(str::to_owned)
        .context("whoami response missing user_id")
}

/// Tracks the per-user auto-accept tasks so each user is puppeted at most once.
#[derive(Debug)]
pub struct PuppetManager {
    homeserver: String,
    bot_user_id: String,
    running: Mutex<HashSet<String>>,
}

impl PuppetManager {
    #[must_use]
    pub fn new(homeserver: String, bot_user_id: String) -> Self {
        Self {
            homeserver,
            bot_user_id,
            running: Mutex::new(HashSet::new()),
        }
    }

    /// Start the auto-accept loop for `mxid` using `token`, unless one is
    /// already running for that user. Returns immediately; the loop runs in a
    /// background task.
    pub async fn ensure_running(self: &Arc<Self>, mxid: String, token: String) {
        {
            let mut running = self.running.lock().await;
            if !running.insert(mxid.clone()) {
                return; // already puppeting this user
            }
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            info!("Starting Matrix auto-accept puppet for {mxid}");
            run_auto_accept(&manager.homeserver, &token, &mxid, &manager.bot_user_id).await;
            manager.running.lock().await.remove(&mxid);
            warn!("Matrix auto-accept puppet for {mxid} stopped");
        });
    }
}

/// `/sync` loop that joins every room `mxid` is invited to by `bot_user_id`.
/// Returns when the token is rejected (so a revoked/expired token doesn't spin).
async fn run_auto_accept(homeserver: &str, token: &str, mxid: &str, bot_user_id: &str) {
    // Minimal filter: we only care about invites, so keep joined-room payloads
    // tiny to bound the initial full sync.
    let filter = r#"{"room":{"timeline":{"limit":1}},"presence":{"types":[]}}"#;
    let http = crate::net::client_with_timeouts(); // 120 s timeout accommodates the 30 s long-poll
    let mut since: Option<String> = None;

    loop {
        let mut query: Vec<(&str, &str)> = vec![("timeout", "30000"), ("filter", filter)];
        if let Some(s) = &since {
            query.push(("since", s));
        }
        let resp = match http
            .get(format!("{homeserver}/_matrix/client/v3/sync"))
            .query(&query)
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("auto-accept sync request failed for {mxid}: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            error!(
                "auto-accept token for {mxid} was rejected (401); stopping. Re-run `login-matrix`."
            );
            return;
        }
        if !resp.status().is_success() {
            warn!("auto-accept sync for {mxid} returned {}", resp.status());
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            continue;
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                warn!("auto-accept sync body parse failed for {mxid}: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                continue;
            }
        };
        since = json["next_batch"].as_str().map(str::to_owned);

        let Some(invites) = json["rooms"]["invite"].as_object() else {
            continue;
        };
        for (room_id, data) in invites {
            if invited_by(data, mxid, bot_user_id) {
                join_room(&http, homeserver, token, room_id, mxid).await;
            }
        }
    }
}

/// True if the invite stripped state contains an `m.room.member` invite for
/// `mxid` whose sender is `bot_user_id`.
fn invited_by(invite_room: &serde_json::Value, mxid: &str, bot_user_id: &str) -> bool {
    invite_room["invite_state"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|ev| {
            ev["type"] == "m.room.member"
                && ev["state_key"] == mxid
                && ev["content"]["membership"] == "invite"
                && ev["sender"] == bot_user_id
        })
}

async fn join_room(
    http: &reqwest::Client,
    homeserver: &str,
    token: &str,
    room_id: &str,
    mxid: &str,
) {
    let url = format!("{homeserver}/_matrix/client/v3/rooms/{room_id}/join");
    match http
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => info!("Auto-joined {room_id} as {mxid}"),
        Ok(r) => warn!("Auto-join of {room_id} as {mxid} failed: {}", r.status()),
        Err(e) => warn!("Auto-join of {room_id} as {mxid} errored: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{invited_by, localpart};

    fn invite_room(events: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "invite_state": { "events": events } })
    }

    #[test]
    fn localpart_extracts_name() {
        assert_eq!(
            localpart("@inkpotmonkey:matrix.example.com"),
            "inkpotmonkey"
        );
        assert_eq!(localpart("inkpotmonkey"), "inkpotmonkey");
    }

    #[test]
    fn accepts_invite_from_the_bot() {
        let room = invite_room(&serde_json::json!([
            {
                "type": "m.room.member",
                "state_key": "@me:localhost",
                "sender": "@_jmap_bot:localhost",
                "content": { "membership": "invite" }
            }
        ]));
        assert!(invited_by(&room, "@me:localhost", "@_jmap_bot:localhost"));
    }

    #[test]
    fn rejects_invite_from_a_non_bot() {
        // A stranger inviting us must NOT trigger auto-join — the puppet is
        // scoped to the bridge's own invites only.
        let room = invite_room(&serde_json::json!([
            {
                "type": "m.room.member",
                "state_key": "@me:localhost",
                "sender": "@stranger:localhost",
                "content": { "membership": "invite" }
            }
        ]));
        assert!(!invited_by(&room, "@me:localhost", "@_jmap_bot:localhost"));
    }

    #[test]
    fn rejects_invite_for_a_different_user() {
        let room = invite_room(&serde_json::json!([
            {
                "type": "m.room.member",
                "state_key": "@someone_else:localhost",
                "sender": "@_jmap_bot:localhost",
                "content": { "membership": "invite" }
            }
        ]));
        assert!(!invited_by(&room, "@me:localhost", "@_jmap_bot:localhost"));
    }

    #[test]
    fn rejects_non_invite_membership() {
        let room = invite_room(&serde_json::json!([
            {
                "type": "m.room.member",
                "state_key": "@me:localhost",
                "sender": "@_jmap_bot:localhost",
                "content": { "membership": "join" }
            }
        ]));
        assert!(!invited_by(&room, "@me:localhost", "@_jmap_bot:localhost"));
    }
}
