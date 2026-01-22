use serde::{Deserialize, Serialize};
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;

#[derive(Serialize, Deserialize)]
pub struct Registration {
    pub id: String,
    pub url: String,
    pub as_token: String,
    pub hs_token: String,
    pub sender_localpart: String,
    pub namespaces: Namespaces,
}

#[derive(Serialize, Deserialize)]
pub struct Namespaces {
    pub users: Vec<Namespace>,
    pub aliases: Vec<Namespace>,
    pub rooms: Vec<Namespace>,
}

#[derive(Serialize, Deserialize)]
pub struct Namespace {
    pub exclusive: bool,
    pub regex: String,
}

pub fn generate_token() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

pub fn generate_registration(url: &str) -> Registration {
    Registration {
        id: "jmap-bridge".to_string(),
        url: url.to_string(),
        as_token: generate_token(),
        hs_token: generate_token(),
        sender_localpart: "_jmap_bot".to_string(),
        namespaces: Namespaces {
            users: vec![Namespace {
                exclusive: true,
                regex: "@_jmap_.*".to_string(),
            }],
            aliases: vec![],
            rooms: vec![],
        },
    }
}
