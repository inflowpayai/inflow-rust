//! Endpoint primitives for the Buyer and Seller crates, not an integrator endpoint API.

use crate::{Error, FacilitatorResult, SettleResponse, VerifyRequest, VerifyResponse, invalid};
use http::{HeaderMap, Method};
use inflow_core::{
    ClientOptions,
    internal::{ApprovalCleanup, HttpClient, HttpResponse},
};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct X402Client(HttpClient);

impl X402Client {
    pub async fn payment_status(
        &self,
        id: &str,
        options: inflow_core::PaymentStatusOptions,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.0.payment_status(id, options, cancellation).await
    }

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
        Ok(self.verify_with_metadata(request, token).await?.response)
    }

    pub async fn verify_with_metadata(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<FacilitatorResult<VerifyResponse>, Error> {
        let body = prepare(request)?;
        let response = match self
            .0
            .request_with_response(
                Method::POST,
                "/v1/x402/verify",
                Some(body),
                HeaderMap::new(),
                0,
                token,
            )
            .await
        {
            Ok(response) => response,
            Err(error)
                if error.http_status == 412
                    && error.body["isValid"] == false
                    && error.body["invalidReason"] == "permit2_allowance_required" =>
            {
                HttpResponse {
                    body: *error.body,
                    headers: *error.headers,
                }
            }
            Err(error) => return Err(error),
        };
        outcome(&response.body, "isValid")?;
        Ok(FacilitatorResult {
            extension_responses: extension_responses(&response.headers),
            response: VerifyResponse(response.body),
        })
    }

    pub async fn settle(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<SettleResponse, Error> {
        Ok(self.settle_with_metadata(request, token).await?.response)
    }

    pub async fn settle_with_metadata(
        &self,
        request: &VerifyRequest,
        token: &CancellationToken,
    ) -> Result<FacilitatorResult<SettleResponse>, Error> {
        let body = prepare(request)?;
        let mut attempt = 1;
        loop {
            // Match Node: only a known pending settlement can be retried, using identical payment material.
            match self
                .0
                .request_with_response(
                    Method::POST,
                    "/v1/x402/settle",
                    Some(body.clone()),
                    HeaderMap::new(),
                    0,
                    token,
                )
                .await
            {
                Ok(response) => {
                    outcome(&response.body, "success")?;
                    return Ok(FacilitatorResult {
                        extension_responses: extension_responses(&response.headers),
                        response: SettleResponse(response.body),
                    });
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

fn extension_responses(headers: &HeaderMap) -> Option<serde_json::Map<String, Value>> {
    crate::decode(headers.get("extension-responses")?.to_str().ok()?)
        .ok()?
        .as_object()
        .cloned()
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
