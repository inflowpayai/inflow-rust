#![doc = include_str!("../README.md")]

mod error;
mod lifecycle;
mod options;
mod transport;

pub use error::Error;
pub use options::{AccessTokenProvider, Authentication, ClientOptions};
pub use transport::{Transport, TransportError, TransportRequest, TransportResponse};

#[doc(hidden)]
pub mod internal;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Environment {
    #[default]
    Production,
    Sandbox,
}

impl Environment {
    pub const fn api_base_url(self) -> &'static str {
        match self {
            Self::Production => "https://api.inflowpay.ai",
            Self::Sandbox => "https://sandbox.inflowpay.ai",
        }
    }
}
