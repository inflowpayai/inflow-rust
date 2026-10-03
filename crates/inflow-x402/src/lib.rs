#![doc = include_str!("../README.md")]

mod identifier;
mod wire;

pub use identifier::{
    generate_payment_id, identifier_declaration, identifier_entry, valid_payment_id,
};
pub use inflow_core::Error;
pub use wire::{decode, encode, facilitator_request};
// The v2 response enums omit extension data. These upstream wrappers retain complete JSON.
pub use x402_types::proto::v2::{PaymentRequirements, X402Version2};
pub use x402_types::proto::{SettleRequest, SettleResponse, VerifyRequest, VerifyResponse};

#[doc(hidden)]
pub mod internal;

pub const X402_VERSION: u8 = 2;
pub const NETWORK_INFLOW: &str = "inflow:1";
pub const PAYMENT_REQUIRED: &str = "PAYMENT-REQUIRED";
pub const PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
pub const PAYMENT_RESPONSE: &str = "PAYMENT-RESPONSE";

fn invalid(field: &str) -> Error {
    Error::new("INVALID_X402_DATA", format!("invalid x402 {field}"))
}
