use serde::{Serialize, de::DeserializeOwned};

/// Base64-encoded binary data, as the protocol transmits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Binary(pub String);

pub trait Command: Serialize {
    const METHOD: &'static str;
    type Returns: DeserializeOwned;
}

pub trait Event: DeserializeOwned {
    const NAME: &'static str;
}

pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/protocol.rs"));
}

pub mod client;
pub use client::{Answer, Arrival, CdpError, Client, Feed, Subscription, Tagged};
