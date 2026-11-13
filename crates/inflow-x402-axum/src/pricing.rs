use crate::PaymentLayer;
use axum::{
    body::{Body, Bytes, to_bytes},
    response::Response,
};
use http::{Request, StatusCode, request::Parts};
use inflow_x402_seller::Error;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tower::{Layer, Service, ServiceExt};

/// Resolves a concrete payment layer once per request, before payment processing.
pub struct RequestPaymentLayer<F> {
    resolve: Arc<F>,
    body_limit: Option<usize>,
}

impl<F> RequestPaymentLayer<F> {
    pub fn new(resolve: F) -> Self {
        Self {
            resolve: Arc::new(resolve),
            body_limit: None,
        }
    }

    /// Buffers at most `limit` bytes for pricing and restores them for the handler.
    pub fn with_body_limit(mut self, limit: usize) -> Self {
        self.body_limit = Some(limit);
        self
    }
}

impl<F> Clone for RequestPaymentLayer<F> {
    fn clone(&self) -> Self {
        Self {
            resolve: self.resolve.clone(),
            body_limit: self.body_limit,
        }
    }
}

impl<S, F> Layer<S> for RequestPaymentLayer<F> {
    type Service = RequestPaymentService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestPaymentService {
            inner,
            options: self.clone(),
        }
    }
}

pub struct RequestPaymentService<S, F> {
    inner: S,
    options: RequestPaymentLayer<F>,
}

impl<S: Clone, F> Clone for RequestPaymentService<S, F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            options: self.options.clone(),
        }
    }
}

impl<S, F, Fut> Service<Request<Body>> for RequestPaymentService<S, F>
where
    S: Service<Request<Body>, Response = Response, Error = std::convert::Infallible>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send,
    S::Error: Send + 'static,
    F: Fn(Parts, Option<Bytes>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<PaymentLayer, Error>> + Send,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        let options = self.options.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let (body, pricing_body) = if let Some(limit) = options.body_limit {
                match to_bytes(body, limit).await {
                    Ok(bytes) => (Body::from(bytes.clone()), Some(bytes)),
                    Err(error) => {
                        use std::error::Error;
                        let status = if error
                            .source()
                            .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
                        {
                            StatusCode::PAYLOAD_TOO_LARGE
                        } else {
                            StatusCode::BAD_REQUEST
                        };
                        return Ok(failure(status));
                    }
                }
            } else {
                (body, None)
            };
            let layer = match (options.resolve)(parts.clone(), pricing_body).await {
                Ok(layer) => layer,
                Err(error) => {
                    let mut response = failure(StatusCode::INTERNAL_SERVER_ERROR);
                    response.extensions_mut().insert(error);
                    return Ok(response);
                }
            };
            layer
                .layer(inner)
                .oneshot(Request::from_parts(parts, body))
                .await
        })
    }
}

fn failure(status: StatusCode) -> Response {
    let body = serde_json::json!({"type":"about:blank", "status":status.as_u16(), "title":status.canonical_reason()});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        http::HeaderValue::from_static("application/problem+json"),
    );
    response
        .headers_mut()
        .insert("cache-control", http::HeaderValue::from_static("no-store"));
    response
}
