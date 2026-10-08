use crate::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use inflow_mpp_seller::{ChallengeOptions, ClientOptions, Method, Offer, Seller, SellerOptions};
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
    let offer = seller.offer(
        method,
        request,
        ChallengeOptions {
            description: Some("Example widgets".into()),
            ..Default::default()
        },
    )?;
    Ok(Router::new()
        .route("/free", get(|| async { Json(json!({"ok":true})) }))
        .route("/api/widgets", get(widgets))
        .with_state(offer))
}

async fn widgets(State(offer): State<Offer>, headers: HeaderMap) -> Response {
    let result = payment(&offer, &headers).await;
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

async fn payment(offer: &Offer, headers: &HeaderMap) -> Result<Response> {
    let Some(header) = headers.get("authorization") else {
        let challenge = inflow_mpp::render_challenge(&offer.challenge(None)?)?;
        return Ok((
            StatusCode::PAYMENT_REQUIRED,
            [("www-authenticate", challenge)],
        )
            .into_response());
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
    let receipt = offer
        .accept(&credential, None, &CancellationToken::new())
        .await?;
    let receipt = inflow_mpp::encode(&serde_json::to_value(receipt)?)?;
    // Fulfill only after accept has validated and settled. This route has no application login.
    Ok((
        [("payment-receipt", receipt)],
        Json(json!({"widgets":[1,2,3]})),
    )
        .into_response())
}
