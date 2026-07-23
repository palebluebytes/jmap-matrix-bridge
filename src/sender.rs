//! JMAP email sending.
//!
//! [`JmapSender`] wraps the JMAP client and provides a clean API for composing
//! and submitting emails.  Both fresh emails and replies are built through the
//! same internal [`EmailBuilder`], eliminating code duplication.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use std::sync::Arc;

use jmap_client::Method;
use jmap_client::blob::upload::UploadResponse;
use jmap_client::client::Client;
use jmap_client::core::request::Arguments;
use jmap_client::email::{EmailBodyPart, EmailBodyValue, Property};
use jmap_client::mailbox::Role as MailboxRole;
use jmap_client::mailbox::query::Filter as MailboxFilter;

// ─── Public surface ───────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct AttachmentInfo {
    pub blob_id: String,
    pub name: String,
    pub mime_type: String,
}

/// Thin wrapper around [`Client`] that exposes high-level email operations.
#[derive(Clone)]
pub struct JmapSender {
    pub(crate) client: Arc<Client>,
    /// When true, threaded replies carry a standard quoted-original of the
    /// parent message. The quote lives ONLY in the outbound email — it is never
    /// written to Matrix (the inbound path strips quotes for display).
    quote_replies: bool,
}

impl std::fmt::Debug for JmapSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapSender").finish_non_exhaustive()
    }
}

impl JmapSender {
    pub const fn new(client: Arc<Client>) -> Self {
        Self {
            client,
            quote_replies: false,
        }
    }

    /// Enable quoting the parent message in outbound replies (off by default).
    /// Mirrors the const-builder pattern on [`crate::client_manager::ClientManager`].
    #[must_use]
    pub const fn with_quote_replies(mut self, enabled: bool) -> Self {
        self.quote_replies = enabled;
        self
    }

    /// Send a fresh email to `to`.  Returns the JMAP email ID on success.
    pub async fn send_email(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        attachments: Vec<AttachmentInfo>,
    ) -> Result<String> {
        self.submit(EmailBuilder {
            to,
            subject,
            body,
            in_reply_to: None,
            references: &[],
            attachments,
        })
        .await
    }

    /// Send a reply to an existing email thread.  Returns the new JMAP email ID.
    pub async fn reply_to_email(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        _parent_email_id: &str,
        thread_id: &str,
        attachments: Vec<AttachmentInfo>,
    ) -> Result<String> {
        // Thread correctly using real RFC 5322 Message-IDs from the JMAP thread —
        // NOT the JMAP internal email id (which means nothing to other mail
        // servers or to Stalwart's own thread grouping). Resolving from the
        // thread is robust: it doesn't depend on a single, possibly-stale parent.
        let (in_reply_to, references) = self.reply_headers(thread_id).await;
        let refs: Vec<&str> = references.iter().map(String::as_str).collect();

        // Optionally append a standard quoted-original of the parent message
        // (the same message In-Reply-To points at). This is an email-layer
        // artifact only — it never reaches Matrix — and is best-effort: if the
        // quote can't be built the reply still sends unquoted.
        let quoted_body = if self.quote_replies {
            self.reply_quote(thread_id)
                .await
                .map(|quote| format!("{body}\n\n{quote}"))
        } else {
            None
        };
        let body = quoted_body.as_deref().unwrap_or(body);

        self.submit(EmailBuilder {
            to,
            subject,
            body,
            in_reply_to: in_reply_to.as_deref(),
            references: &refs,
            attachments,
        })
        .await
    }

    /// Resolve `(in_reply_to, references)` for a reply from the JMAP thread:
    /// `references` = every Message-ID in the thread (oldest→newest), and
    /// `in_reply_to` = the newest message's Message-ID. Best-effort: empty on
    /// failure (the reply still sends, just unthreaded).
    async fn reply_headers(&self, thread_id: &str) -> (Option<String>, Vec<String>) {
        // 1. The thread's email ids, in display order (oldest first).
        let email_ids = self.thread_email_ids(thread_id).await;
        if email_ids.is_empty() {
            return (None, Vec::new());
        }

        // 2. Their Message-IDs.
        let mut request = self.client.build();
        request
            .get_email()
            .ids(email_ids.clone())
            .properties([Property::MessageId]);
        let emails = match request.send().await {
            Ok(mut r) => r
                .pop_method_response()
                .and_then(|m| m.unwrap_get_email().ok())
                .map_or_else(Vec::new, |mut g| g.take_list()),
            Err(e) => {
                tracing::warn!(error = %e, thread_id, "Failed to fetch thread emails for threading");
                return (None, Vec::new());
            }
        };

        // 3. Build the chain in thread order; In-Reply-To = the newest message.
        let mut references: Vec<String> = Vec::new();
        let mut in_reply_to: Option<String> = None;
        for eid in &email_ids {
            if let Some(email) = emails.iter().find(|e| e.id() == Some(eid.as_str()))
                && let Some(mid) = email.message_id().and_then(<[String]>::first)
            {
                if !references.iter().any(|r| r == mid) {
                    references.push(mid.clone());
                }
                in_reply_to = Some(mid.clone());
            }
        }
        (in_reply_to, references)
    }

