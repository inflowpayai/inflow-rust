#![doc = include_str!("../README.md")]

use axum::response::Response;
use http::{
    StatusCode,
    header::{CACHE_CONTROL, HeaderValue},
};
use inflow_x402_seller::{Error, Facilitator, Route};
use serde::Serialize;
use serde_json::Value;
use tower::{Layer, util::MapResponse};
use x402_axum::{X402Middleware, layer::X402LayerBuilder, paygate::StaticPriceTags};
use x402_types::{proto::v2::PriceTag, scheme::ExtensionKey};

type UpstreamLayer = X402LayerBuilder<StaticPriceTags<PriceTag>, Facilitator>;

#[derive(Clone)]
pub struct PaymentLayer(UpstreamLayer);

impl PaymentLayer {
    pub fn with_description(mut self, description: String) -> Self {
        self.0 = self.0.with_description(description);
        self
    }

    pub fn with_mime_type(mut self, mime: String) -> Self {
        self.0 = self.0.with_mime_type(mime);
        self
    }

    pub fn with_extension<T: ExtensionKey + Serialize>(mut self, extension: T) -> Self {
        self.0 = self.0.with_extension(extension);
        self
    }
}

impl<S> Layer<S> for PaymentLayer
where
    UpstreamLayer: Layer<S>,
{
    type Service = MapResponse<<UpstreamLayer as Layer<S>>::Service, fn(Response) -> Response>;

    fn layer(&self, inner: S) -> Self::Service {
        MapResponse::new(self.0.layer(inner), payment_cache_control)
    }
}

fn payment_cache_control(mut response: Response) -> Response {
    // Upstream owns payment execution; these headers match Node's payment cache policy.
    if matches!(
        response.status(),
        StatusCode::PAYMENT_REQUIRED | StatusCode::PRECONDITION_FAILED
    ) {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else if response.headers().contains_key("payment-response") {
        let private = response
            .headers()
            .get_all(CACHE_CONTROL)
            .iter()
            .any(|value| {
                let mut quoted = false;
                let mut escaped = false;
                value
                    .as_bytes()
                    .split(|byte| {
                        // Quoted extension values can contain commas and escaped quotes.
                        if escaped {
                            escaped = false;
                            return false;
                        }
                        match byte {
                            b'\\' if quoted => escaped = true,
                            b'"' => quoted = !quoted,
                            _ => {}
                        }
                        *byte == b',' && !quoted
                    })
                    .any(|directive| directive.trim_ascii().eq_ignore_ascii_case(b"private"))
            });
        if !private {
            response
                .headers_mut()
                .append(CACHE_CONTROL, HeaderValue::from_static("private"));
        }
    }
    response
}

/// Uses upstream verification, handler execution and settlement, in that order.
pub fn payment_layer(
    facilitator: Facilitator,
    route: Route,
    resource: &str,
) -> Result<PaymentLayer, Error> {
    let resource = url::Url::parse(resource)
        .map_err(|_| invalid("resource must be an absolute HTTP or HTTPS URL"))?;
    if !matches!(resource.scheme(), "http" | "https")
        || resource.host_str().is_none()
        || !resource.username().is_empty()
        || resource.password().is_some()
    {
        return Err(invalid(
            "resource must be an HTTP or HTTPS URL without credentials",
        ));
    }
    // Upstream settles the authorized ceiling; it cannot consume measured usage from the handler.
    if route.accepts.iter().any(|offer| offer.scheme == "upto") {
        return Err(invalid(
            "automatic metered routes are unsupported; use explicit verification and settlement",
        ));
    }
    let mut offers = route.accepts.into_iter().map(|requirements| PriceTag {
        requirements,
        enricher: None,
    });
    let first = offers
        .next()
        .ok_or_else(|| invalid("protected route requires at least one payment offer"))?;
    let mut layer = X402Middleware::from_facilitator(facilitator)
        .with_price_tag(first)
        .with_resource(resource);
    for offer in offers {
        layer = layer.with_price_tag(offer);
    }
    layer = layer.with_extension(Identifier(inflow_x402::identifier_declaration()));
    for (key, value) in route.extensions {
        layer = match key.as_str() {
            "eip2612GasSponsoring" => layer.with_extension(Eip2612(value)),
            "inflowEip7702GasSponsoring" => layer.with_extension(Eip7702(value)),
            _ => {
                return Err(invalid(
                    "attach custom extensions using the returned layer's with_extension method",
                ));
            }
        };
    }
    Ok(PaymentLayer(layer))
}

#[derive(Serialize)]
#[serde(transparent)]
struct Identifier(Value);
impl ExtensionKey for Identifier {
    const EXTENSION_KEY: &'static str = "payment-identifier";
}
#[derive(Serialize)]
#[serde(transparent)]
struct Eip2612(Value);
impl ExtensionKey for Eip2612 {
    const EXTENSION_KEY: &'static str = "eip2612GasSponsoring";
}
#[derive(Serialize)]
#[serde(transparent)]
struct Eip7702(Value);
impl ExtensionKey for Eip7702 {
    const EXTENSION_KEY: &'static str = "inflowEip7702GasSponsoring";
}

fn invalid(message: &str) -> Error {
    Error::new("INVALID_X402_ROUTE", message)
}
