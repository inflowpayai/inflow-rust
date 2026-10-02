use http::{HeaderMap, Method};
use std::{future::Future, pin::Pin};

pub struct TransportRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub struct TransportResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    Network,
    Timeout,
    BodyTooLarge,
}

/// An attempt includes reading the complete response body. Dropping its future must stop the attempt.
pub trait Transport: Send + Sync {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>>;
}

pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct ReqwestTransport(reqwest::Client);

impl ReqwestTransport {
    pub(crate) fn new() -> Result<Self, crate::Error> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map(Self)
            .map_err(crate::Error::invalid)
    }
}

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let mut response = self
                .0
                .request(request.method, request.url)
                .headers(request.headers)
                .body(request.body)
                .send()
                .await
                .map_err(network_error)?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                if chunk.len() > MAX_BODY_BYTES - body.len() {
                    return Err(TransportError::BodyTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(TransportResponse {
                status,
                headers,
                body,
            })
        })
    }
}

fn network_error(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Network
    }
}

#[cfg(test)]
#[path = "tests/transport.rs"]
mod tests;