    /// The thread's email ids in display order (oldest first); the last is the
    /// newest message, which `In-Reply-To` points at. Best-effort: empty on
    /// failure. Shared by [`reply_headers`] and [`reply_quote`] so threading and
    /// quoting always reference the same message.
    async fn thread_email_ids(&self, thread_id: &str) -> Vec<String> {
        let mut request = self.client.build();
        request.get_thread().ids([thread_id.to_owned()]);
        match request.send().await {
            Ok(mut r) => r
                .pop_method_response()
                .and_then(|m| m.unwrap_get_thread().ok())
                .and_then(|mut t| t.take_list().into_iter().next())
                .map_or_else(Vec::new, |thread| thread.email_ids().to_vec()),
            Err(e) => {
                tracing::warn!(error = %e, thread_id, "Failed to fetch thread");
                Vec::new()
            }
        }
    }

    /// Build a plain-text quote of the newest message in the thread — the same
    /// message `In-Reply-To` points at — formatted as a standard reply trailer
    /// (`On {date}, {from} wrote:` followed by the `>`-quoted original body).
    /// Best-effort: returns `None` on any failure so a reply still sends.
    async fn reply_quote(&self, thread_id: &str) -> Option<String> {
        let newest = self.thread_email_ids(thread_id).await.pop()?;

        let mut request = self.client.build();
        let email_req = request.get_email();
        email_req.ids([newest]).properties([
            Property::From,
            Property::SentAt,
            Property::ReceivedAt,
            Property::TextBody,
            Property::BodyValues,
        ]);
        email_req
            .arguments()
            .fetch_text_body_values(true)
            .max_body_value_bytes(65_536);
        let email = request
            .send()
            .await
            .ok()?
            .pop_method_response()?
            .unwrap_get_email()
            .ok()?
            .take_list()
            .into_iter()
            .next()?;

        // Attribution: prefer "Name <addr>", fall back to the bare address.
        let from = email
            .from()
            .and_then(|f| f.first())
            .map_or_else(String::new, |addr| {
                addr.name().map_or_else(
                    || addr.email().to_owned(),
                    |name| format!("{name} <{}>", addr.email()),
                )
            });
        let date =
            crate::services::content::format_utc(email.sent_at().or_else(|| email.received_at())?);
        let body = email
            .text_body()
            .and_then(|parts| parts.first())
            .and_then(|part| part.part_id())
            .and_then(|pid| email.body_value(pid))
            .map(|v| v.value().to_owned())?;

        Some(crate::services::content::format_reply_quote(
            &from, &date, &body,
        ))
    }

    /// Returns the server-advertised maximum upload size in bytes.
    ///
    /// Returns `0` when the capability is absent (treat as unconstrained).
    #[must_use]
    pub fn max_upload_size(&self) -> usize {
        self.client.session().core_capabilities().map_or(
            0,
            jmap_client::core::session::CoreCapabilities::max_size_upload,
        )
    }

    /// Upload raw bytes to the JMAP blob store.
    ///
    /// Enforces the server's `maxSizeUpload` limit before making a network
    /// request, returning a human-readable [`Err`] when the file is too large
    /// so callers can surface it directly to the Matrix user.
    pub async fn upload_attachment(&self, file_bytes: &[u8], mime_type: &str) -> Result<String> {
        let max = self.max_upload_size();
        if max > 0 && file_bytes.len() > max {
            anyhow::bail!(
                "Attachment too large ({} bytes). The JMAP server limit is {} ({}).",
                file_bytes.len(),
                max,
                human_bytes(max),
            );
        }

        let mut resp: UploadResponse = self
            .client
            .upload(None, file_bytes.to_vec(), Some(mime_type))
            .await?;
        Ok(resp.take_blob_id())
    }

