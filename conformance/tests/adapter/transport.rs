use crate::{bad, string};
use inflow_core::{
    Authentication, ClientOptions, Error, Transport, TransportError, TransportRequest,
    TransportResponse,
};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

struct Local {
    base: String,
    client: reqwest::Client,
    config: Option<Value>,
    supported: Option<Value>,
}

pub fn options(input: &Value) -> Result<ClientOptions, Error> {
    let fixtures = input.get("config").is_some();
    let base = if fixtures {
        String::new()
    } else {
        let url =
            url::Url::parse(string(input, "base_url")?).map_err(|_| bad("invalid loopback URL"))?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(bad("adapter requires loopback origin"));
        }
        url.origin().ascii_serialization()
    };
    Ok(ClientOptions {
        authentication: input["api_key"]
            .as_str()
            .map(|v| Authentication::ApiKey(v.into()))
            .unwrap_or(if fixtures {
                Authentication::ApiKey("test-only-seller-key".into())
            } else {
                Authentication::Anonymous
            }),
        transport: Some(Arc::new(Local {
            base,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .build()
                .map_err(|_| bad("HTTP client construction failed"))?,
            config: input.get("config").cloned(),
            supported: fixtures.then(|| {
                input
                    .get("supported")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"kinds":[],"extensions":[],"signers":{}}))
            }),
        })),
        ..Default::default()
    })
}

impl Transport for Local {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let path = request
                .url
                .strip_prefix("https://api.inflowpay.ai/")
                .expect("unexpected SDK destination");
            if self.config.is_some() {
                assert_eq!(request.method, http::Method::GET);
                let value = match path {
                    "v1/x402/config" => self.config.as_ref(),
                    "v1/x402/supported" => self.supported.as_ref(),
                    _ => panic!("unexpected fixture request"),
                }
                .expect("missing fixture configuration");
                return Ok(TransportResponse {
                    status: 200,
                    headers: http::HeaderMap::new(),
                    body: serde_json::to_vec(value).expect("JSON fixture"),
                });
            }
            let response = self
                .client
                .request(request.method, format!("{}/{path}", self.base))
                .headers(request.headers)
                .body(request.body)
                .send()
                .await
                .map_err(network)?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let mut response = response;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network)? {
                if body.len() + chunk.len() > 8 * 1024 * 1024 {
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
fn network(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Network
    }
}
