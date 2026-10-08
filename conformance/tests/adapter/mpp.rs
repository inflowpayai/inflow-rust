use crate::{bad, read, string, transport, wire};
use inflow_core::Error;
use inflow_mpp::{Base64UrlJson, Credential, PaymentChallenge};
use inflow_mpp_seller::{ChallengeOptions, Method, Seller, SellerOptions};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SECRET: &str = "test-only-signed-fixture-secret-at-least-32-bytes";
const EXPIRES: &str = "2099-01-01T00:00:00Z";

pub fn sign(value: Value) -> Result<Value, Error> {
    let challenge = PaymentChallenge::with_secret_key_full(
        SECRET,
        string(&value, "realm")?,
        string(&value, "method")?,
        string(&value, "intent")?,
        Base64UrlJson::from_raw(string(&value, "request")?),
        Some(EXPIRES),
        None,
        None,
        None,
        None,
    );
    Ok(json!({"id":challenge.id,"expires":challenge.expires}))
}

pub async fn execute(op: &str, input: &Value) -> Result<Value, Error> {
    match op {
        "mpp.core.encode" => return wire(inflow_mpp::encode(&input["value"])?),
        "mpp.core.decode" => return inflow_mpp::decode(string(input, "value")?),
        "mpp.core.decode-credential" => {
            return wire(inflow_mpp::decode_credential(string(input, "value")?)?);
        }
        "mpp.core.decode-receipt" => {
            return wire(inflow_mpp::decode_receipt(string(input, "value")?)?);
        }
        "mpp.core.parse-challenges" => {
            let headers: Vec<String> = if let Some(value) = input["headers"].as_str() {
                vec![value.into()]
            } else {
                read(input["headers"].clone())?
            };
            return wire(inflow_mpp::parse_challenges(
                &headers.iter().map(String::as_str).collect::<Vec<_>>(),
            )?);
        }
        _ => {}
    }
    let token = CancellationToken::new();
    if op == "mpp.buyer.payment-status" {
        let buyer = inflow_mpp_buyer::Buyer::new(transport::options(input)?)?;
        let mut values = Vec::new();
        for _ in 0..input["reads"].as_u64().unwrap_or(1) {
            values.push(
                buyer
                    .get_payment_status(
                        string(input, "transaction_id")?,
                        inflow_core::PaymentStatusOptions {
                            retries: input["retries"].as_u64().unwrap_or(0) as u8,
                        },
                        &token,
                    )
                    .await?,
            );
        }
        return Ok(json!(values));
    }
    if matches!(op, "mpp.buyer.fulfil" | "mpp.buyer.cancel") {
        use inflow_mpp_buyer::{Buyer, PaymentOptions, WaitOptions};
        let buyer = Buyer::new(transport::options(input)?)?;
        let payment = if input["challenge"]["method"] == "card" {
            let context = &input["context"];
            buyer
                .prepare_card(
                    &read(input["challenge"].clone())?,
                    inflow_mpp_buyer::CardPaymentOptions {
                        merchant: inflow_mpp_buyer::Merchant {
                            name: context["merchant"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .into(),
                            url: context["merchant"]["url"]
                                .as_str()
                                .unwrap_or_default()
                                .into(),
                            country_code: context["merchant"]["countryCode"]
                                .as_str()
                                .unwrap_or_default()
                                .into(),
                        },
                        instrument_id: context["instrumentId"].as_str().map(str::to_owned),
                    },
                    &token,
                )
                .await?
        } else {
            buyer
                .prepare(
                    &read(input["challenge"].clone())?,
                    PaymentOptions {
                        instrument_id: input["context"]["instrumentId"].as_str().map(str::to_owned),
                        subscription_id: input["context"]["subscriptionId"]
                            .as_str()
                            .map(str::to_owned),
                    },
                    &token,
                )
                .await?
        };
        if op == "mpp.buyer.cancel" {
            let _ = payment.cancel().await;
        }
        return wire(
            payment
                .wait(WaitOptions {
                    poll_interval: Duration::ZERO,
                    timeout: Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(5000)),
                })
                .await?,
        );
    }
    if !matches!(
        op,
        "mpp.seller.prepare"
            | "mpp.seller.validate"
            | "mpp.seller.verify"
            | "mpp.seller.route-binding"
    ) {
        return Err(bad("unknown MPP operation"));
    }
    let intent = input
        .get("credential")
        .map(|v| &v["challenge"]["intent"])
        .unwrap_or(&input["intent"]);
    if intent != "charge" {
        return Err(bad("Seller adapter supports charge only"));
    }
    let seller = Seller::new(
        transport::options(input)?,
        SellerOptions {
            realm: "seller.example".into(),
            secret_key: SECRET.into(),
        },
        &token,
    )
    .await?;
    let name = input
        .get("credential")
        .map(|v| &v["challenge"]["method"])
        .unwrap_or(&input["method"])
        .as_str()
        .ok_or_else(|| bad("missing method"))?;
    let method = match name {
        "inflow" => Method::Inflow,
        "tempo" => Method::Tempo,
        "stripe" => Method::Stripe,
        "card" => Method::Card,
        _ => return Err(bad("unknown method")),
    };
    let credential: Option<Credential> = input.get("credential").cloned().map(read).transpose()?;
    let mut request = match &credential {
        Some(c) => inflow_mpp::decode(c.challenge.request.raw())?,
        None => input["request"].clone(),
    };
    if credential.is_some() && matches!(method, Method::Stripe | Method::Card) {
        let cents = string(&request, "amount")?
            .parse::<u64>()
            .map_err(|_| bad("fixture wire amount"))?;
        request["amount"] = json!(format!("{}.{:02}", cents / 100, cents % 100));
        if let Some(metadata) = request["methodDetails"].get("metadata").cloned() {
            request["metadata"] = metadata;
        }
        if let Some(billing) = request["methodDetails"].get("billingRequired").cloned() {
            request["billingRequired"] = billing;
        }
    }
    let offer = seller.offer(method, request, ChallengeOptions::default())?;
    match op {
        "mpp.seller.prepare" => Ok(offer.request().clone()),
        "mpp.seller.validate" => wire(
            offer
                .validate(
                    credential
                        .as_ref()
                        .ok_or_else(|| bad("missing credential"))?,
                    None,
                    &token,
                )
                .await?,
        ),
        "mpp.seller.verify" => wire(
            offer
                .accept(
                    credential
                        .as_ref()
                        .ok_or_else(|| bad("missing credential"))?,
                    None,
                    &token,
                )
                .await?,
        ),
        _ => {
            let credential = read(
                json!({"challenge":offer.challenge(None)?,"payload":input["credential_payload"],"source":input["source"]}),
            )?;
            let replacement = seller.offer(
                method,
                input["replacement_request"].clone(),
                ChallengeOptions::default(),
            )?;
            protected_route(replacement, credential).await
        }
    }
}