    /// Upload a stream of bytes directly to the JMAP blob store (streaming upload).
    ///
    /// This prevents high concurrent RAM usage / memory spikes (OOM risk) when
    /// transferring larger attachments from Matrix to JMAP.
    pub async fn upload_attachment_stream<S, E>(&self, stream: S, mime_type: &str) -> Result<String>
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Send + Sync + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        static UPLOAD_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        use jmap_client::core::session::URLPart;
        let account_id = self.client.default_account_id();
        let mut upload_url = String::new();

        for part in self.client.upload_url() {
            match part {
                URLPart::Value(value) => {
                    upload_url.push_str(value);
                }
                URLPart::Parameter(_param) => {
                    upload_url.push_str(account_id);
                }
            }
        }

        let client = UPLOAD_CLIENT.get_or_init(reqwest::Client::new);

        let body = reqwest::Body::wrap_stream(stream);
        let mut req = client
            .post(&upload_url)
            .header(reqwest::header::CONTENT_TYPE, mime_type)
            .timeout(self.client.timeout());

        for (name, value) in self.client.headers() {
            req = req.header(name.clone(), value.clone());
        }

        let resp = req.body(body).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("JMAP media upload failed: {status} - {text}");
        }

        let mut upload_resp: UploadResponse = resp.json().await?;
        Ok(upload_resp.take_blob_id())
    }

    /// Extract media, download it from Matrix, check its size limits, wrap it in a
    /// stream with size verification (OOM protection), apply filename/mime fallbacks,
    /// and upload it to the JMAP server.
    pub async fn upload_matrix_media(
        &self,
        matrix: &crate::matrix::MatrixClient,
        content: &RoomMessageEventContent,
    ) -> Result<AttachmentInfo> {
        let mxc_uri = match &content.msgtype {
            MessageType::File(f) => match &f.source {
                MediaSource::Plain(url) => Some(url),
                MediaSource::Encrypted(_) => None,
            },
            MessageType::Image(i) => match &i.source {
                MediaSource::Plain(url) => Some(url),
                MediaSource::Encrypted(_) => None,
            },
            MessageType::Audio(a) => match &a.source {
                MediaSource::Plain(url) => Some(url),
                MediaSource::Encrypted(_) => None,
            },
            MessageType::Video(v) => match &v.source {
                MediaSource::Plain(url) => Some(url),
                MediaSource::Encrypted(_) => None,
            },
            _ => None,
        }
        .context("No plain media URL found (or encrypted media is unsupported)")?;

        let max_size = self.max_upload_size();

        // 1. Check declared size first
        let declared_size = match &content.msgtype {
            MessageType::Image(img) => img.info.as_ref().and_then(|i| i.size),
            MessageType::File(file) => file.info.as_ref().and_then(|i| i.size),
            MessageType::Audio(audio) => audio.info.as_ref().and_then(|i| i.size),
            MessageType::Video(video) => video.info.as_ref().and_then(|i| i.size),
            _ => None,
        };

        if let Some(size) = declared_size {
            let size_bytes = usize::try_from(u64::from(size)).unwrap_or(usize::MAX);
            if max_size > 0 && size_bytes > max_size {
                anyhow::bail!(
                    "Media attachment exceeds the maximum size allowed by the JMAP mail server (limit: {}). Upload aborted.",
                    human_bytes(max_size)
                );
            }
        }

        // 2. Download from Matrix (Streaming)
        let (stream, filename, mime_type) = matrix.download_media_stream(mxc_uri.as_str()).await?;

        // 3. Wrap stream with dynamic size checking (to prevent OOM)
        let mut total_bytes = 0;
        let limit_stream = stream.map(move |chunk| match chunk {
            Ok(bytes) => {
                total_bytes += bytes.len();
                if max_size > 0 && total_bytes > max_size {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "File transfer exceeded maximum JMAP upload limit",
                    ))
                } else {
                    Ok(bytes)
                }
            }
            Err(e) => Err(std::io::Error::other(e.to_string())),
        });

        // 4. Use content bodies / mime types as safe fallbacks
        let filename_to_use = if filename.is_empty() || filename == "attachment" {
            content.msgtype.body().to_owned()
        } else {
            filename
        };

        let mime_type_to_use = if mime_type == "application/octet-stream" {
            let event_mime = match &content.msgtype {
                MessageType::File(f) => f.info.as_ref().and_then(|i| i.mimetype.as_deref()),
                MessageType::Image(i) => i.info.as_ref().and_then(|i| i.mimetype.as_deref()),
                MessageType::Audio(a) => a.info.as_ref().and_then(|i| i.mimetype.as_deref()),
                MessageType::Video(v) => v.info.as_ref().and_then(|i| i.mimetype.as_deref()),
                _ => None,
            };
            event_mime.unwrap_or(&mime_type).to_owned()
        } else {
            mime_type
        };

        // 5. Upload to JMAP
        let blob_id = self
            .upload_attachment_stream(limit_stream, &mime_type_to_use)
            .await?;

        Ok(AttachmentInfo {
            blob_id,
            name: filename_to_use,
            mime_type: mime_type_to_use,
        })
    }

    /// Mark an email as read in JMAP by adding the `$seen` keyword.
    pub async fn mark_as_read(&self, email_id: &str) -> Result<()> {
        self.set_seen(email_id, true).await
    }

    /// Mark an email as unread in JMAP by removing the `$seen` keyword — the
    /// mail-side reflection of a Matrix "mark unread" (`m.marked_unread`).
    pub async fn mark_as_unread(&self, email_id: &str) -> Result<()> {
        self.set_seen(email_id, false).await
    }

    /// Set (or clear) the `$seen` keyword on an email via `Email/set`.
    async fn set_seen(&self, email_id: &str, seen: bool) -> Result<()> {
        let mut request = self.client.build();
        {
            let params = request.params(Method::SetEmail);
            let mut args = Arguments::email_set(params);
            args.email_set_mut().update(email_id).keyword("$seen", seen);
            request.add_method_call(Method::SetEmail, args);
        }

        request.send().await?;
        Ok(())
    }

    /// Move every email in `thread_id` into the account's mailbox with `role`
    /// (Trash or Junk), replacing their current mailboxes — the JMAP side of a
    /// Matrix-driven trash/junk (ADR-0012). Returns `false` when the account has
    /// no mailbox with that role, so the caller can fall back to a local-only
    /// unbridge rather than guessing a destination.
    pub async fn move_thread_to_role(&self, thread_id: &str, role: MailboxRole) -> Result<bool> {
        let Some(target) = self.mailbox_id_for_role(role).await? else {
            return Ok(false);
        };
        let email_ids = self.thread_email_ids(thread_id).await;
        if email_ids.is_empty() {
            return Ok(true);
        }
        let mut request = self.client.build();
        {
            let params = request.params(Method::SetEmail);
            let mut args = Arguments::email_set(params);
            let set = args.email_set_mut();
            for id in &email_ids {
                set.update(id).mailbox_ids([target.as_str()]);
            }
            request.add_method_call(Method::SetEmail, args);
        }
        request.send().await?;
        Ok(true)
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// All the data needed to build a JMAP `Email/set` + `EmailSubmission/set` batch.
struct EmailBuilder<'a> {
    to: &'a str,
    subject: &'a str,
    body: &'a str,
    in_reply_to: Option<&'a str>,
    references: &'a [&'a str],
    attachments: Vec<AttachmentInfo>,
}

