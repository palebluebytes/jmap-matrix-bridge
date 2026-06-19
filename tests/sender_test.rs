#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::sender::JmapSender;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_reply_to_email_sets_references() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // reply_to_email resolves Message-IDs from the JMAP thread: Thread/get gives
    // the email ids, then Email/get gives their Message-IDs.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            json["methodCalls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c[0] == "Thread/get")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Thread/get", {
                "accountId": "acc1", "state": "s1", "notFound": [],
                "list": [{ "id": "thread-123", "emailIds": ["e_root", "e_reply"] }]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            json["methodCalls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c[0] == "Email/get")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "acc1", "state": "s1", "notFound": [],
                "list": [
                    { "id": "e_root", "messageId": ["<root@example.com>"] },
                    { "id": "e_reply", "messageId": ["<msgid-1@example.com>"] }
                ]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            for call in method_calls {
                let arr = call.as_array().unwrap();
                if arr[0].as_str().unwrap() == "Email/set" {
                    let create = arr[1]
                        .as_object()
                        .unwrap()
                        .get("create")
                        .unwrap()
                        .as_object()
                        .unwrap();
                    let email = create.get("draft").unwrap().as_object().unwrap();
                    // In-Reply-To must be the parent's real RFC Message-ID, not
                    // the JMAP internal id we were handed.
                    let in_reply_to = email.get("inReplyTo").unwrap().as_array().unwrap();
                    assert_eq!(in_reply_to[0].as_str().unwrap(), "<msgid-1@example.com>");
                    // References = the parent's chain + its Message-ID.
                    let references = email.get("references").unwrap().as_array().unwrap();
                    assert_eq!(references[0].as_str().unwrap(), "<root@example.com>");
                    assert_eq!(references[1].as_str().unwrap(), "<msgid-1@example.com>");
                    return true;
                }
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", {
                    "created": {
                        "draft": {
                            "id": "email-id-456"
                        }
                    }
                }, "0"],
                ["EmailSubmission/set", {
                    "created": {
                        "sub": {
                            "id": "sub-id-789"
                        }
                    }
                }, "1"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // submit() resolves the Sent mailbox before composing; answer that
    // Mailbox/query (priority 1 so it wins over any catch-all POST mock).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls
                .iter()
                .any(|call| call.as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {
                "accountId": "acc1",
                "queryState": "mq1",
                "position": 0,
                "ids": ["sent-mailbox-id"]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", mock_server.uri()))
        .await
        .unwrap();
    let sender = JmapSender::new(std::sync::Arc::new(client));

    let result = sender
        .reply_to_email(
            "to@example.com",
            "Re: Subject",
            "Body",
            "message-id-123",
            "thread-123",
            vec![],
        )
        .await;
    if let Err(e) = &result {
        eprintln!("Error: {e:?}");
    }
    assert!(
        result.is_ok(),
        "reply_to_email() should succeed when references are set"
    );
    assert_eq!(result.unwrap(), "email-id-456");
}

#[tokio::test]
async fn test_send_email_with_attachments() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/set and VERIFY ATTACHMENTS
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            for call in method_calls {
                let arr = call.as_array().unwrap();
                if arr[0].as_str().unwrap() == "Email/set" {
                    let create = arr[1]
                        .as_object()
                        .unwrap()
                        .get("create")
                        .unwrap()
                        .as_object()
                        .unwrap();
                    for (_, email) in create {
                        let email_obj = email.as_object().unwrap();
                        let attachments = email_obj.get("attachments").unwrap().as_array().unwrap();
                        if attachments.len() == 1 {
                            let att = attachments[0].as_object().unwrap();
                            if att.get("blobId").unwrap().as_str().unwrap() == "blob-123"
                                && att.get("name").unwrap().as_str().unwrap() == "test.txt"
                                && att.get("type").unwrap().as_str().unwrap() == "text/plain"
                            {
                                return true;
                            }
                        }
                    }
                }
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", { "created": { "draft": { "id": "email-id-456" } } }, "0"],
                ["EmailSubmission/set", { "created": { "sub": { "id": "sub-id-789" } } }, "1"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // submit() resolves the Sent mailbox before composing; answer that
    // Mailbox/query (priority 1 so it wins over the catch-all EmailSubmission mock).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls
                .iter()
                .any(|call| call.as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {
                "accountId": "acc1",
                "queryState": "mq1",
                "position": 0,
                "ids": ["sent-mailbox-id"]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", mock_server.uri()))
        .await
        .unwrap();
    let sender = jmap_matrix_bridge::sender::JmapSender::new(std::sync::Arc::new(client));

    let attachments = vec![jmap_matrix_bridge::sender::AttachmentInfo {
        blob_id: "blob-123".to_string(),
        name: "test.txt".to_string(),
        mime_type: "text/plain".to_string(),
    }];

    let result = sender
        .send_email("to@example.com", "Subject", "Body", attachments)
        .await;
    assert!(
        result.is_ok(),
        "send_email() should succeed with attachments"
    );
    assert_eq!(result.unwrap(), "email-id-456");
}

#[tokio::test]
async fn test_upload_attachment_stream() {
    let mock_server = MockServer::start().await;

    // 1. Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": format!("{}/upload", mock_server.uri()),
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // 2. Mock POST to /upload
    Mock::given(method("POST"))
        .and(path("/upload"))
        .and(|request: &wiremock::Request| {
            // Verify body is streamed correctly and matches expected text
            let body_str = std::str::from_utf8(&request.body).unwrap();
            body_str == "streamed bytes content"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accountId": "a1",
            "blobId": "blob-uploaded-stream",
            "type": "text/plain",
            "size": 22
        })))
        .mount(&mock_server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", mock_server.uri()))
        .await
        .unwrap();
    let sender = jmap_matrix_bridge::sender::JmapSender::new(std::sync::Arc::new(client));

    // 3. Create a stream of bytes
    let data = vec![Ok::<_, std::io::Error>(bytes::Bytes::from(
        "streamed bytes content",
    ))];
    let stream = futures_util::stream::iter(data);

    let result = sender.upload_attachment_stream(stream, "text/plain").await;
    assert!(result.is_ok(), "upload_attachment_stream() should succeed");
    assert_eq!(result.unwrap(), "blob-uploaded-stream");
}

#[tokio::test]
async fn test_send_email_sets_from_header_from_identity() {
    let mock_server = MockServer::start().await;

    // JMAP session discovery.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "thomas",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // Identity/get -> the account's From identity (name + email).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            json.get("methodCalls")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_array().unwrap()[0].as_str().unwrap() == "Identity/get")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Identity/get", {
                "accountId": "acc1",
                "state": "i1",
                "list": [{ "id": "id1", "name": "Thomas", "email": "thomas@palebluebytes.space" }],
                "notFound": []
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    // Email/set must carry a `from` matching the identity (the bug: it didn't).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let calls = json.get("methodCalls").unwrap().as_array().unwrap();
            for call in calls {
                let arr = call.as_array().unwrap();
                if arr[0].as_str().unwrap() == "Email/set" {
                    let draft = arr[1]
                        .as_object()
                        .unwrap()
                        .get("create")
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .get("draft")
                        .unwrap()
                        .as_object()
                        .unwrap();
                    let from = draft
                        .get("from")
                        .expect("Email/set create must include `from`")
                        .as_array()
                        .unwrap();
                    assert_eq!(from.len(), 1);
                    assert_eq!(
                        from[0].get("email").unwrap().as_str().unwrap(),
                        "thomas@palebluebytes.space"
                    );
                    assert_eq!(from[0].get("name").unwrap().as_str().unwrap(), "Thomas");
                    return true;
                }
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", { "created": { "draft": { "id": "email-id-1" } } }, "0"],
                ["EmailSubmission/set", { "created": { "sub": { "id": "sub-1" } } }, "1"]
            ]
        })))
        .with_priority(2)
        .mount(&mock_server)
        .await;

    // Mailbox/query for the Sent folder.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            json.get("methodCalls")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {
                "accountId": "acc1", "queryState": "mq1", "position": 0, "ids": ["sent-id"]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", mock_server.uri()))
        .await
        .unwrap();
    let sender = JmapSender::new(std::sync::Arc::new(client));

    let result = sender
        .send_email("to@example.com", "Subject", "Body", vec![])
        .await;
    assert!(result.is_ok(), "send_email() should succeed: {result:?}");
    assert_eq!(result.unwrap(), "email-id-1");
}

