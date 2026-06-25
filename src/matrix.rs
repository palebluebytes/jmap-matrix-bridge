//! Matrix Client Service homeserver REST API wrapper.
//!
//! [`MatrixClient`] is a thin, purpose-built HTTP client for the Matrix
//! Client-Server API.  It is *not* a full Matrix SDK — it only implements the
//! operations the bridge actually needs.

use anyhow::{Context, Result};
use tracing::info;

use bytes::BytesMut;
use matrix_sdk::ruma::api::{
    IncomingResponse, OutgoingRequest, OutgoingRequestAppserviceExt, auth_scheme::SendAccessToken,
};
use matrix_sdk::ruma::{
    EventId, RoomId, UserId,
    api::client::{
        account::register::v3::Request as RegisterRequest,
        authenticated_media::get_content::v1::Request as DownloadRequest,
        media::create_content::v3::Request as UploadRequest,
        membership::invite_user::v3::{InvitationRecipient, Request as InviteRequest},
        membership::join_room_by_id::v3::Request as JoinRequest,
        membership::leave_room::v3::Request as LeaveRequest,
        message::send_message_event::v3::Request as SendMessageRequest,
        profile::{
            set_avatar_url::v3::Request as SetAvatarRequest,
            set_display_name::v3::Request as SetDisplayNameRequest,
        },
        redact::redact_event::v3::Request as RedactRequest,
        room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
    },
    events::room::message::{MessageType, RoomMessageEventContent},
};

/// True when a ghost send failed because the ghost is not joined to the room
/// (Matrix replies with `M_FORBIDDEN` and "sender's membership `leave` is not
/// `join`"). Such a send succeeds once the ghost (re)joins the room.
fn is_ghost_not_joined(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    s.contains("M_FORBIDDEN") && s.contains("is not `join`")
}

#[derive(Clone, Debug)]
pub struct MatrixClient {
    pub(crate) client: matrix_sdk::Client,
    pub(crate) http_client: reqwest::Client,
    pub(crate) as_token: String,
    pub(crate) homeserver_url: String,
    pub(crate) domain: String,
}