impl JmapSender {
    /// Resolve the id of the mailbox with the given `role` (e.g. Sent/Drafts),
    /// if the account has one.
    async fn mailbox_id_for_role(&self, role: MailboxRole) -> Result<Option<String>> {
        let mut request = self.client.build();
        request.query_mailbox().filter(MailboxFilter::role(role));
        let mut response = request
            .send()
            .await?
            .pop_method_response()
            .context("Empty response for Mailbox/query")?
            .unwrap_query_mailbox()?;
        Ok(response.take_ids().into_iter().next())
    }

    /// Fetch the account's primary sending identity as `(id, display name,
    /// email)`. Returns `None` if the account exposes no identity carrying an
    /// email.
    async fn primary_identity(&self) -> Option<(Option<String>, Option<String>, String)> {
        let mut request = self.client.build();
        request.get_identity();
        let response = request
            .send()
            .await
            .ok()?
            .pop_method_response()?
            .unwrap_get_identity()
            .ok()?;
        response.list().iter().find_map(|identity| {
            identity.email().map(|email| {
                (
                    identity.id().map(str::to_owned),
                    identity.name().map(str::to_owned),
                    email.to_owned(),
                )
            })
        })
    }

    /// Build and submit a JMAP batch request, returning the created email's ID.
    async fn submit(&self, params: EmailBuilder<'_>) -> Result<String> {
        // Every JMAP email must belong to at least one mailbox or the server
        // rejects the Email/set ("Message has to belong to at least one
        // mailbox"). File the outgoing copy in Sent, falling back to Drafts.
        let mailbox_id = match self.mailbox_id_for_role(MailboxRole::Sent).await? {
            Some(id) => id,
            None => self
                .mailbox_id_for_role(MailboxRole::Drafts)
                .await?
                .context("Account has neither a Sent nor a Drafts mailbox")?,
        };

        let mut request = self.client.build();

        // — Email/set —
        request.add_method_call(
            Method::SetEmail,
            Arguments::email_set(request.params(Method::SetEmail)),
        );
        let email_set = request
            .method_calls
            .last_mut()
            .expect("just pushed")
            .1
            .email_set_mut();

        // Resolve the account's own From identity. Outgoing mail MUST carry a
        // `From:` header (RFC 5322) or relays/recipients reject it — and without
        // it the Sent copy comes back senderless and gets re-bridged as a bogus
        // `unknown@sender` room. Fall back to the session username only if the
        // account exposes no identity.
        let (identity_id, from_name, from_email) = self
            .primary_identity()
            .await
            .unwrap_or_else(|| (None, None, self.client.session().username().to_owned()));
        let from_addr: jmap_client::email::EmailAddress = from_name.map_or_else(
            || from_email.clone().into(),
            |name| (name, from_email.clone()).into(),
        );

        let email = email_set.create_with_id("draft");
        email.mailbox_id(&mailbox_id, true);
        email.subject(params.subject);
        email.from(vec![from_addr]);
        email.to(vec![params.to]);

        if let Some(reply_to) = params.in_reply_to {
            email.in_reply_to(vec![reply_to.to_owned()]);
        }
        if !params.references.is_empty() {
            email.references(params.references.iter().map(ToString::to_string));
        }

        // Body
        let body_part = EmailBodyPart::new()
            .part_id("body")
            .content_type("text/plain");
        email.body_structure(body_part.into());
        email.body_value(
            "body".to_owned(),
            EmailBodyValue::from(params.body.to_owned()),
        );

        // Attachments
        for att in params.attachments {
            let part = EmailBodyPart::new()
                .blob_id(att.blob_id)
                .name(att.name)
                .content_type(att.mime_type);
            email.attachment(part);
        }

        // — EmailSubmission/set —
        request.add_method_call(
            Method::SetEmailSubmission,
            Arguments::email_submission_set(request.params(Method::SetEmailSubmission)),
        );
        let submission_set = request
            .method_calls
            .last_mut()
            .expect("just pushed")
            .1
            .email_submission_set_mut();

        let submission = submission_set.create_with_id("sub");
        submission.email_id("#draft");
        // Bind the submission to the account's sending identity. Stalwart rejects
        // submissions whose envelope/From can't be tied to a known identity, so
        // without this the message is saved to Sent but never queued for delivery.
        if let Some(id) = &identity_id {
            submission.identity_id(id.clone());
        }
        // Envelope return-path = the real address, not the bare login name.
        let rcpt_to = vec![params.to.to_owned()];
        submission.envelope(from_email, rcpt_to);

        // — Send and unpack —
        let mut response = request.send().await?;
        let mut email_set_resp = response.method_response_by_pos(0).unwrap_set_email()?;
        let email_id = email_set_resp
            .created("draft")?
            .id()
            .context("JMAP response did not include an ID for the created email")?
            .to_owned();

        // `method_response_by_pos` removes by index, so after taking position 0
        // the EmailSubmission response is now at position 0. Verify the
        // submission was actually accepted — otherwise the message sits in Sent
        // but is never delivered, and we would wrongly report success.
        response
            .method_response_by_pos(0)
            .unwrap_set_email_submission()?
            .created("sub")
            .context("email saved to Sent but the JMAP submission was rejected (not delivered)")?;

        Ok(email_id)
    }
}

// ─── Utilities ────────────────────────────────────────────────────────────────

/// Format a byte count as a human-readable string (e.g. `"5.0 MB"`).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn human_bytes(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = UNITS[0];
    for u in UNITS.iter().skip(1) {
        if size < 1024.0 {
            break;
        }
        size /= 1024.0;
        unit = u;
    }
    // Avoid spurious ".0" for round numbers.
    if size.fract() < 0.05 {
        format!("{size:.0} {unit}")
    } else {
        format!("{size:.1} {unit}")
    }
}