#[tokio::test]
async fn send_email_errors_when_submission_is_rejected() {
    let mock_server = MockServer::start().await;

    // Session discovery.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "thomas", "accounts": {}, "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download", "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // Identity/get.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|r: &wiremock::Request| {
            let j: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            j["methodCalls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c[0] == "Identity/get")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "methodResponses": [["Identity/get", {
                "accountId": "acc1", "state": "i1",
                "list": [{ "id": "id1", "name": "Thomas", "email": "thomas@palebluebytes.space" }],
                "notFound": []
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    // Mailbox/query (Sent).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|r: &wiremock::Request| {
            let j: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            j["methodCalls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c[0] == "Mailbox/query")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "methodResponses": [["Mailbox/query", {
                "accountId": "acc1", "queryState": "mq1", "position": 0, "ids": ["sent-id"]
            }, "0"]]
        })))
        .with_priority(1)
        .mount(&mock_server)
        .await;

    // Email created, but the submission is REJECTED — must surface as an error,
    // not a false success (the bug: the message would sit in Sent undelivered).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|r: &wiremock::Request| {
            let j: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            j["methodCalls"].as_array().unwrap().iter().any(|c| c[0] == "Email/set")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "methodResponses": [
                ["Email/set", { "created": { "draft": { "id": "email-id-1" } } }, "0"],
                ["EmailSubmission/set", {
                    "notCreated": { "sub": { "type": "forbiddenFrom", "description": "From not allowed" } }
                }, "1"]
            ]
        })))
        .with_priority(2)
        .mount(&mock_server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", mock_server.uri()))
        .await
        .unwrap();
    let sender = JmapSender::new(std::sync::Arc::new(client));

    let result = sender
        .send_email("to@example.com", "Subject", "Body", vec![])
        .await;
    assert!(
        result.is_err(),
        "a rejected submission must be an error, not a silent success"
    );
}
