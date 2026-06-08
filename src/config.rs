use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct Registration {
    pub id: String,
    pub url: String,
    pub as_token: String,
    pub hs_token: String,
    pub sender_localpart: String,
    pub namespaces: Namespaces,
    #[serde(
        rename = "de.matrix.org.msc2409.ephemeral",
        alias = "receive_ephemeral",
        alias = "ephemeral",
        default
    )]
    pub receive_ephemeral: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Namespaces {
    pub users: Vec<Namespace>,
    pub aliases: Vec<Namespace>,
    pub rooms: Vec<Namespace>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Namespace {
    pub exclusive: bool,
    pub regex: String,
}

pub(crate) fn generate_token() -> String {
    use rand::distr::SampleString;
    rand::distr::Alphanumeric.sample_string(&mut rand::rng(), 64)
}

#[must_use]
pub fn generate_registration(url: &str) -> Registration {
    Registration {
        id: "jmap-bridge".to_owned(),
        url: url.to_owned(),
        as_token: generate_token(),
        hs_token: generate_token(),
        sender_localpart: "_jmap_bot".to_owned(),
        namespaces: Namespaces {
            users: vec![Namespace {
                exclusive: true,
                regex: "@_jmap_.*".to_owned(),
            }],
            aliases: vec![],
            rooms: vec![],
        },
        receive_ephemeral: true,
    }
}
