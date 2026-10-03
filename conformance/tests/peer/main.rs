#[path = "../adapter/transport.rs"]
mod transport;

use axum::{
    Router,
    body::Body,
    extract::State,
    response::{IntoResponse, Response},
    routing::get,
};
use http::{HeaderMap, StatusCode};
use inflow_core::{ClientOptions, Error};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn bad(message: &str) -> Error {
    Error::new("PEER_ERROR", message)
}
fn string<'a>(value: &'a Value, name: &str) -> std::result::Result<&'a str, Error> {
    value[name]
        .as_str()
        .ok_or_else(|| bad("missing peer setting"))
}
fn local(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("loopback endpoint required".into());
    }
    Ok(url)
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?)
}
fn output(value: Value) -> Result<()> {
    println!("{value}");
    io::stdout().flush()?;
    Ok(())
}

async fn buyer(s: &Value, options: ClientOptions) -> Result<()> {
    let token = CancellationToken::new();
    let target = local(string(s, "Target")?)?;
    let http = client()?;
    let response = if s["Protocol"] == "mpp" {
        let response = http
            .get(target.clone())
            .header("x-app-session", "test-only-session")
            .send()
            .await?;
        if response.status() != 402 {
            return Err("expected unpaid challenge".into());
        }
        let headers = response
            .headers()
            .get_all("www-authenticate")
            .iter()
            .map(|v| v.to_str())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let challenges = inflow_mpp::parse_challenges(&headers)?;
        let method = if s["Variant"] == "tempo" {
            "tempo"
        } else {
            "inflow"
        };
        let intent = if s["Variant"] == "subscription" {
            "subscription"
        } else {
            "charge"
        };
        let challenge = challenges
            .iter()
            .find(|c| c.method.as_str() == method && c.intent.as_str() == intent)
            .ok_or("missing selected challenge")?;
        let buyer = inflow_mpp_buyer::Buyer::new(options)?;
        let payment = buyer
            .prepare(
                challenge,
                inflow_mpp_buyer::PaymentOptions {
                    subscription_id: s["SubscriptionID"].as_str().map(str::to_owned),
                    ..Default::default()
                },
                &token,
            )
            .await?;
        let credential = payment
            .wait(inflow_mpp_buyer::WaitOptions {
                poll_interval: Duration::ZERO,
                timeout: Duration::from_secs(5),
            })
            .await?;
        let value = format!("Payment {}", inflow_mpp::encode_credential(&credential)?);
        http.get(target)
            .header(
                credential
                    .challenge
                    .header
                    .as_deref()
                    .unwrap_or("authorization"),
                value,
            )
            .header("x-app-session", "test-only-session")
            .send()
            .await?
    } else {
        let buyer = inflow_x402_buyer::Buyer::new(
            inflow_x402_buyer::BuyerOptions {
                client: options,
                ..Default::default()
            },
            &token,
        )
        .await?;
        inflow_x402_buyer::HttpBuyer::new(Some(buyer))?
            .execute(
                http.get(target)
                    .header("x-app-session", "test-only-session")
                    .build()?,
                Default::default(),
                inflow_x402_buyer::WaitOptions {
                    poll_interval: Duration::ZERO,
                    timeout: Duration::from_secs(5),
                },
                &token,
            )
            .await?
    };
    let status = response.status().as_u16();
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let receipt = response
        .headers()
        .get(if s["Protocol"] == "mpp" {
            "payment-receipt"
        } else {
            "payment-response"
        })
        .map(|v| -> Result<Value> {
            Ok(if s["Protocol"] == "mpp" {
                serde_json::to_value(inflow_mpp::decode_receipt(v.to_str()?)?)?
            } else {
                inflow_x402::decode(v.to_str()?)?
            })
        })
        .transpose()?;
    output(json!({"status":status,"cache":cache,"receipt":receipt,"body":response.text().await?}))
}

