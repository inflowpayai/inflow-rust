use crate::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri, request::Parts},
    response::{IntoResponse, Response},
    routing::get,
};
use inflow_mpp_seller::{
    ChallengeOptions, ClientOptions, Method, Offer, RequestPaymentLayer, Seller, SellerOptions,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub async fn router(
    options: ClientOptions,
    secret: String,
    token: &CancellationToken,
) -> Result<Router> {
    router_with_method(
        options,
        secret,
        Method::Inflow,
        json!({"amount":"0.01", "currency":"USDC", "methodDetails":{"rail":"balance"}}),
        token,
    )
    .await
}

pub async fn router_with_method(
    options: ClientOptions,
    secret: String,
    method: Method,
    request: serde_json::Value,
    token: &CancellationToken,
) -> Result<Router> {
    let seller = Seller::new(
        options,
        SellerOptions {
            realm: "127.0.0.1:3000".into(),
            secret_key: secret,
        },
        token,
    )
    .await?;
    let state = Pricing {
        seller,
        method,
        request,
    };
    state.offer(&Uri::from_static("/api/widgets"))?;
    let pricing = state.clone();
    let layer = RequestPaymentLayer::new(move |parts: Parts| {
        let result = pricing.offer(&parts.uri);
        async move { result }
    });
    Ok(Router::new()
        .route("/free", get(|| async { Json(json!({"ok":true})) }))
        .route("/api/widgets", get(widgets))
        .route(
            "/api/tower-widgets",
            get(|uri: Uri| async move { Json(widgets_body(&uri)) }).route_layer(layer),
        )
        .with_state(state))
}

#[derive(Clone)]
struct Pricing {
    seller: Seller,
    method: Method,
    request: serde_json::Value,
}

impl Pricing {
    fn offer(&self, uri: &Uri) -> std::result::Result<Offer, inflow_mpp_seller::Error> {
        let mut request = self.request.clone();
        if large_bundle(uri) {
            request["amount"] = json!(if request["currency"] == "USDC" {
                "0.02"
            } else {
                "2.50"
            });
        }
        self.seller.offer(
            self.method,
            request,
            ChallengeOptions {
                description: Some("Example widgets".into()),
                ..Default::default()
            },
        )
    }
}

fn large_bundle(uri: &Uri) -> bool {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .any(|(key, value)| key == "bundle" && value == "large")
}

fn widgets_body(uri: &Uri) -> serde_json::Value {
    json!({"widgets": if large_bundle(uri) { vec![1,2,3,4,5,6] } else { vec![1,2,3] }})
}

async fn widgets(State(pricing): State<Pricing>, uri: Uri, headers: HeaderMap) -> Response {
    let result = payment(&pricing, &headers, &uri).await;
    let mut response = match result {
        Ok(response) => response,
        // An uncertain settlement is not a fresh offer. Do not prompt another charge or deliver widgets.
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "Payment could not be confirmed. Check transactions before retrying.",
        )
            .into_response(),
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}

async fn payment(pricing: &Pricing, headers: &HeaderMap, uri: &Uri) -> Result<Response> {
    let offer = pricing.offer(uri)?;
    let Some(header) = headers.get("authorization") else {
        return payment_required(&offer);
    };
    if headers.get_all("authorization").iter().count() != 1 {
        return Ok((StatusCode::BAD_REQUEST, "Send one Payment credential.").into_response());
    }
    let encoded = header
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Payment"));
    let Some((_, encoded)) = encoded else {
        return Ok((StatusCode::BAD_REQUEST, "Expected Payment authorization.").into_response());
    };
    let credential = match inflow_mpp::decode_credential(encoded.trim()) {
        Ok(value) => value,
        Err(_) => {
            return Ok((StatusCode::BAD_REQUEST, "Invalid Payment credential.").into_response());
        }
    };
    let receipt = match offer
        .accept(&credential, None, &CancellationToken::new())
        .await
    {
        Ok(receipt) => receipt,
        Err(error) if error.code == "MPP_CREDENTIAL_MISMATCH" => return payment_required(&offer),
        Err(error) => return Err(error.into()),
    };
    let receipt = inflow_mpp::encode(&serde_json::to_value(receipt)?)?;
    // Fulfill only after accept has validated and settled. This route has no application login.
    Ok(([("payment-receipt", receipt)], Json(widgets_body(uri))).into_response())
}

fn payment_required(offer: &Offer) -> Result<Response> {
    let challenge = inflow_mpp::render_challenge(&offer.challenge(None)?)?;
    Ok((
        StatusCode::PAYMENT_REQUIRED,
        [("www-authenticate", challenge)],
    )
        .into_response())
}
