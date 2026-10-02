//! Shared InFlow environment and client configuration.

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