async fn protected_route(
    offer: inflow_mpp_seller::Offer,
    credential: Credential,
) -> Result<Value, Error> {
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    async fn handler(
        State(offer): State<inflow_mpp_seller::Offer>,
        headers: HeaderMap,
    ) -> StatusCode {
        let encoded = headers
            .get("authorization")
            .expect("test authorization")
            .to_str()
            .expect("ASCII header")
            .strip_prefix("Payment ")
            .expect("Payment scheme");
        let credential = inflow_mpp::decode_credential(encoded).expect("test credential");
        match offer
            .accept(&credential, None, &CancellationToken::new())
            .await
        {
            Ok(_) => StatusCode::OK,
            Err(error) if error.code == "MPP_CREDENTIAL_MISMATCH" => StatusCode::PAYMENT_REQUIRED,
            Err(error) => panic!("unexpected protected-route failure: {error}"),
        }
    }
    let app = Router::new()
        .route("/resource", get(handler))
        .with_state(offer);
    let request = Request::builder()
        .uri("/resource")
        .header(
            "authorization",
            format!("Payment {}", inflow_mpp::encode_credential(&credential)?),
        )
        .body(Body::empty())
        .map_err(|_| bad("cannot create test resource request"))?;
    let response = app.oneshot(request).await.expect("infallible router");
    Ok(json!({"status":response.status().as_u16()}))
}
