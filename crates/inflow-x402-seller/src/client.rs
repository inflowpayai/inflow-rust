use crate::{
    Authentication, CancellationToken, ClientOptions, Error, SettleRequest, SettleResponse,
    SupportedResponse, VerifyRequest, VerifyResponse, invalid,
};
use inflow_x402::internal::X402Client;
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

#[derive(Clone, Default)]
struct Cache<T> {
    value: Arc<Mutex<Option<(T, Instant)>>>,
    generation: Arc<AtomicU64>,
}

impl<T: Clone> Cache<T> {
    async fn load(
        &self,
        fetch: impl Future<Output = Result<T, Error>>,
        force: bool,
        token: &CancellationToken,
    ) -> Result<T, Error> {
        let observed = self.generation.load(Ordering::Acquire);
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(Error::new("CANCELLED", "operation cancelled")),
            result = async {
                let mut cache = self.value.lock().await;
                if let Some((value, fetched)) = &*cache
                    && fetched.elapsed() < Duration::from_secs(3600)
                    && (!force || self.generation.load(Ordering::Acquire) != observed)
                {
                    return Ok(value.clone());
                }
                let value = fetch.await?;
                *cache = Some((value.clone(), Instant::now()));
                self.generation.fetch_add(1, Ordering::Release);
                Ok(value)
            } => result,
        }
    }
}

/// Implements the upstream facilitator interface; construction does not make a request.
#[derive(Clone)]
pub struct Facilitator {
    client: X402Client,
    supported: Cache<SupportedResponse>,
}

impl Facilitator {
    /// Anonymous access and Seller API keys use the same verification and settlement endpoints.
    pub fn new(options: ClientOptions) -> Result<Self, Error> {
        Ok(Self {
            client: X402Client::new(options)?,
            supported: Cache::default(),
        })
    }

    pub async fn supported(&self, token: &CancellationToken) -> Result<SupportedResponse, Error> {
        self.supported
            .load(self.fetch_supported(token), false, token)
            .await
    }

    async fn fetch_supported(&self, token: &CancellationToken) -> Result<SupportedResponse, Error> {
        serde_json::from_value(self.client.supported(token).await?)
            .map_err(|_| invalid("invalid facilitator capabilities"))
    }

    pub async fn verify(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<VerifyResponse, Error> {
        self.client.verify(request, token).await
    }

    pub async fn settle(
        &self,
        request: &SettleRequest,
        token: &CancellationToken,
    ) -> Result<SettleResponse, Error> {
        self.client.settle(request, token).await
    }
}

impl x402_types::facilitator::Facilitator for Facilitator {
    type Error = Error;

    async fn supported(&self) -> Result<SupportedResponse, Error> {
        self.supported(&CancellationToken::new()).await
    }

    async fn verify(&self, request: &VerifyRequest) -> Result<VerifyResponse, Error> {
        self.verify(request, &CancellationToken::new()).await
    }

    async fn settle(&self, request: &SettleRequest) -> Result<SettleResponse, Error> {
        self.settle(request, &CancellationToken::new()).await
    }
}

#[derive(Clone)]
pub struct Seller {
    facilitator: Facilitator,
    config: Cache<Value>,
}

impl Seller {
    /// Loads Seller configuration and facilitator capabilities before returning.
    pub async fn new(options: ClientOptions, token: &CancellationToken) -> Result<Self, Error> {
        if !matches!(&options.authentication, Authentication::ApiKey(key) if !key.trim().is_empty())
            && !matches!(&options.authentication, Authentication::ApiKeyProvider(_))
        {
            return Err(invalid("Seller configuration requires a Seller API key"));
        }
        let seller = Self {
            facilitator: Facilitator::new(options)?,
            config: Cache::default(),
        };
        tokio::try_join!(seller.config(token), seller.facilitator.supported(token))?;
        Ok(seller)
    }

    pub fn facilitator(&self) -> Facilitator {
        self.facilitator.clone()
    }

    pub async fn config(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.config
            .load(self.fetch_config(token), false, token)
            .await
    }

    pub async fn refresh_config(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.config
            .load(self.fetch_config(token), true, token)
            .await
    }

    pub async fn refresh_supported(
        &self,
        token: &CancellationToken,
    ) -> Result<SupportedResponse, Error> {
        self.facilitator
            .supported
            .load(self.facilitator.fetch_supported(token), true, token)
            .await
    }

    async fn fetch_config(&self, token: &CancellationToken) -> Result<Value, Error> {
        let value = self.facilitator.client.config(token).await?;
        for field in ["assets", "wallets", "paymentMethods"] {
            if !value[field].is_array() {
                return Err(invalid(
                    "Seller config requires assets, wallets and paymentMethods arrays",
                ));
            }
        }
        Ok(value)
    }

    pub async fn signer_addresses(
        &self,
        network: &str,
        token: &CancellationToken,
    ) -> Result<Vec<String>, Error> {
        let supported = self.facilitator.supported(token).await?;
        let wildcard = network
            .split_once(':')
            .filter(|(namespace, _)| !namespace.is_empty())
            .map(|(namespace, _)| format!("{namespace}:*"));
        Ok(supported
            .signers
            .iter()
            .find(|(key, _)| key.to_string() == network)
            .map(|(_, value)| value)
            .or_else(|| {
                wildcard.as_ref().and_then(|pattern| {
                    supported
                        .signers
                        .iter()
                        .find(|(key, _)| key.to_string() == *pattern)
                        .map(|(_, value)| value)
                })
            })
            .cloned()
            .unwrap_or_default())
    }
}
