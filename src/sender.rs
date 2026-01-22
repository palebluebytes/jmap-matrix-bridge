use std::sync::Arc;
use anyhow::{Result, Context};
use jmap_client::client::Client;
use jmap_client::core::set::SetObject;
use jmap_client::email::EmailBodyPart;
use jmap_client::core::response::EmailSetResponse;

#[derive(Clone)]
pub struct JmapSender {
    client: Arc<Client>,
}

impl JmapSender {
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }

    pub async fn send_email(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        // 1. Create the email object (Draft)
        let mut request = self.client.build();
        
        let draft_ref = request.set_email().create()
            .to([to]) // Assuming &str implements Into<EmailAddress>
            .subject(subject)
            .text_body(EmailBodyPart::new().part_id("body"))
            .body_value("body".to_string(), body)
            .create_id()
            .unwrap();

        let mut response = request.send_single::<EmailSetResponse>().await?;
        let email_id = response.created(&draft_ref)
            .context("Failed to create draft email")?
            .id()
            .context("Server returned no ID for created email")?
            .to_string();

        // 2. Submit the email (using server default identity)
        let mut request = self.client.build();
        let _submission_ref = request.set_email_submission().create()
            .email_id(email_id)
            .create_id()
            .unwrap();

        request.send_single::<jmap_client::core::response::EmailSubmissionSetResponse>().await?;

        Ok(())
    }
}
