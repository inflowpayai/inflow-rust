#![doc = include_str!("../README.md")]

mod card;
#[cfg(feature = "tower")]
mod middleware;
#[cfg(feature = "tower")]
mod pricing;
mod seller;

pub use inflow_core::{Authentication, ClientOptions, Environment, Error};
pub use inflow_mpp::{Credential, PaymentChallenge, Receipt, decode_credential, render_challenge};
#[cfg(feature = "tower")]
pub use mpp::server::middleware::{PaymentBodyLayer, PaymentLayer};
#[cfg(feature = "tower")]
pub use pricing::{
    RequestPaymentBodyLayer, RequestPaymentBodyService, RequestPaymentLayer, RequestPaymentService,
};
pub use seller::{ChallengeOptions, Method, Offer, Seller, SellerOptions, Validation};
pub use tokio_util::sync::CancellationToken;
