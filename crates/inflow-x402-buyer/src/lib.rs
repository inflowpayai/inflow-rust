#![doc = include_str!("../README.md")]

mod buyer;
mod http;
mod payment;

#[cfg(feature = "evm")]
pub mod eip7702;

#[cfg(feature = "evm")]
pub use x402_chain_eip155 as evm;
#[cfg(feature = "solana")]
pub use x402_chain_solana as solana;

pub use buyer::{Buyer, BuyerOptions};
pub use http::{HttpBuyer, PaymentExtension};
pub use inflow_core::{Authentication, ClientOptions, Environment, Error, PaymentStatusOptions};
pub use payment::{EncodedPayment, Payment, SignOptions, WaitOptions};
pub use tokio_util::sync::CancellationToken;
pub use x402_types::proto::{OriginalJson, v2::PaymentRequired};
pub use x402_types::scheme::client::{PaymentSelector, X402SchemeClient};

fn invalid(message: &str) -> Error {
    Error::new("X402_INVALID_PAYMENT", message)
}
