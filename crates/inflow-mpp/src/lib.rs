#![doc = include_str!("../README.md")]

mod card;
mod codec;
mod headers;
mod methods;
mod stripe;

#[doc(hidden)]
pub mod internal;

pub use codec::{Credential, decode, decode_credential, decode_receipt, encode, encode_credential};
pub use headers::{parse_challenges, render_challenge};
pub use inflow_core::Error;
pub use methods::{validate_payload, validate_request};
pub use mpp::protocol::core::{Base64UrlJson, PaymentChallenge, Receipt};

pub const METHOD_INFLOW: &str = "inflow";
pub const METHOD_TEMPO: &str = "tempo";
pub const INTENT_CHARGE: &str = "charge";
pub const INTENT_SUBSCRIPTION: &str = "subscription";