impl MatrixClient {
    pub async fn new(homeserver_url: &str, as_token: &str, domain: &str) -> Result<Self> {
        let client = matrix_sdk::Client::builder()
            .homeserver_url(homeserver_url)
            .build()
            .await?;

        // Restore session for the bot
        let bot_user_id = UserId::parse(format!("@{}:{}", "_jmap_bot", domain))?;
        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: bot_user_id.clone(),
                device_id: matrix_sdk::ruma::device_id!("APP_SERVICE").to_owned(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: as_token.to_owned(),
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await?;

        Ok(Self {
            client,
            http_client: crate::net::client_with_timeouts(),
            as_token: as_token.to_owned(),
            homeserver_url: homeserver_url.trim_end_matches('/').to_owned(),
            domain: domain.to_owned(),
        })
    }

    // ── Identity helpers ──────────────────────────────────────────────

    /// The MXID of the bridge bot (e.g. `@_jmap_bot:example.com`).
    #[must_use]
    pub fn bot_user_id(&self) -> String {
        format!("@_jmap_bot:{}", self.domain)
    }

    /// Build a `/rooms/{room_id}/state/{event_type}/{state_key}` URL with each
    /// path segment percent-encoded (room ids contain `!` and `:`).
    fn state_url(&self, room_id: &str, event_type: &str, state_key: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.homeserver_url)?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("homeserver URL cannot be a base"))?
            .extend(&[
                "_matrix", "client", "v3", "rooms", room_id, "state", event_type, state_key,
            ]);
        Ok(url)
    }

    /// PUT a state event into a room, acting as the bridge bot (the room creator,
    /// so it has the power level to do so).
    ///
    /// Done over raw HTTP rather than `matrix_sdk`'s cached room state because
    /// the appservice runs no `/sync`, so the SDK has no room state.
    async fn put_state(
        &self,
        room_id: &str,
        event_type: &str,
        state_key: &str,
        body: &serde_json::Value,
    ) -> Result<()> {
        let url = self.state_url(room_id, event_type, state_key)?;
        let resp = self
            .http_client
            .put(url)
            .bearer_auth(&self.as_token)
            .json(body)
            .send()
            .await?;
        anyhow::ensure!(
            resp.status().is_success(),
            "PUT {event_type} state failed: {}",
            resp.status()
        );
        Ok(())
    }

    /// Set a room's name. Used by `!compose` to label a freshly-opened
    /// conversation with the user's chosen subject.
    pub async fn set_room_name(&self, room_id: &str, name: &str) -> Result<()> {
        self.put_state(
            room_id,
            "m.room.name",
            "",
            &serde_json::json!({ "name": name }),
        )
        .await
    }

    pub async fn set_room_topic(&self, room_id: &str, topic: &str) -> Result<()> {
        self.put_state(
            room_id,
            "m.room.topic",
            "",
            &serde_json::json!({ "topic": topic }),
        )
        .await
    }

    /// Best-effort read of a room's current name from the state API, or `None`
    /// if unset/unreadable. Used to pick the subject for a fresh outbound email.
    pub async fn room_name(&self, room_id: &str) -> Option<String> {
        let url = self.state_url(room_id, "m.room.name", "").ok()?;
        let resp = self
            .http_client
            .get(url)
            .bearer_auth(&self.as_token)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        json.get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// Create a private Matrix space (a room with `type: m.space`), inviting the
    /// real user. Created as the bot. Used to group all of a user's bridged
    /// email conversation rooms under one parent.
    pub async fn create_space(
        &self,
        name: &str,
        topic: &str,
        invite_user_id: &str,
    ) -> Result<String> {
        info!("Creating email space: {name}");
        let mut request = CreateRoomRequest::new();
        request.name = Some(name.to_owned());
        request.topic = Some(topic.to_owned());
        request.preset = Some(RoomPreset::PrivateChat);
        request.invite = vec![UserId::parse(invite_user_id)?];
        request.is_direct = false;
        request.creation_content = Some(matrix_sdk::ruma::serde::Raw::from_json_string(
            serde_json::json!({ "type": "m.space" }).to_string(),
        )?);

        let bot_id = UserId::parse(self.bot_user_id())?;
        let resp = self.send_as_ghost(request, &bot_id, None).await?;
        Ok(resp.room_id.to_string())
    }

    /// Link `child_room_id` into `space_id` as a child (and back-reference the
    /// space from the child), so the contact room shows up inside the space.
    /// Both state events are written as the bot.
    pub async fn add_room_to_space(&self, space_id: &str, child_room_id: &str) -> Result<()> {
        let via = serde_json::json!([self.domain]);
        self.put_state(
            space_id,
            "m.space.child",
            child_room_id,
            &serde_json::json!({ "via": via, "suggested": true }),
        )
        .await?;
        self.put_state(
            child_room_id,
            "m.space.parent",
            space_id,
            &serde_json::json!({ "via": via, "canonical": true }),
        )
        .await
    }

    /// A unique transaction ID safe for use in Matrix PUT requests.
    ///
    /// Uses a UUID v4 rather than `SystemTime` to avoid the `unwrap()` that
    /// the system-clock approach requires and to be monotonically unique.
    fn txn_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Helper to send a Ruma request as an `AppService` ghost.
    async fn send_as_ghost<R>(
        &self,
        request: R,
        user_id: &UserId,
        timestamp: Option<u64>,
    ) -> Result<R::IncomingResponse>
    where
        R: OutgoingRequest + OutgoingRequestAppserviceExt,
        for<'a> <R as matrix_sdk::ruma::api::Metadata>::Authentication:
            matrix_sdk::ruma::api::auth_scheme::AuthScheme<
                    Input<'a> = matrix_sdk::ruma::api::auth_scheme::SendAccessToken<'a>,
                >,
        for<'a> <R as matrix_sdk::ruma::api::Metadata>::PathBuilder:
            matrix_sdk::ruma::api::path_builder::PathBuilder<
                    Input<'a> = std::borrow::Cow<'a, matrix_sdk::ruma::api::SupportedVersions>,
                >,
    {
        let as_token = SendAccessToken::IfRequired(&self.as_token);
        let identity = matrix_sdk::ruma::api::AppserviceUserIdentity::new(user_id);
        let versions =
            std::collections::BTreeSet::from([matrix_sdk::ruma::api::MatrixVersion::V1_1]);
        let supported = matrix_sdk::ruma::api::SupportedVersions {
            versions,
            features: std::collections::BTreeSet::new(),
        };
        let considering: std::borrow::Cow<'static, matrix_sdk::ruma::api::SupportedVersions> =
            std::borrow::Cow::Owned(supported);
        let http_req = request.try_into_http_request_with_identity::<BytesMut>(
            &self.homeserver_url,
            as_token,
            identity,
            considering,
        )?;

        let (parts, body) = http_req.into_parts();
        let http_req = http::Request::from_parts(parts, reqwest::Body::from(body.freeze()));
        let mut reqwest_req = reqwest::Request::try_from(http_req)?;
        if let Some(ts) = timestamp {
            reqwest_req
                .url_mut()
                .query_pairs_mut()
                .append_pair("ts", &ts.to_string());
        }
        let method = reqwest_req.method().clone();
        let url = reqwest_req.url().clone();
        tracing::debug!("Sending Matrix request as ghost {user_id}: {method} {url}");

        let resp = self.http_client.execute(reqwest_req).await?;

        let status = resp.status();
        tracing::debug!(
            "Received Matrix response as ghost {user_id}: status {status} for {method} {url}"
        );

        let mut http_resp_builder = http::Response::builder().status(status);
        for (k, v) in resp.headers() {
            http_resp_builder = http_resp_builder.header(k, v);
        }

        let body = resp.bytes().await?;
        let body_str = String::from_utf8_lossy(&body);
        if status.is_success() {
            tracing::trace!("Matrix API success response body: {body_str}");
        } else {
            // The "membership `leave` is not `join`" 403 is expected and recovered
            // by the caller (send_as_ghost_joining invites + joins the ghost and
            // retries), e.g. when a multi-party thread's room was created for a
            // different participant's ghost. Log that quietly; surface every other
            // API error loudly. Keep the match in sync with is_ghost_not_joined.
            let recoverable_not_joined =
                status.as_u16() == 403 && body_str.contains("is not `join`");
            if recoverable_not_joined {
                tracing::debug!(
                    "Ghost {user_id} not joined to room ({method} {url}); will invite+join+retry"
                );
            } else {
                tracing::warn!(
                    "Matrix API error response for ghost {user_id} {method} {url}: {status} - body: {body_str}"
                );
            }
            // Surface the status and raw body so callers can react (e.g. join
            // the ghost and retry on a membership 403). Parsing a non-success
            // response with try_from_http_response would otherwise yield an
            // opaque error that hides the errcode.
            anyhow::bail!("Matrix API error [{status}]: {body_str}");
        }

        let http_resp = http_resp_builder.body(body)?;

        R::IncomingResponse::try_from_http_response(http_resp)
            .map_err(|e| anyhow::anyhow!("Matrix API error: {e}"))
    }

    // ── User management ───────────────────────────────────────────────

    /// Register a ghost/bot user if it doesn't exist. Returns `true` if the user
    /// was newly created, `false` if it already existed — callers set the display
    /// name only on creation, to avoid profile-change `m.room.member` churn that
    /// would bump the contact's rooms to "now" and wreck date ordering.
    pub async fn ensure_user_exists(&self, localpart: &str) -> Result<bool> {
        info!("Ensuring Matrix user exists: {localpart}");

        let user_id = UserId::parse(format!("@{}:{}", localpart, self.domain))?;

        let mut request = RegisterRequest::new();
        request.username = Some(localpart.to_owned());
        request.inhibit_login = true;
        request.login_type =
            Some(matrix_sdk::ruma::api::client::account::register::LoginType::ApplicationService);

        let as_token = SendAccessToken::Always(&self.as_token);
        let identity = matrix_sdk::ruma::api::AppserviceUserIdentity::new(&user_id);
        let versions =
            std::collections::BTreeSet::from([matrix_sdk::ruma::api::MatrixVersion::V1_1]);
        let supported = matrix_sdk::ruma::api::SupportedVersions {
            versions,
            features: std::collections::BTreeSet::new(),
        };
        let considering = std::borrow::Cow::Owned(supported);
        let http_req = request.try_into_http_request_with_identity::<BytesMut>(
            &self.homeserver_url,
            as_token,
            identity,
            considering,
        )?;

        let (parts, body) = http_req.into_parts();
        let http_req = http::Request::from_parts(parts, reqwest::Body::from(body.freeze()));
        let reqwest_req = reqwest::Request::try_from(http_req)?;

        // NB: do NOT log the request headers here — they carry the appservice
        // `as_token` as `Authorization: Bearer …` (the bridge's master credential).
        let resp = self.http_client.execute(reqwest_req).await?;

        let status = resp.status();
        tracing::debug!("Registration response status for {localpart}: {status}");

        match status {
            s if s.is_success() => {
                info!("User {localpart} registered successfully");
                Ok(true)
            }
            s if s == reqwest::StatusCode::BAD_REQUEST => {
                let text = resp.text().await.unwrap_or_default();
                if text.contains("M_USER_IN_USE") {
                    info!("User {localpart} already exists, proceeding");
                    Ok(false)
                } else {
                    anyhow::bail!("Failed to register user {localpart}: 400 Bad Request - {text}");
                }
            }
            s => {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Failed to register user {localpart}: {s} - {text}");
            }
        }
    }

    #[allow(deprecated)]
    pub async fn set_display_name(&self, user_id: &str, display_name: &str) -> Result<()> {
        info!("Setting display name for {user_id}: {display_name}");

        let user_id = UserId::parse(user_id)?.clone();
        let request = SetDisplayNameRequest::new(user_id.clone(), Some(display_name.to_owned()));

        self.send_as_ghost(request, &user_id, None).await?;
        info!("Display name set to {display_name}");
        Ok(())
    }

    #[allow(deprecated)]
    /// Upload `avatar_bytes` and set it as `user_id`'s avatar, returning the
    /// resulting `mxc://` URI so callers can persist it and avoid re-uploading
    /// an unchanged image on the next startup.
    pub async fn set_avatar(
        &self,
        user_id: &str,
        avatar_bytes: &[u8],
        mime_type: &str,
    ) -> Result<String> {
        info!("Setting avatar for {user_id}");

        let user_id = UserId::parse(user_id)?.clone();

        // 1. Upload the image
        let mut upload_req = UploadRequest::new(avatar_bytes.to_vec());
        upload_req.content_type = Some(mime_type.to_owned());

        let upload_resp = self.send_as_ghost(upload_req, &user_id, None).await?;
        let mxc_url = upload_resp.content_uri;

        // 2. Set the avatar URL
        let avatar_req = SetAvatarRequest::new(user_id.clone(), Some(mxc_url.clone()));

        self.send_as_ghost(avatar_req, &user_id, None).await?;
        info!("Avatar set to {mxc_url}");
        Ok(mxc_url.to_string())
    }

    // ── Media ──────────────────────────────────────────────────────────

    pub async fn upload_media(
        &self,
        user_id: &str,
        file_bytes: &[u8],
        mime_type: &str,
    ) -> Result<String> {
        let mut request = UploadRequest::new(file_bytes.to_vec());
        request.content_type = Some(mime_type.to_owned());

        let user_id = UserId::parse(user_id)?.clone();
        let resp = self.send_as_ghost(request, &user_id, None).await?;
        Ok(resp.content_uri.to_string())
    }

    pub async fn upload_media_stream<S>(
        &self,
        user_id: &str,
        stream: S,
        mime_type: &str,
        file_name: &str,
    ) -> Result<String>
    where
        S: futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + Sync + 'static,
    {
        let user_id = UserId::parse(user_id)?.clone();
        // Ruma's UploadRequest forces a Vec<u8>, but we can use its metadata
        // and replace the body after conversion to an HTTP request.
        let mut request = UploadRequest::new(vec![]);
        request.content_type = Some(mime_type.to_owned());
        request.filename = Some(file_name.to_owned());

        let as_token = SendAccessToken::IfRequired(&self.as_token);
        let identity = matrix_sdk::ruma::api::AppserviceUserIdentity::new(&user_id);
        let versions =
            std::collections::BTreeSet::from([matrix_sdk::ruma::api::MatrixVersion::V1_1]);
        let supported = matrix_sdk::ruma::api::SupportedVersions {
            versions,
            features: std::collections::BTreeSet::new(),
        };
        let considering = std::borrow::Cow::Owned(supported);
        let http_req = request.try_into_http_request_with_identity::<BytesMut>(
            &self.homeserver_url,
            as_token,
            identity,
            considering,
        )?;

        let (parts, _) = http_req.into_parts();
        let body = reqwest::Body::wrap_stream(stream);
        let http_req = http::Request::from_parts(parts, body);

        let resp = self
            .http_client
            .execute(reqwest::Request::try_from(http_req)?)
            .await?;
        let status = resp.status();

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Matrix media upload failed: {status} - {text}");
        }

        // Reconstruct the response to reuse Ruma's parsing logic
        let mut http_resp_builder = http::Response::builder().status(status);
        for (k, v) in resp.headers() {
            http_resp_builder = http_resp_builder.header(k, v);
        }
        let body_bytes = resp.bytes().await?;
        let http_resp = http_resp_builder.body(body_bytes)?;

        let upload_resp = <UploadRequest as matrix_sdk::ruma::api::OutgoingRequest>::IncomingResponse::try_from_http_response(http_resp)?;
        Ok(upload_resp.content_uri.to_string())
    }

    /// # Errors
    /// Returns an error if the MXC URL is invalid or the download fails.
    #[allow(clippy::missing_errors_doc)]
    pub async fn download_media(&self, mxc_url: &str) -> Result<Vec<u8>> {
        let mxc = <&matrix_sdk::ruma::MxcUri>::try_from(mxc_url).context("Invalid MXC URL")?;
        let (server_name, media_id) = mxc.parts().context("Invalid MXC parts")?;

        let request = DownloadRequest::new(media_id.to_owned(), server_name.to_owned());

        // Download can be done as the bot
        let versions =
            std::collections::BTreeSet::from([matrix_sdk::ruma::api::MatrixVersion::V1_1]);
        let supported = matrix_sdk::ruma::api::SupportedVersions {
            versions,
            features: std::collections::BTreeSet::new(),
        };
        let considering = std::borrow::Cow::Owned(supported);
        let http_req = request.try_into_http_request::<BytesMut>(
            &self.homeserver_url,
            matrix_sdk::ruma::api::auth_scheme::SendAccessToken::IfRequired(&self.as_token),
            considering,
        )?;

        let (parts, body) = http_req.into_parts();
        let http_req = http::Request::from_parts(parts, reqwest::Body::from(body.freeze()));

        let resp = self
            .http_client
            .execute(reqwest::Request::try_from(http_req)?)
            .await?
            .error_for_status()
            .context("Failed to download media")?;

        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn download_media_stream(
        &self,
        mxc_url: &str,
    ) -> Result<(
        impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + 'static,
        String,
        String,
    )> {
        let mxc = <&matrix_sdk::ruma::MxcUri>::try_from(mxc_url).context("Invalid MXC URL")?;
        let (server_name, media_id) = mxc.parts().context("Invalid MXC parts")?;

        let request = DownloadRequest::new(media_id.to_owned(), server_name.to_owned());

        let versions =
            std::collections::BTreeSet::from([matrix_sdk::ruma::api::MatrixVersion::V1_1]);
        let supported = matrix_sdk::ruma::api::SupportedVersions {
            versions,
            features: std::collections::BTreeSet::new(),
        };
        let considering = std::borrow::Cow::Owned(supported);
        let http_req = request.try_into_http_request::<BytesMut>(
            &self.homeserver_url,
            matrix_sdk::ruma::api::auth_scheme::SendAccessToken::IfRequired(&self.as_token),
            considering,
        )?;

        let (parts, body) = http_req.into_parts();
        let http_req = http::Request::from_parts(parts, reqwest::Body::from(body.freeze()));

        let resp = self
            .http_client
            .execute(reqwest::Request::try_from(http_req)?)
            .await?
            .error_for_status()
            .context("Failed to download media")?;

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();

        let content_disposition = resp
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_owned();

        let filename = content_disposition.split("filename=").nth(1).map_or_else(
            || "attachment".to_owned(),
            |s| s.trim_matches('"').to_owned(),
        );

        Ok((resp.bytes_stream(), filename, content_type))
    }

    // ── Rooms ──────────────────────────────────────────────────────────

    pub async fn join_room(&self, room_id: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id)?;
        self.client.join_room_by_id(&room_id).await?;
        Ok(())
    }

    pub async fn join_room_as(&self, room_id: &str, user_id: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id)?;
        let user_id = UserId::parse(user_id)?;

        let request = JoinRequest::new(room_id);

        self.send_as_ghost(request, &user_id, None).await?;
        Ok(())
    }

    /// Make `user_id` (the bot or a ghost) leave `room_id`. Used to tear a room
    /// down after its thread is trashed/junked (#25). Best-effort at the call
    /// site — a ghost that isn't a member just yields a harmless error.
    pub async fn leave_room(&self, room_id: &str, user_id: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id)?;
        let user_id = UserId::parse(user_id)?;

        let request = LeaveRequest::new(room_id);

        self.send_as_ghost(request, &user_id, None).await?;
        Ok(())
    }

    /// Invite `user_id` to `room_id`, acting as the bridge bot (which created
    /// the room and holds invite power). Needed before a ghost can join an
    /// invite-only room it has left: a bare `/join` on a non-public room is
    /// rejected with "cannot join a room that is not `public`".
    pub async fn invite_to_room(&self, room_id: &str, user_id: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id)?;
        let user_id = UserId::parse(user_id)?;

        let request = InviteRequest::new(room_id, InvitationRecipient::UserId { user_id });

        let bot_id = UserId::parse(self.bot_user_id())?;
        self.send_as_ghost(request, &bot_id, None).await?;
        Ok(())
    }

    /// Create a direct-message room and invite `invite_user_id`.
    ///
    /// Both `create_room_for_thread` and `create_room_for_contact` delegate
    /// here; this is the single place that touches the `createRoom` endpoint
    /// for private conversations.
    async fn create_dm_room(
        &self,
        name: &str,
        topic: &str,
        invite_user_ids: &[&str],
    ) -> Result<String> {
        let mut request = CreateRoomRequest::new();
        request.name = Some(name.to_owned());
        request.topic = Some(topic.to_owned());
        request.preset = Some(RoomPreset::PrivateChat);
        request.invite = invite_user_ids
            .iter()
            .map(UserId::parse)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        request.is_direct = true;

        let guest_access_event = matrix_sdk::ruma::serde::Raw::from_json_string(
            serde_json::json!({
                "type": "m.room.guest_access",
                "state_key": "",
                "content": {
                    "guest_access": "forbidden"
                }
            })
            .to_string(),
        )?;
        request.initial_state = vec![guest_access_event];

        let bot_id = UserId::parse(self.bot_user_id())?;
        let resp = self.send_as_ghost(request, &bot_id, None).await?;
        Ok(resp.room_id.to_string())
    }

    pub async fn create_room_for_thread(
        &self,
        thread_subject: &str,
        inviter_user_id: &str,
    ) -> Result<String> {
        info!("Creating room for thread: {thread_subject}");
        self.create_dm_room(
            thread_subject,
            &format!("Email Thread: {thread_subject}"),
            &[inviter_user_id],
        )
        .await
    }

    pub async fn create_room_for_contact(
        &self,
        contact_name: &str,
        contact_email: &str,
        matrix_user_id: &str,
    ) -> Result<String> {
        info!("Creating room for contact: {contact_name}");
        // The room is invite-only (PrivateChat). Invite BOTH the real user and the
        // contact's ghost as part of the create itself (fully committed before this
        // returns), so the ghost's subsequent join is permitted — otherwise the
        // homeserver rejects it with "join rule invite forbids it".
        let ghost_localpart = crate::ghost::email_to_localpart(contact_email);
        let ghost_user_id = format!("@{ghost_localpart}:{}", self.domain);
        self.create_dm_room(
            contact_name,
            &format!("Conversation with {contact_email}"),
            &[matrix_user_id, &ghost_user_id],
        )
        .await
    }

    pub async fn create_room_for_mailbox(&self, mailbox_name: &str) -> Result<String> {
        info!("Creating room for mailbox: {mailbox_name}");

        let mut request = CreateRoomRequest::new();
        request.name = Some(mailbox_name.to_owned());
        request.topic = Some(format!("Mailbox: {mailbox_name}"));
        // Mailbox rooms contain private mail — must NOT be public.
        request.preset = Some(RoomPreset::PrivateChat);
        request.is_direct = false;

        let guest_access_event = matrix_sdk::ruma::serde::Raw::from_json_string(
            serde_json::json!({
                "type": "m.room.guest_access",
                "state_key": "",
                "content": {
                    "guest_access": "forbidden"
                }
            })
            .to_string(),
        )?;
        request.initial_state = vec![guest_access_event];

        let bot_id = UserId::parse(self.bot_user_id())?;
        let resp = self.send_as_ghost(request, &bot_id, None).await?;
        Ok(resp.room_id.to_string())
    }

    // ── Messaging ──────────────────────────────────────────────────────────

    pub async fn send_message(
        &self,
        room_id: &str,
        body_text: &str,
        formatted_body: Option<&str>,
        thread_root_id: Option<&str>,
    ) -> Result<String> {
        self.send_message_as(
            room_id,
            body_text,
            formatted_body,
            thread_root_id,
            None, // no latest_event for bot messages
            &self.bot_user_id(),
            None,
        )
        .await
    }

    pub async fn notify_user(&self, matrix_user_id: &str, message: &str) -> Result<()> {
        let target_user_id = UserId::parse(matrix_user_id).context("Invalid Matrix User ID")?;
        for room in self.client.joined_rooms() {
            let room_id = room.room_id().to_string();
            if let Ok(members) = room.members(matrix_sdk::RoomMemberships::JOIN).await {
                if members.iter().any(|m| m.user_id() == target_user_id)
                    && self
                        .send_message(&room_id, message, None, None)
                        .await
                        .is_ok()
                {
                    return Ok(());
                }
            }
        }
        let room_id = self
            .create_dm_room(
                "JMAP Bridge Bot",
                "Authentication Notification",
                &[matrix_user_id],
            )
            .await?;
        self.send_message(&room_id, message, None, None).await?;
        Ok(())
    }

    /// Send a Matrix message as a ghost user.
    ///
    /// `thread_root_id`: root event of the thread (for `m.thread` relation).
    /// `thread_latest_event_id`: most recently bridged event in the thread,
    /// used as `m.latest_event`.  Pass `None` for non-threaded messages or
    /// when starting a new thread (falls back to root).
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_as(
        &self,
        room_id: &str,
        body_text: &str,
        formatted_body: Option<&str>,
        thread_root_id: Option<&str>,
        thread_latest_event_id: Option<&str>,
        sender_id: &str,
        timestamp: Option<u64>,
    ) -> Result<String> {
        let room_id = RoomId::parse(room_id).context("Invalid Room ID")?;
        let thread_root_id = thread_root_id
            .map(EventId::parse)
            .transpose()
            .context("Invalid Thread Root ID")?;
        let thread_latest_event_id = thread_latest_event_id
            .map(EventId::parse)
            .transpose()
            .context("Invalid Thread Latest Event ID")?;

        let build = || -> Result<SendMessageRequest> {
            let mut content = formatted_body.map_or_else(
                || RoomMessageEventContent::text_plain(body_text),
                |html| RoomMessageEventContent::text_html(body_text, html),
            );
            if let Some(root_id) = thread_root_id.clone() {
                // Use the provided latest event, falling back to the root when
                // this is the first message in the thread.
                let latest_id = thread_latest_event_id
                    .clone()
                    .unwrap_or_else(|| root_id.clone());
                content.relates_to =
                    Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                        matrix_sdk::ruma::events::relation::Thread::plain(root_id, latest_id),
                    ));
            }
            Ok(SendMessageRequest::new(
                room_id.clone(),
                Self::txn_id().into(),
                &matrix_sdk::ruma::events::AnyMessageLikeEventContent::RoomMessage(content),
            )?)
        };

        let sender = UserId::parse(sender_id)?;
        let resp = self
            .send_as_ghost_joining(room_id.as_str(), sender_id, &sender, timestamp, build)
            .await?;
        Ok(resp.event_id.to_string())
    }

    /// React to `target_event_id` with `key` (an emoji) as the bot, returning the
    /// reaction event id so it can later be redacted (the send-state indicator,
    /// #26).
    pub async fn send_reaction(
        &self,
        room_id: &str,
        target_event_id: &str,
        key: &str,
    ) -> Result<String> {
        let room = RoomId::parse(room_id).context("Invalid Room ID")?;
        let target = EventId::parse(target_event_id).context("Invalid target event id")?;
        let bot_id = self.bot_user_id();
        let sender = UserId::parse(&bot_id)?;
        let build = || -> Result<SendMessageRequest> {
            let content = matrix_sdk::ruma::events::reaction::ReactionEventContent::new(
                matrix_sdk::ruma::events::relation::Annotation::new(target.clone(), key.to_owned()),
            );
            Ok(SendMessageRequest::new(
                room.clone(),
                Self::txn_id().into(),
                &matrix_sdk::ruma::events::AnyMessageLikeEventContent::Reaction(content),
            )?)
        };
        let resp = self
            .send_as_ghost_joining(room.as_str(), &bot_id, &sender, None, build)
            .await?;
        Ok(resp.event_id.to_string())
    }

    /// Edit an existing event in place via `m.replace`. The Matrix spec requires
    /// the replacement to be authored by the ORIGINAL event's sender, so
    /// `sender_id` here is the contact ghost that posted the message (not the
    /// human user). Used to re-render a bridged email with its images inlined.
    pub async fn send_edit_as(
        &self,
        room_id: &str,
        target_event_id: &str,
        body_text: &str,
        formatted_body: &str,
        sender_id: &str,
    ) -> Result<String> {
        let room_id = RoomId::parse(room_id).context("Invalid Room ID")?;
        let target = EventId::parse(target_event_id).context("Invalid target Event ID")?;

        let build = || -> Result<SendMessageRequest> {
            let content = RoomMessageEventContent::text_html(body_text, formatted_body)
                .make_replacement(
                    matrix_sdk::ruma::events::room::message::ReplacementMetadata::new(
                        target.clone(),
                        None,
                    ),
                );
            Ok(SendMessageRequest::new(
                room_id.clone(),
                Self::txn_id().into(),
                &matrix_sdk::ruma::events::AnyMessageLikeEventContent::RoomMessage(content),
            )?)
        };

        let sender = UserId::parse(sender_id)?;
        let resp = self
            .send_as_ghost_joining(room_id.as_str(), sender_id, &sender, None, build)
            .await?;
        Ok(resp.event_id.to_string())
    }

    /// Send a ghost request, self-healing the common "ghost not joined to the
    /// room" 403. A ghost is joined to its contact room once at creation, but a
    /// swallowed join error (or a room from an earlier run) can leave it absent,
    /// after which every send fails. On that specific membership error we join
    /// the ghost and retry the send once. `build` reconstructs the request so it
    /// can be issued again (ruma requests are not cloneable).
    async fn send_as_ghost_joining<F>(
        &self,
        room_id: &str,
        sender_id: &str,
        sender: &UserId,
        timestamp: Option<u64>,
        build: F,
    ) -> Result<matrix_sdk::ruma::api::client::message::send_message_event::v3::Response>
    where
        F: Fn() -> Result<SendMessageRequest>,
    {
        match self.send_as_ghost(build()?, sender, timestamp).await {
            Ok(resp) => Ok(resp),
            Err(e) if is_ghost_not_joined(&e) => {
                tracing::info!(
                    "Ghost {sender_id} not joined to room {room_id}; inviting, joining and retrying send"
                );
                // The room is invite-only, so re-invite the ghost (as the bot)
                // before joining — a bare /join on a non-public room it has left
                // is rejected. Invite is best-effort: it errors harmlessly if the
                // ghost is already invited.
                if let Err(invite_err) = self.invite_to_room(room_id, sender_id).await {
                    tracing::debug!(
                        "Invite of {sender_id} to {room_id} before rejoin failed (continuing): {invite_err:#}"
                    );
                }
                self.join_room_as(room_id, sender_id).await?;
                self.send_as_ghost(build()?, sender, timestamp).await
            }
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_file_as(
        &self,
        room_id: &str,
        mxc_url: &str,
        file_name: &str,
        mime_type: &str,
        thread_root_id: Option<&str>,
        thread_latest_event_id: Option<&str>,
        sender_id: &str,
        timestamp: Option<u64>,
    ) -> Result<String> {
        let room_id = RoomId::parse(room_id).context("Invalid Room ID")?;
        let mxc_url = matrix_sdk::ruma::OwnedMxcUri::from(mxc_url.to_owned());
        let thread_root_id = thread_root_id
            .map(EventId::parse)
            .transpose()
            .context("Invalid Thread Root ID")?;
        let thread_latest_event_id = thread_latest_event_id
            .map(EventId::parse)
            .transpose()
            .context("Invalid Thread Latest Event ID")?;

        let build = || -> Result<SendMessageRequest> {
            let mut content = RoomMessageEventContent::new(MessageType::File(
                matrix_sdk::ruma::events::room::message::FileMessageEventContent::plain(
                    file_name.to_owned(),
                    mxc_url.clone(),
                ),
            ));

            if let MessageType::File(ref mut file) = content.msgtype {
                let mut info = matrix_sdk::ruma::events::room::message::FileInfo::new();
                info.mimetype = Some(mime_type.to_owned());
                file.info = Some(Box::new(info));
            }

            if let Some(root_id) = thread_root_id.clone() {
                let latest_id = thread_latest_event_id
                    .clone()
                    .unwrap_or_else(|| root_id.clone());
                content.relates_to =
                    Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                        matrix_sdk::ruma::events::relation::Thread::plain(root_id, latest_id),
                    ));
            }

            Ok(SendMessageRequest::new(
                room_id.clone(),
                Self::txn_id().into(),
                &matrix_sdk::ruma::events::AnyMessageLikeEventContent::RoomMessage(content),
            )?)
        };

        let sender = UserId::parse(sender_id)?;
        let resp = self
            .send_as_ghost_joining(room_id.as_str(), sender_id, &sender, timestamp, build)
            .await?;
        Ok(resp.event_id.to_string())
    }

    pub async fn redact_event(&self, room_id: &str, event_id: &str, reason: &str) -> Result<()> {
        let room_id = RoomId::parse(room_id)?;
        let event_id = EventId::parse(event_id)?;

        let mut request = RedactRequest::new(room_id, event_id, Self::txn_id().into());
        request.reason = Some(reason.to_owned());

        let bot_id = UserId::parse(self.bot_user_id())?;
        self.send_as_ghost(request, &bot_id, None).await?;
        Ok(())
    }
}
