use crate::{Error, Offer, PaymentBodyLayer, PaymentLayer};
use bytes::Bytes;
use http::{Request, Response, StatusCode, request::Parts};
use http_body_util::{BodyExt, Full, Limited};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tower::{Layer, Service, ServiceExt};

/// Resolves one offer per request without reading its body.
pub struct RequestPaymentLayer<F>(Arc<F>);

impl<F> RequestPaymentLayer<F> {
    pub fn new(resolve: F) -> Self {
        Self(Arc::new(resolve))
    }
}

impl<F> Clone for RequestPaymentLayer<F> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S, F> Layer<S> for RequestPaymentLayer<F> {
    type Service = RequestPaymentService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestPaymentService {
            inner,
            resolve: self.0.clone(),
        }
    }
}

pub struct RequestPaymentService<S, F> {
    inner: S,
    resolve: Arc<F>,
}

impl<S: Clone, F> Clone for RequestPaymentService<S, F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            resolve: self.resolve.clone(),
        }
    }
}

impl<S, F, Fut, ReqBody, ResBody> Service<Request<ReqBody>> for RequestPaymentService<S, F>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send,
    ReqBody: Send + 'static,
    ResBody: Default + From<Bytes> + Send + 'static,
    F: Fn(Parts) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Offer, Error>> + Send,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        let resolve = self.resolve.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let offer = match resolve(parts.clone()).await {
                Ok(offer) => offer,
                Err(error) => {
                    let mut response = failure(StatusCode::INTERNAL_SERVER_ERROR);
                    response.extensions_mut().insert(error);
                    return Ok(response);
                }
            };
            PaymentLayer::new(offer)
                .with_problem_details(ResBody::from)
                .layer(inner)
                .oneshot(Request::from_parts(parts, body))
                .await
        })
    }
}

/// Resolves one offer from bounded body bytes and binds payment to those bytes.
pub struct RequestPaymentBodyLayer<F> {
    resolve: Arc<F>,
    max_body_bytes: usize,
}

impl<F> RequestPaymentBodyLayer<F> {
    pub fn new(resolve: F) -> Self {
        Self {
            resolve: Arc::new(resolve),
            max_body_bytes: 2 * 1024 * 1024,
        }
    }

    pub fn max_body_bytes(mut self, limit: usize) -> Self {
        self.max_body_bytes = limit;
        self
    }
}

impl<F> Clone for RequestPaymentBodyLayer<F> {
    fn clone(&self) -> Self {
        Self {
            resolve: self.resolve.clone(),
            max_body_bytes: self.max_body_bytes,
        }
    }
}

impl<S, F> Layer<S> for RequestPaymentBodyLayer<F> {
    type Service = RequestPaymentBodyService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestPaymentBodyService {
            inner,
            options: self.clone(),
        }
    }
}

pub struct RequestPaymentBodyService<S, F> {
    inner: S,
    options: RequestPaymentBodyLayer<F>,
}

impl<S: Clone, F> Clone for RequestPaymentBodyService<S, F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            options: self.options.clone(),
        }
    }
}

impl<S, F, Fut, ReqBody, ResBody> Service<Request<ReqBody>> for RequestPaymentBodyService<S, F>
where
    S: Service<Request<Full<Bytes>>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send,
    S::Error: Send,
    ReqBody: http_body::Body<Data = Bytes> + Send + 'static,
    ReqBody::Error: std::fmt::Display + Send + Sync + 'static,
    ResBody: Default + From<Bytes> + Send + 'static,
    F: Fn(Parts, Bytes) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Offer, Error>> + Send,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let clone = self.inner.clone();
        let inner = std::mem::replace(&mut self.inner, clone);
        let options = self.options.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let body = match Limited::new(body.map_err(|e| e.to_string()), options.max_body_bytes)
                .collect()
                .await
            {
                Ok(body) => body.to_bytes(),
                Err(error) => {
                    return Ok(failure(if error.is::<http_body_util::LengthLimitError>() {
                        StatusCode::PAYLOAD_TOO_LARGE
                    } else {
                        StatusCode::BAD_REQUEST
                    }));
                }
            };
            let offer = match (options.resolve)(parts.clone(), body.clone()).await {
                Ok(offer) => offer,
                Err(error) => {
                    let mut response = failure(StatusCode::INTERNAL_SERVER_ERROR);
                    response.extensions_mut().insert(error);
                    return Ok(response);
                }
            };
            PaymentBodyLayer::new(offer)
                .max_body_bytes(options.max_body_bytes)
                .with_problem_details(ResBody::from)
                .layer(inner)
                .oneshot(Request::from_parts(parts, Full::new(body)))
                .await
        })
    }
}

fn failure<B: From<Bytes>>(status: StatusCode) -> Response<B> {
    let body = serde_json::json!({"type":"about:blank", "status":status.as_u16(), "title":status.canonical_reason()});
    let mut response = Response::new(B::from(Bytes::from(body.to_string())));
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
