//! Matrix "double-puppet" auto-join.
//!
//! An appservice can only invite a real Matrix user to a room, never force them
//! to join — so without help the user must manually accept ("Start chatting")
//! every email room the bridge creates. This module logs in as the real user
//! (declaratively via a configured password, or interactively via a token the
//! user provides with `login-matrix`) and runs a lightweight `/sync` loop that
//! auto-accepts invites sent by the bridge bot.

use crate::client_manager::ClientManager;
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

/// The shared-secret-auth login password for `mxid`.
///
/// The lowercase hex of HMAC-SHA512(secret, mxid) over the *full* Matrix id —
/// the matrix-synapse-shared-secret-auth / mautrix convention. The homeserver
/// validates the HMAC and issues a token, so the bridge can mint a double-puppet
/// token without the user pasting one (ADR-0014).
#[must_use]
pub fn shared_secret_password(secret: &str, mxid: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;
    let mut mac =
        Hmac::<Sha512>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(mxid.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Mint a double-puppet access token for `mxid` via shared-secret-auth.
///
/// A password login whose password is [`shared_secret_password`]. Errors
/// (including a homeserver without the module) leave the caller to fall back to
/// manual `login-matrix`.
pub async fn mint_via_shared_secret(homeserver: &str, mxid: &str, secret: &str) -> Result<String> {
    login_password(homeserver, mxid, &shared_secret_password(secret, mxid)).await
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
    /// background task. `client_manager` lets the loop reflect the user's
    /// `m.marked_unread` room state back onto the mail (`$seen`).
    pub async fn ensure_running(
        self: &Arc<Self>,
        mxid: String,
        token: String,
        client_manager: Arc<ClientManager>,
    ) {
        {
            let mut running = self.running.lock().await;
            if !running.insert(mxid.clone()) {
                return; // already puppeting this user
            }
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            info!("Starting Matrix auto-accept puppet for {mxid}");
            run_auto_accept(
                &manager.homeserver,
                &token,
                &mxid,
                &manager.bot_user_id,
                &client_manager,
            )
            .await;
            manager.running.lock().await.remove(&mxid);
            warn!("Matrix auto-accept puppet for {mxid} stopped");
        });
    }
}

/// `/sync` loop that joins every room `mxid` is invited to by `bot_user_id`, and
/// reflects the user's `m.marked_unread` room flags back onto the mail (`$seen`).
/// Returns when the token is rejected (so a revoked/expired token doesn't spin).
async fn run_auto_accept(
    homeserver: &str,
    token: &str,
    mxid: &str,
    bot_user_id: &str,
    client_manager: &ClientManager,
) {
    // Keep timelines tiny (we act on invites + room account data, not messages),
    // but do NOT filter out account_data — that's where m.marked_unread lives.
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

        // Reflect Element "mark as unread" onto the mail before invites, so a
        // sync with no pending invites still processes account-data changes.
        handle_marked_unread(client_manager, mxid, &json).await;

        if let Some(invites) = json["rooms"]["invite"].as_object() {
            for (room_id, data) in invites {
                if invited_by(data, mxid, bot_user_id) {
                    join_room(&http, homeserver, token, room_id, mxid).await;
                }
            }
        }
    }
}

/// Reflect Element "mark as unread" (`m.marked_unread`, MSC2867) onto the mail:
/// when the user flags a bridged thread room unread, clear `$seen` on that
/// thread's latest email so the mailbox shows it unread too. Change-driven — an
/// incremental `/sync` only carries a room's account data when it changes — and
/// gated per room so a restart's initial full sync doesn't re-issue.
async fn handle_marked_unread(cm: &ClientManager, mxid: &str, sync: &serde_json::Value) {
    let Some(joined) = sync["rooms"]["join"].as_object() else {
        return;
    };
    for (room_id, data) in joined {
        let Some(events) = data["account_data"]["events"].as_array() else {
            continue;
        };
        let Some(unread) = parse_marked_unread(events) else {
            continue;
        };
        if let Err(e) = sync_marked_unread(cm, mxid, room_id, unread).await {
            warn!(%room_id, error = %e, "Failed to reflect marked_unread to JMAP");
        }
    }
}

/// The `marked_unread` boolean from a room's `account_data` events, honoring both
/// the stable `m.marked_unread` and the unstable `com.famedly.marked_unread`
/// type names (Element writes the former; some clients still use the latter).
/// `None` when neither is present; a present event with no `unread` field reads
/// as `false`.
fn parse_marked_unread(events: &[serde_json::Value]) -> Option<bool> {
    events.iter().find_map(|ev| {
        matches!(
            ev["type"].as_str(),
            Some("m.marked_unread" | "com.famedly.marked_unread")
        )
        .then(|| ev["content"]["unread"].as_bool().unwrap_or(false))
    })
}