#[derive(Clone)]
struct Handler {
    platform: String,
    status: u16,
}
async fn protected(handler: Handler) -> Result<Response> {
    client()?
        .post(format!("{}/handler", handler.platform))
        .send()
        .await?
        .error_for_status()?;
    Ok(Response::builder()
        .status(handler.status)
        .header("content-type", "application/json")
        .body(Body::from("{\"paidResource\":true}"))?)
}
#[derive(Clone)]
struct MppState {
    offer: inflow_mpp_seller::Offer,
    handler: Handler,
}
async fn mpp_handler(State(state): State<MppState>, headers: HeaderMap) -> Response {
    let result: Result<Response> = async {
        let Some(header) = headers.get("authorization") else {
            return Ok((
                StatusCode::PAYMENT_REQUIRED,
                [(
                    "www-authenticate",
                    inflow_mpp::render_challenge(&state.offer.challenge(None)?)?,
                )],
            )
                .into_response());
        };
        let value = header
            .to_str()?
            .strip_prefix("Payment ")
            .ok_or("invalid Payment header")?;
        let credential = inflow_mpp::decode_credential(value)?;
        match state
            .offer
            .accept(&credential, None, &CancellationToken::new())
            .await
        {
            Ok(receipt) => {
                let mut response = protected(state.handler).await?;
                response.headers_mut().insert(
                    "payment-receipt",
                    inflow_mpp::encode(&serde_json::to_value(receipt)?)?.parse()?,
                );
                response
                    .headers_mut()
                    .insert("cache-control", "private".parse()?);
                Ok(response)
            }
            Err(error) if error.code == "MPP_PAYMENT_FAILED" => {
                Ok((StatusCode::PAYMENT_REQUIRED, error.message).into_response())
            }
            Err(error) => Err(error.into()),
        }
    }
    .await;
    result.unwrap_or_else(|error| {
        eprintln!("{error}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

async fn seller(s: &Value, options: ClientOptions) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let resource = format!("http://{}/paid", listener.local_addr()?);
    let token = CancellationToken::new();
    let handler = Handler {
        platform: string(s, "Platform")?.into(),
        status: s["HandlerStatus"]
            .as_u64()
            .ok_or("missing handler status")?
            .try_into()?,
    };
    let router = if s["Protocol"] == "mpp" {
        let seller = inflow_mpp_seller::Seller::new(
            options,
            inflow_mpp_seller::SellerOptions {
                realm: "interop".into(),
                secret_key: "test-only-binding-secret-at-least-32-bytes".into(),
            },
            &token,
        )
        .await?;
        let tempo = s["Variant"] == "tempo";
        let terms = if tempo {
            json!({"amount":"10000","currency":"0x20c0000000000000000000000000000000000000","recipient":"0x1111111111111111111111111111111111111111"})
        } else {
            json!({"amount":"0.01","currency":"USDC"})
        };
        let offer = seller.offer(
            if tempo {
                inflow_mpp_seller::Method::Tempo
            } else {
                inflow_mpp_seller::Method::Inflow
            },
            terms,
            Default::default(),
        )?;
        Router::new()
            .route("/paid", get(mpp_handler))
            .with_state(MppState { offer, handler })
    } else {
        let seller = inflow_x402_seller::Seller::new(options, &token).await?;
        let mut terms = inflow_x402_seller::OfferOptions::new("0.01 USDC");
        terms.schemes = Some(vec![string(s, "Variant")?.into()]);
        let route = seller.route(&terms, &token).await?;
        let layer = inflow_x402_axum::payment_layer(seller.facilitator(), route, &resource)?;
        Router::new().route(
            "/paid",
            get(move || {
                let handler = handler.clone();
                async move { protected(handler).await.expect("handler evidence failed") }
            })
            .route_layer(layer),
        )
    };
    let router = router.layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            if request.headers().contains_key("x-api-key")
                || request
                    .headers()
                    .get("x-app-session")
                    .and_then(|v| v.to_str().ok())
                    != Some("test-only-session")
            {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            next.run(request).await
        },
    ));
    output(json!({"url":resource}))?;
    axum::serve(listener, router).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if !std::env::args().any(|v| v == "--peer") {
        return Ok(());
    }
    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .ok_or("missing settings")??;
    let s: Value = serde_json::from_str(&line)?;
    local(string(&s, "Platform")?)?;
    let role = string(&s, "Role")?;
    let options = transport::options(
        &json!({"base_url":s["Platform"],"api_key":format!("test-only-{role}-key")}),
    )?;
    match role {
        "buyer" => buyer(&s, options).await,
        "seller" => seller(&s, options).await,
        _ => Err("invalid role".into()),
    }
}
