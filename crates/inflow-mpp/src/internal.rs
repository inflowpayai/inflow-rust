//! Endpoint primitives shared by the Buyer and Seller crates, not an integrator endpoint API.

use crate::codec::invalid;
use http::{HeaderMap, HeaderValue, Method};
use inflow_core::{
    ClientOptions, Error,
    internal::{ApprovalCleanup, HttpClient},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct MppClient(HttpClient);

impl MppClient {
    pub fn approval_cleanup(&self, id: &str) -> Result<ApprovalCleanup, Error> {
        ApprovalCleanup::new(self.0.clone(), id)
    }

    pub async fn poll_transaction(
        &self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        // Buyer polling owns its deadline and error policy; do not insert transport retry delays.
        self.call(
            Method::GET,
            &format!("/v1/transactions/{}/mpp", segment(id)?),
            None,
            None,
            0,
            cancellation,
        )
        .await
    }

    pub fn new(options: ClientOptions) -> Result<Self, Error> {
        Ok(Self(HttpClient::new(options)?))
    }

    pub async fn config(&self, cancellation: &CancellationToken) -> Result<Value, Error> {
        self.call(Method::GET, "/v1/mpp/config", None, None, 3, cancellation)
            .await
    }

    pub async fn supported(&self, cancellation: &CancellationToken) -> Result<Value, Error> {
        self.call(
            Method::GET,
            "/v1/transactions/mpp-supported",
            None,
            None,
            3,
            cancellation,
        )
        .await
    }

    pub async fn create(
        &self,
        body: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        // A lost response does not prove that the platform failed to create an approval.
        self.call(
            Method::POST,
            "/v1/transactions/mpp",
            Some(body),
            None,
            0,
            cancellation,
        )
        .await
    }

    pub async fn transaction(
        &self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.call(
            Method::GET,
            &format!("/v1/transactions/{}/mpp", segment(id)?),
            None,
            None,
            3,
            cancellation,
        )
        .await
    }

    pub async fn authorize(
        &self,
        id: &str,
        challenge: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        // Each successful authorization issues a fresh credential; do not retry a lost response.
        self.call(
            Method::POST,
            &format!("/v1/subscriptions/{}/authorize", segment(id)?),
            Some(json!({"challenge": challenge})),
            None,
            0,
            cancellation,
        )
        .await
    }

    pub async fn validate(
        &self,
        credential: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.call(
            Method::POST,
            "/v1/mpp/validate",
            Some(json!({"credential": credential})),
            None,
            3,
            cancellation,
        )
        .await
    }

    pub async fn broadcast(
        &self,
        credential: Value,
        idempotency_key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.call(
            Method::POST,
            "/v1/mpp/broadcast",
            Some(json!({"credential": credential})),
            Some(idempotency_key),
            3,
            cancellation,
        )
        .await
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
        retries: u8,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        let mut headers = HeaderMap::new();
        if let Some(key) = key {
            if key.trim().is_empty() {
                return Err(invalid("idempotency key"));
            }
            headers.insert(
                "idempotency-key",
                HeaderValue::from_str(key).map_err(|_| invalid("idempotency key"))?,
            );
        }
        self.0
            .request(method, path, body, headers, retries, cancellation)
            .await
    }
}

fn segment(value: &str) -> Result<String, Error> {
    if value.is_empty() || matches!(value, "." | "..") {
        return Err(invalid("identifier"));
    }
    Ok(value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect())
}