/// The mail-side action implied by a room's `marked_unread` flag versus our stored
/// per-room gate. Pure, so the dedup transition table is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum UnreadAction {
    /// unread just turned on: clear `$seen` on the mail and set the gate.
    MarkUnseen,
    /// unread just turned off: drop the gate (reads are mirrored by receipts).
    ClearGate,
    /// State unchanged since we last acted — do nothing (idempotent across the
    /// restart full-sync, which re-reports every room's current account data).
    NoOp,
}

const fn unread_action(gated: bool, unread: bool) -> UnreadAction {
    match (gated, unread) {
        (false, true) => UnreadAction::MarkUnseen,
        (true, false) => UnreadAction::ClearGate,
        (false, false) | (true, true) => UnreadAction::NoOp,
    }
}

/// Apply one room's `m.marked_unread` state to JMAP, deduped via a per-room gate
/// in `jmap_state`. Only the unread→true edge touches the mail (clears `$seen`
/// on the thread's latest email); the →false edge just clears the gate, since
/// Element also emits a read receipt that the receipt path mirrors as `$seen`.
async fn sync_marked_unread(
    cm: &ClientManager,
    mxid: &str,
    room_id: &str,
    unread: bool,
) -> Result<()> {
    let key = format!("marked_unread:{room_id}");
    let gated = matches!(cm.store.get_jmap_state(mxid, &key).await, Ok(Some(_)));
    match unread_action(gated, unread) {
        UnreadAction::NoOp => Ok(()),
        UnreadAction::ClearGate => {
            cm.store.delete_jmap_state(mxid, &key).await?;
            Ok(())
        }
        UnreadAction::MarkUnseen => {
            let Some(email_id) = cm.store.get_latest_email_id_by_room(room_id).await? else {
                return Ok(()); // not a bridged thread room
            };
            let Some(client) = cm.get_client(mxid).await else {
                return Ok(()); // no live JMAP session for this user right now
            };
            crate::sender::JmapSender::new(client)
                .mark_as_unread(&email_id)
                .await?;
            cm.store.save_jmap_state(mxid, &key, "1").await?;
            info!(%room_id, %email_id, "Reflected Element mark-unread to JMAP ($seen cleared)");
            Ok(())
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

/// Join `room_id` as the user owning `token`, erroring on any non-2xx.
///
/// The fire-and-forget [`join_room`] above serves the async `/sync` auto-accept
/// loop; this variant is for callers that must confirm the real user is a member
/// *synchronously* — specifically, before the bridge posts the first email into a
/// freshly created room. If the join is left to the async loop, the message lands
/// while the user is merely invited: Matrix then treats it as pre-join history
/// (never counted as unread) and the later join becomes the newest event in the
/// room, so "… joined the room" shows as the last message. Joining first makes
/// the email both the latest event and unread.
pub async fn join_room_via_token(homeserver: &str, token: &str, room_id: &str) -> Result<()> {
    let url = format!("{homeserver}/_matrix/client/v3/rooms/{room_id}/join");
    let resp = crate::net::client_with_timeouts()
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "join of {room_id} failed ({status})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{UnreadAction, invited_by, localpart, parse_marked_unread, unread_action};

    #[test]
    fn parses_marked_unread_from_either_type_name() {
        let stable = [serde_json::json!({
            "type": "m.marked_unread", "content": { "unread": true }
        })];
        assert_eq!(parse_marked_unread(&stable), Some(true));

        let unstable = [serde_json::json!({
            "type": "com.famedly.marked_unread", "content": { "unread": false }
        })];
        assert_eq!(parse_marked_unread(&unstable), Some(false));

        // A present event with no `unread` field reads as false (not unread).
        let no_field = [serde_json::json!({ "type": "m.marked_unread", "content": {} })];
        assert_eq!(parse_marked_unread(&no_field), Some(false));

        // Unrelated account data -> None (nothing to reflect).
        let other =
            [serde_json::json!({ "type": "m.fully_read", "content": { "event_id": "$x" } })];
        assert_eq!(parse_marked_unread(&other), None);
        assert_eq!(parse_marked_unread(&[]), None);
    }

    #[test]
    fn unread_action_only_acts_on_transitions() {
        // ungated + unread -> mark the mail unseen (and gate it).
        assert_eq!(unread_action(false, true), UnreadAction::MarkUnseen);
        // gated + not-unread -> drop the gate (read is mirrored by receipts).
        assert_eq!(unread_action(true, false), UnreadAction::ClearGate);
        // Steady states -> nothing (so a restart's full sync doesn't re-issue).
        assert_eq!(unread_action(true, true), UnreadAction::NoOp);
        assert_eq!(unread_action(false, false), UnreadAction::NoOp);
    }

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
