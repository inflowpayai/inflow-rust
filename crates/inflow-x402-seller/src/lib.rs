#![doc = include_str!("../README.md")]

mod client;
mod offers;

pub use client::{Facilitator, Seller};
pub use inflow_core::{Authentication, ClientOptions, Environment, Error};
pub use inflow_x402::{
    PaymentRequirements, SettleRequest, SettleResponse, VerifyRequest, VerifyResponse,
};
pub use offers::{OfferOptions, Price, Route};
pub use tokio_util::sync::CancellationToken;
pub use x402_types::proto::SupportedResponse;

fn invalid(message: &str) -> Error {
    Error::new("INVALID_X402_CONFIGURATION", message)
}
