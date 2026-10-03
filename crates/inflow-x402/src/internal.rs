//! Endpoint primitives for the Buyer and Seller crates, not an integrator endpoint API.

use crate::{Error, SettleResponse, VerifyRequest, VerifyResponse, invalid};
use http::{HeaderMap, Method};
use inflow_core::{
    ClientOptions,
    internal::{ApprovalCleanup, HttpClient},
};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct X402Client(HttpClient);

impl X402Client {
    pub fn new(options: ClientOptions) -> Result<Self, Error> {
        Ok(Self(HttpClient::new(options)?))
    }

    pub fn approval_cleanup(&self, id: &str) -> Result<ApprovalCleanup, Error> {
        ApprovalCleanup::new(self.0.clone(), id)
    }

    pub async fn config(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.call(Method::GET, "/v1/x402/config", None, 3, token)
            .await
    }

    pub async fn supported(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.call(Method::GET, "/v1/x402/supported", None, 3, token)
            .await
    }

    pub async fn buyer_supported(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.call(
            Method::GET,
            "/v1/transactions/x402-supported",
            None,
            3,
            token,
        )
        .await
    }

    pub async fn balances(&self, token: &CancellationToken) -> Result<Value, Error> {
        self.call(Method::GET, "/v1/balances", None, 3, token).await
    }

    pub async fn create(&self, body: Value, token: &CancellationToken) -> Result<Value, Error> {
        // A lost creation response does not prove that no approval was created.
        self.call(Method::POST, "/v1/transactions/x402", Some(body), 0, token)
            .await
    }

    pub async fn poll(&self, id: &str, token: &CancellationToken) -> Result<Value, Error> {
        if id.is_empty() || matches!(id, "." | "..") {
            return Err(invalid("transaction identifier"));
        }
        let id: String = id
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        self.call(
            Method::GET,
            &format!("/v1/transactions/{id}/x402"),
            None,
            0,
            token,
        )
        .await
    }

    pub async fn verify(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<VerifyResponse, Error> {
        let body = prepare(request)?;
        let response = match self
            .call(Method::POST, "/v1/x402/verify", Some(body), 0, token)
            .await
        {
            Ok(response) => response,
            Err(error)
                if error.http_status == 412
                    && error.body["isValid"] == false
                    && error.body["invalidReason"] == "permit2_allowance_required" =>
            {
                *error.body
            }
            Err(error) => return Err(error),
        };
        outcome(&response, "isValid")?;
        Ok(VerifyResponse(response))
    }

    pub async fn settle(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<SettleResponse, Error> {
        let body = prepare(request)?;
        let mut attempt = 1;
        loop {
            // Match Node: only a known pending settlement can be retried, using identical payment material.
            match self
                .call(
                    Method::POST,
                    "/v1/x402/settle",
                    Some(body.clone()),
                    0,
                    token,
                )
                .await
            {
                Ok(response) => {
                    outcome(&response, "success")?;
                    return Ok(SettleResponse(response));
                }
                Err(error)
                    if attempt < 5
                        && error.http_status == 409
                        && error.body["errorReason"] == "idempotency_pending" =>
                {
                    attempt += 1;
                    let seconds = error
                        .headers
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(5)
                        .min(5);
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => return Err(Error::new("CANCELLED", "operation cancelled")),
                        _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        retries: u8,
        token: &CancellationToken,
    ) -> Result<Value, Error> {
        self.0
            .request(method, path, body, HeaderMap::new(), retries, token)
            .await
    }
}

fn prepare(request: &VerifyRequest) -> Result<Value, Error> {
    let mut body: Value = serde_json::from_str(request.as_str()).map_err(|_| invalid("request"))?;
    if body["x402Version"] != 2 {
        return Err(invalid("request version"));
    }
    crate::facilitator_request(&body["paymentPayload"], &body["paymentRequirements"])?;
    body["paymentPayload"] = crate::identifier::ensure_identifier(&body["paymentPayload"])?;
    Ok(body)
}

fn outcome(response: &Value, field: &str) -> Result<(), Error> {
    if !response[field].is_boolean() {
        return Err(invalid("response outcome"));
    }
    Ok(())
}
