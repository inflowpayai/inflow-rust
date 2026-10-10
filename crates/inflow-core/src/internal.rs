//! Cross-crate implementation details, not a supported endpoint client API.

use crate::transport::{MAX_BODY_BYTES, ReqwestTransport};
use crate::{Authentication, ClientOptions, Error, Transport, TransportError, TransportRequest};
use http::{HeaderMap, HeaderValue, Method};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub use crate::lifecycle::{ApprovalCleanup, poll};

#[derive(Clone)]
pub struct HttpClient {
    options: ClientOptions,
    transport: Arc<dyn Transport>,
}

impl HttpClient {
    pub async fn payment_status(
        &self,
        id: &str,
        options: crate::PaymentStatusOptions,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        if id.is_empty() || matches!(id, "." | "..") {
            return Err(Error::invalid("transaction identifier"));
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
        self.request(
            Method::GET,
            &format!("/v1/transactions/{id}"),
            None,
            HeaderMap::new(),
            options.retries,
            cancellation,
        )
        .await
    }

    pub fn new(options: ClientOptions) -> Result<Self, Error> {
        if options.timeout.is_zero() {
            return Err(Error::invalid("timeout"));
        }
        if let Authentication::ApiKey(key) = &options.authentication {
            credential(key)?;
        }
        let transport = match &options.transport {
            Some(transport) => Arc::clone(transport),
            None => Arc::new(ReqwestTransport::new()?),
        };
        Ok(Self { options, transport })
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        headers: HeaderMap,
        retries: u8,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        if !path.starts_with('/') || path.starts_with("//") || path.contains(['#', '\\']) {
            return Err(Error::invalid("path"));
        }
        for name in [
            "authorization",
            "x-api-key",
            "cookie",
            "host",
            "proxy-authorization",
        ] {
            if headers.contains_key(name) {
                return Err(Error::invalid("header"));
            }
        }
        let body = body.map(|v| v.to_string().into_bytes());
        if body.as_ref().is_some_and(|b| b.len() > MAX_BODY_BYTES) {
            return Err(Error::new("BODY_TOO_LARGE", "request body exceeds 8 MiB"));
        }
        // Each protocol operation opts into retries; creating or cancelling an approval uses zero.
        let mut secrets = Vec::new();
        let mut attempt = 0;
        loop {
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancelled()),
                result = self.attempt(&method, path, body.as_deref(), &headers, &mut secrets, cancellation) => result,
            };
            let (result, retryable) = result?;
            match result {
                Ok(body) => return Ok(body),
                Err(mut error) if !retryable || attempt == retries.min(3) => {
                    if error.endpoint.is_empty() {
                        error.endpoint = crate::error::scrub(path, &secrets);
                    }
                    return Err(error);
                }
                Err(_) => {}
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(cancelled()),
                _ = tokio::time::sleep(Duration::from_millis((200 << attempt) + fastrand::u64(0..(50 << attempt)))) => {}
            }
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        method: &Method,
        path: &str,
        body: Option<&[u8]>,
        extra: &HeaderMap,
        secrets: &mut Vec<String>,
        cancellation: &CancellationToken,
    ) -> Result<(Result<Value, Error>, bool), Error> {
        let mut headers = extra.clone();
        headers.insert("accept", HeaderValue::from_static("application/json"));
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("inflow-rust/", env!("CARGO_PKG_VERSION"))),
        );
        if body.is_some() {
            headers.insert("content-type", HeaderValue::from_static("application/json"));
        }
        // Provider errors bypass transport retries, matching the Node client.
        match &self.options.authentication {
            Authentication::Anonymous => {}
            Authentication::ApiKey(key) => {
                secrets.push(key.clone());
                headers.insert("x-api-key", credential(key)?);
            }
            Authentication::ApiKeyProvider(provider) => {
                let key = provider.api_key().await?;
                headers.insert("x-api-key", credential(&key)?);
                secrets.push(key);
            }
            Authentication::Bearer(provider) => {
                let token = provider.access_token().await?;
                credential(&token)?;
                let mut value =
                    HeaderValue::from_str(&format!("Bearer {token}")).map_err(Error::invalid)?;
                value.set_sensitive(true);
                secrets.push(token);
                headers.insert("authorization", value);
            }
        }
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let request = TransportRequest {
            method: method.clone(),
            url: format!("{}{path}", self.options.environment.api_base_url()),
            headers,
            body: body.unwrap_or_default().to_vec(),
        };
        let response =
            tokio::time::timeout(self.options.timeout, self.transport.send(request)).await;
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(TransportError::BodyTooLarge)) => {
                return Ok((
                    Err(Error::new("BODY_TOO_LARGE", "response body exceeds 8 MiB")),
                    false,
                ));
            }
            Ok(Err(TransportError::Network)) => {
                return Ok((
                    Err(Error::new("NETWORK_ERROR", "network request failed")),
                    true,
                ));
            }
            Ok(Err(TransportError::Timeout)) | Err(_) => {
                return Ok((Err(Error::new("TIMEOUT", "request timed out")), true));
            }
        };
        if response.body.len() > MAX_BODY_BYTES {
            return Ok((
                Err(Error::new("BODY_TOO_LARGE", "response body exceeds 8 MiB")),
                false,
            ));
        }
        let body = if response.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&response.body).unwrap_or_else(|_| {
                Value::String(String::from_utf8_lossy(&response.body).into_owned())
            })
        };
        if (200..300).contains(&response.status) {
            return Ok((Ok(body), false));
        }
        let retryable = matches!(response.status, 429 | 502 | 503 | 504);
        Ok((
            Err(Error::response(
                path,
                response.status,
                response.headers,
                body,
                secrets,
            )),
            retryable,
        ))
    }
}

fn credential(value: &str) -> Result<HeaderValue, Error> {
    if value.is_empty() || !value.bytes().all(|b| b > 32 && b < 127) {
        return Err(Error::invalid("credential"));
    }
    let mut header = HeaderValue::from_str(value).map_err(Error::invalid)?;
    header.set_sensitive(true);
    Ok(header)
}

fn cancelled() -> Error {
    Error::new("CANCELLED", "operation cancelled")
}
