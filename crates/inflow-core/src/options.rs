use crate::{Environment, Error, Transport};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// Called for each HTTP attempt. Provider failures are not transport failures and are not retried.
pub trait AccessTokenProvider: Send + Sync {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>>;
}

/// Called for each HTTP attempt, including retries. Errors stop the request.
pub trait ApiKeyProvider: Send + Sync {
    fn api_key(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>>;
}

#[derive(Clone, Default)]
pub enum Authentication {
    #[default]
    Anonymous,
    ApiKey(String),
    ApiKeyProvider(Arc<dyn ApiKeyProvider>),
    Bearer(Arc<dyn AccessTokenProvider>),
}

#[derive(Clone)]
pub struct ClientOptions {
    pub environment: Environment,
    pub authentication: Authentication,
    pub timeout: Duration,
    /// Custom transports receive credentials. They must not follow redirects or log request secrets.
    pub transport: Option<Arc<dyn Transport>>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            environment: Environment::Production,
            authentication: Authentication::Anonymous,
            timeout: Duration::from_secs(30),
            transport: None,
        }
    }
}
