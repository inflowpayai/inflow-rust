use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_mpp::{Base64UrlJson, encode, render_challenge};
use inflow_mpp_buyer::*;
use mpp::client::ChargeClient;
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};

#[derive(Clone, Copy)]
enum Outcome {
    Ready,
    Rejected,
    Network,
    Pending,
    WrongChallenge,
    Hang,
    Malformed,
}

struct Platform {
    outcome: Outcome,
    requests: Mutex<Vec<TransportRequest>>,
    entered: Notify,
}

impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let cancelling = request.url.contains("/approvals/");
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            self.entered.notify_one();
            let (status, body) = if cancelling {
                (200, json!({}))
            } else {
                match self.outcome {
                    Outcome::Network => return Err(TransportError::Network),
                    Outcome::Hang => return std::future::pending().await,
                    Outcome::Rejected => (
                        403,
                        json!({"code":"PAYMENT_DENIED","detail":"buyer declined","transactionId":"tx-denied"}),
                    ),
                    Outcome::Pending => (
                        200,
                        json!({"state":"pending","transactionId":"tx-pending","approvalId":"approval","retryAfterSeconds":3600}),
                    ),
                    Outcome::Malformed => (200, json!({"state":"ready","credential":"bad"})),
                    Outcome::Ready | Outcome::WrongChallenge => {
                        let mut challenge = body["challenge"].clone();
                        if matches!(self.outcome, Outcome::WrongChallenge) {
                            challenge["id"] = json!("different");
                        }
                        let payload = if challenge["method"] == "card" {
                            json!({"encryptedPayload":"synthetic","network":"visa","panLastFour":"1234","panExpirationMonth":"12","panExpirationYear":"2030","extension":true})
                        } else {
                            json!({"proof":"synthetic","extension":true})
                        };
                        (
                            200,
                            json!({"state":"ready","credential":encode(&json!({"challenge":challenge,"payload":payload,"source":"did:example:buyer"})).unwrap()}),
                        )
                    }
                }
            };
            Ok(TransportResponse {
                status,
                headers: [(
                    "x-request-id".parse().unwrap(),
                    "request-1".parse().unwrap(),
                )]
                .into_iter()
                .collect(),
                body: body.to_string().into_bytes(),
            })
        })
    }
}

fn payment(outcome: Outcome) -> (HttpPayment, Arc<Platform>) {
    let platform = Arc::new(Platform {
        outcome,
        requests: Mutex::default(),
        entered: Notify::new(),
    });
    let buyer = Buyer::new(ClientOptions {
        authentication: Authentication::ApiKey("platform-secret".into()),
        transport: Some(platform.clone()),
        ..Default::default()
    })
    .unwrap();
    (
        HttpPayment::new(
            buyer,
            HttpPaymentMethod::Inflow {
                instrument_id: None,
            },
        ),
        platform,
    )
}

fn challenge(method: &str, id: &str) -> PaymentChallenge {
    let request = match method {
        "card" => {
            json!({"amount":"50","currency":"usd","recipient":"seller","methodDetails":{"acceptedNetworks":["visa"],"merchantName":"Test Shop","encryptionJwk":{"kty":"RSA","alg":"RSA-OAEP-256","use":"enc","kid":"key","n":"test","e":"AQAB"}}})
        }
        "tempo" => {
            json!({"amount":"1000000","currency":"0x20c0000000000000000000000000000000000000","recipient":"0x1111111111111111111111111111111111111111"})
        }
        _ => json!({"amount":"1","currency":"USDC"}),
    };
    PaymentChallenge::new(
        id,
        "shop",
        method,
        "charge",
        Base64UrlJson::from_value(&request).unwrap(),
    )
    .with_description("A useful purchase")
}

struct Resource {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Resource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Resource {
    async fn new(
        challenges: Vec<PaymentChallenge>,
        paid_status: u16,
        location: Option<String>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/resource", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await.unwrap());
                }
                let mut headers = String::from_utf8(bytes).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let paid = headers.lines().any(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("authorization: payment ")
                        || line
                            .to_ascii_lowercase()
                            .starts_with("payment-authorization: payment ")
                });
                headers.push_str(std::str::from_utf8(&body).unwrap());
                observed.lock().unwrap().push(headers);
                let challenge_headers: String = challenges
                    .iter()
                    .map(|c| format!("WWW-Authenticate: {}\r\n", render_challenge(c).unwrap()))
                    .collect();
                let (status, extra) = if paid {
                    (
                        paid_status,
                        if paid_status == 402 {
                            challenge_headers
                        } else {
                            String::new()
                        },
                    )
                } else if let Some(location) = &location {
                    (307, format!("Location: {location}\r\n"))
                } else {
                    (402, challenge_headers)
                };
                let receipt = if paid && status == 200 {
                    format!(
                        "Payment-Receipt: {}\r\n",
                        encode(&json!(inflow_mpp::Receipt::success(
                            "inflow",
                            "payment-reference"
                        )))
                        .unwrap()
                    )
                } else {
                    String::new()
                };
                stream.write_all(format!("HTTP/1.1 {status} Response\r\n{extra}{receipt}Content-Length: 9\r\nConnection: close\r\n\r\ndelivered").as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

fn http() -> ChargeClient {
    ChargeClient::new(reqwest::Client::builder()).unwrap()
}

async fn send(
    payment: &HttpPayment,
    resource: &Resource,
    accept: &str,
) -> Result<reqwest::Response, mpp::client::HttpError> {
    let client = http();
    tokio::time::timeout(
        Duration::from_secs(3),
        payment.send(
            &client,
            client
                .request(reqwest::Method::GET, &resource.url)
                .header("Accept-Payment", accept),
        ),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn native_http_preserves_complete_credential_and_application_authentication() {
    for alternate in [false, true] {
        let (payment, platform) = payment(Outcome::Ready);
        let mut challenge = challenge("inflow", "original");
        challenge.opaque = Some(Base64UrlJson::from_raw("eyJ4IjogMX0="));
        if alternate {
            challenge.header = Some("Payment-Authorization".into());
        }
        let resource = Resource::new(vec![challenge.clone()], 200, None).await;
        let client = http();
        let mut request = client
            .request(reqwest::Method::GET, &resource.url)
            .header("Accept-Payment", "inflow/charge");
        if alternate {
            request = request.bearer_auth("application-session");
        }
        let response = payment.send(&client, request).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "delivered");
        let calls = resource.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let name = if alternate {
            "payment-authorization"
        } else {
            "authorization"
        };
        let header = calls[1]
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim())
            })
            .unwrap();
        let credential =
            inflow_mpp::decode_credential(header.strip_prefix("Payment ").unwrap()).unwrap();
        assert_eq!(json!(credential.challenge), json!(challenge));
        assert_eq!(credential.source.as_deref(), Some("did:example:buyer"));
        assert_eq!(credential.payload["extension"], true);
        assert!(calls.iter().all(|r| !r.contains("platform-secret")));
        if alternate {
            assert!(
                calls
                    .iter()
                    .all(|r| r.contains("authorization: Bearer application-session"))
            );
        }
        let calls = platform.requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].headers["x-api-key"], "platform-secret");
        assert!(!calls[0].headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn failures_stop_native_payment_flow_without_falling_back_to_another_offer() {
    for outcome in [
        Outcome::Rejected,
        Outcome::Network,
        Outcome::Malformed,
        Outcome::WrongChallenge,
    ] {
        let (payment, platform) = payment(outcome);
        let failures = Arc::new(Mutex::new(Vec::new()));
        let seen = failures.clone();
        let payment = payment.with_error_handler(move |challenge, error| {
            seen.lock()
                .unwrap()
                .push((challenge.id.clone(), error.clone()))
        });
        let resource = Resource::new(
            vec![challenge("inflow", "first"), challenge("inflow", "second")],
            200,
            None,
        )
        .await;
        let error = send(&payment, &resource, "inflow/charge")
            .await
            .unwrap_err();
        assert_eq!(resource.requests.lock().unwrap().len(), 1);
        assert_eq!(platform.requests.lock().unwrap().len(), 1);
        let failures = failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "first");
        assert!(error.to_string().contains(&failures[0].1.code));
        if matches!(outcome, Outcome::Rejected) {
            assert_eq!(failures[0].1.http_status, 403);
            assert_eq!(failures[0].1.request_id.as_deref(), Some("request-1"));
            assert_eq!(failures[0].1.body["transactionId"], "tx-denied");
            assert!(error.to_string().contains("request-1"));
            assert!(error.to_string().contains("tx-denied"));
        }
    }
}

#[tokio::test]
async fn no_error_callback_is_required_and_paid_handler_failure_is_not_another_payment() {
    let (failed, platform) = payment(Outcome::Rejected);
    let resource = Resource::new(vec![challenge("inflow", "first")], 200, None).await;
    assert!(
        send(&failed, &resource, "inflow/charge")
            .await
            .unwrap_err()
            .to_string()
            .contains("PAYMENT_DENIED")
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
    let (paid, platform) = payment(Outcome::Ready);
    let resource = Resource::new(vec![challenge("inflow", "first")], 500, None).await;
    assert_eq!(
        send(&paid, &resource, "inflow/charge")
            .await
            .unwrap()
            .status(),
        500
    );
    assert_eq!(resource.requests.lock().unwrap().len(), 2);
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn repeated_paid_challenge_does_not_create_a_second_payment() {
    let (payment, platform) = payment(Outcome::Ready);
    let resource = Resource::new(vec![challenge("inflow", "same")], 402, None).await;
    assert_eq!(
        send(&payment, &resource, "inflow/charge")
            .await
            .unwrap()
            .status(),
        402
    );
    assert_eq!(resource.requests.lock().unwrap().len(), 2);
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn upstream_replays_the_request_body_and_returns_the_receipt_unchanged() {
    let (payment, platform) = payment(Outcome::Ready);
    let resource = Resource::new(vec![challenge("inflow", "post")], 200, None).await;
    let client = http();
    let response = payment
        .send(
            &client,
            client
                .request(reqwest::Method::POST, &resource.url)
                .header("Accept-Payment", "inflow/charge")
                .header("Content-Type", "application/json")
                .body(r#"{"selection":"black shirt"}"#),
        )
        .await
        .unwrap();
    let receipt =
        inflow_mpp::decode_receipt(response.headers()["payment-receipt"].to_str().unwrap())
            .unwrap();
    assert_eq!(receipt.reference, "payment-reference");
    let calls = resource.requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|r| r.starts_with("POST /resource ")
                && r.ends_with(r#"{"selection":"black shirt"}"#))
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_a_pending_http_payment_cleans_up_only_its_known_approval() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (payment, platform) = payment(Outcome::Pending);
        let resource = Resource::new(vec![challenge("inflow", "pending")], 200, None).await;
        let sending = tokio::spawn(async move { send(&payment, &resource, "inflow/charge").await });
        platform.entered.notified().await;
        // The creation response is synchronous after notification; await the next task turn
        // so the payment owns its approval cleanup before dropping the request future.
        tokio::task::yield_now().await;
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
        platform.entered.notified().await;
        let calls = platform.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].url.ends_with("/approvals/approval/cancel"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_error_callbacks_keep_each_requests_challenge_and_error() {
    let (base, platform) = payment(Outcome::Rejected);
    let first = Arc::new(Mutex::new(Vec::new()));
    let second = Arc::new(Mutex::new(Vec::new()));
    let a = first.clone();
    let b = second.clone();
    let one = base.clone().with_error_handler(move |challenge, error| {
        a.lock()
            .unwrap()
            .push((challenge.id.clone(), error.code.clone()))
    });
    let two = base.with_error_handler(move |challenge, error| {
        b.lock()
            .unwrap()
            .push((challenge.id.clone(), error.code.clone()))
    });
    let r1 = Resource::new(vec![challenge("inflow", "one")], 200, None).await;
    let r2 = Resource::new(vec![challenge("inflow", "two")], 200, None).await;
    let (a, b) = tokio::join!(
        send(&one, &r1, "inflow/charge"),
        send(&two, &r2, "inflow/charge")
    );
    assert!(a.is_err() && b.is_err());
    assert_eq!(
        *first.lock().unwrap(),
        vec![("one".into(), "PAYMENT_DENIED".into())]
    );
    assert_eq!(
        *second.lock().unwrap(),
        vec![("two".into(), "PAYMENT_DENIED".into())]
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn caller_preferences_choose_offers_and_missing_opt_in_never_creates_a_payment() {
    let (payment, platform) = payment(Outcome::Ready);
    let resource = Resource::new(
        vec![
            challenge("card", "unsupported"),
            challenge("inflow", "chosen"),
        ],
        200,
        None,
    )
    .await;
    let client = http();
    assert_eq!(
        payment
            .send(&client, client.request(reqwest::Method::GET, &resource.url))
            .await
            .unwrap()
            .status(),
        402
    );
    assert_eq!(
        send(&payment, &resource, "inflow/charge;q=0")
            .await
            .unwrap()
            .status(),
        402
    );
    assert!(platform.requests.lock().unwrap().is_empty());
    assert_eq!(
        send(&payment, &resource, "card/charge, inflow/charge;q=0.5")
            .await
            .unwrap()
            .status(),
        200
    );
    let calls = platform.requests.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(body["challenge"]["id"], "chosen");
}

#[tokio::test]
async fn redirects_are_not_followed_and_subscriptions_remain_manual() {
    let (payment, platform) = payment(Outcome::Ready);
    let target = Resource::new(vec![challenge("inflow", "target")], 200, None).await;
    let redirect = Resource::new(vec![], 200, Some(target.url.clone())).await;
    assert_eq!(
        send(&payment, &redirect, "inflow/charge")
            .await
            .unwrap()
            .status(),
        307
    );
    assert!(target.requests.lock().unwrap().is_empty());
    let mut subscription = challenge("inflow", "subscription");
    subscription.intent = "subscription".into();
    let resource = Resource::new(vec![subscription], 200, None).await;
    assert!(
        send(&payment, &resource, "inflow/subscription")
            .await
            .is_err()
    );
    assert!(platform.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_is_request_local_and_pending_timeout_preserves_transaction_identifier() {
    let (base, platform) = payment(Outcome::Ready);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let first = base.clone().with_cancellation(cancelled);
    let resource = Resource::new(vec![challenge("inflow", "first")], 200, None).await;
    assert!(
        send(&first, &resource, "inflow/charge")
            .await
            .unwrap_err()
            .to_string()
            .contains("MPP_PAYMENT_CANCELLED")
    );
    assert!(platform.requests.lock().unwrap().is_empty());
    assert!(send(&base, &resource, "inflow/charge").await.is_ok());
    for pending in [false, true] {
        let (payment, platform) = payment(if pending {
            Outcome::Pending
        } else {
            Outcome::Hang
        });
        let token = CancellationToken::new();
        let payment = payment.with_cancellation(token.clone());
        let resource = Resource::new(vec![challenge("inflow", "wait")], 200, None).await;
        let sending = send(&payment, &resource, "inflow/charge");
        let cancel = async {
            platform.entered.notified().await;
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (result, ()) = tokio::join!(sending, cancel);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("MPP_PAYMENT_CANCELLED")
        );
        let requests = platform.requests.lock().unwrap();
        assert_eq!(requests.len(), if pending { 2 } else { 1 });
        if pending {
            assert!(requests[1].url.contains("/approvals/"));
        }
    }
    let (payment, platform) = payment(Outcome::Pending);
    let payment = payment.with_wait_options(WaitOptions {
        poll_interval: Duration::ZERO,
        timeout: Duration::from_millis(10),
    });
    let resource = Resource::new(vec![challenge("inflow", "timeout")], 200, None).await;
    let error = send(&payment, &resource, "inflow/charge")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("MPP_PAYMENT_TIMEOUT"));
    assert!(error.to_string().contains("tx-pending"));
    assert_eq!(platform.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn per_request_method_options_do_not_mutate_other_requests() {
    let (base, platform) = payment(Outcome::Ready);
    let first = base
        .clone()
        .with_instrument_id(Some("12345678-1234-1234-1234-123456789abc".into()))
        .unwrap();
    let second = configured(&platform, HttpPaymentMethod::Tempo, MethodLimits::default());
    let first_resource = Resource::new(vec![challenge("inflow", "first")], 200, None).await;
    let second_resource = Resource::new(vec![challenge("tempo", "second")], 200, None).await;
    let (a, b) = tokio::join!(
        send(&first, &first_resource, "inflow/charge"),
        send(&second, &second_resource, "tempo/charge")
    );
    assert_eq!(a.unwrap().status(), 200);
    assert_eq!(b.unwrap().status(), 200);
    assert_eq!(
        send(&base, &first_resource, "inflow/charge")
            .await
            .unwrap()
            .status(),
        200
    );
    let calls = platform.requests.lock().unwrap();
    let bodies: Vec<Value> = calls
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b["options"].get("instrumentId").is_some())
            .count(),
        1
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|b| b["challenge"]["method"] == "tempo")
            .count(),
        1
    );
}

#[tokio::test]
async fn card_options_and_invalid_selectors_use_existing_buyer_validation() {
    let (base, platform) = payment(Outcome::Ready);
    let invalid = base
        .clone()
        .with_instrument_id(Some("invalid".into()))
        .unwrap();
    let resource = Resource::new(vec![challenge("inflow", "bad-option")], 200, None).await;
    assert!(send(&invalid, &resource, "inflow/charge").await.is_err());
    assert!(platform.requests.lock().unwrap().is_empty());
    let card = configured(
        &platform,
        HttpPaymentMethod::Card(CardPaymentOptions {
            merchant: Merchant {
                name: "Test Shop".into(),
                url: "https://shop.example".into(),
                country_code: "US".into(),
            },
            instrument_id: None,
        }),
        MethodLimits::default(),
    );
    let resource = Resource::new(vec![challenge("card", "card")], 200, None).await;
    assert_eq!(
        send(&card, &resource, "card/charge")
            .await
            .unwrap()
            .status(),
        200
    );
    {
        let calls = platform.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(body["options"]["merchant"]["name"], "Test Shop");
    }
    let card = card
        .with_instrument_id(Some("12345678-1234-1234-1234-123456789abc".into()))
        .unwrap();
    assert_eq!(
        send(&card, &resource, "card/charge")
            .await
            .unwrap()
            .status(),
        200
    );
    let body: Value = serde_json::from_slice(&platform.requests.lock().unwrap()[1].body).unwrap();
    assert_eq!(
        body["options"]["instrumentId"],
        "12345678-1234-1234-1234-123456789abc"
    );
}

fn configured(
    platform: &Arc<Platform>,
    method: HttpPaymentMethod,
    limits: MethodLimits,
) -> HttpPayment {
    HttpPayment::new_with_limits(
        Buyer::new(ClientOptions {
            authentication: Authentication::ApiKey("platform-secret".into()),
            transport: Some(platform.clone()),
            ..Default::default()
        })
        .unwrap(),
        method,
        limits,
    )
    .unwrap()
}

#[tokio::test]
async fn card_preparation_failure_never_sends_a_paid_retry() {
    let (_, platform) = payment(Outcome::Rejected);
    let payment = configured(
        &platform,
        HttpPaymentMethod::Card(CardPaymentOptions {
            merchant: Merchant {
                name: "Test Shop".into(),
                url: "https://shop.example".into(),
                country_code: "US".into(),
            },
            instrument_id: None,
        }),
        MethodLimits::default(),
    );
    let resource = Resource::new(vec![challenge("card", "rejected-card")], 200, None).await;
    let error = send(&payment, &resource, "card/charge").await.unwrap_err();
    assert!(error.to_string().contains("PAYMENT_DENIED"));
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
    assert_eq!(resource.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn card_limits_compare_dollars_and_cents_before_creating_payment() {
    for (dollars, cents, amount, allowed) in [
        (Some("0.50"), None, "50", true),
        (Some("0.50"), None, "51", false),
        (None, Some("50"), "50", true),
        (None, Some("50"), "51", false),
        (Some("0.50"), Some("49"), "50", false),
        (Some("0"), None, "1", false),
    ] {
        let (_, platform) = payment(Outcome::Ready);
        let payment = configured(
            &platform,
            HttpPaymentMethod::Card(CardPaymentOptions {
                merchant: Merchant {
                    name: "Test Shop".into(),
                    url: "https://shop.example".into(),
                    country_code: "US".into(),
                },
                instrument_id: None,
            }),
            MethodLimits {
                max_amount: dollars.map(str::to_owned),
                max_atomic_amount: cents.map(str::to_owned),
                ..Default::default()
            },
        );
        let mut offer = challenge("card", "limited-card");
        let mut request: Value = offer.request.decode().unwrap();
        request["amount"] = json!(amount);
        offer.request = Base64UrlJson::from_value(&request).unwrap();
        let resource = Resource::new(vec![offer], 200, None).await;
        let result = send(&payment, &resource, "card/charge").await;
        if allowed {
            assert_eq!(result.unwrap().status(), 200);
            assert_eq!(platform.requests.lock().unwrap().len(), 1);
            assert_eq!(resource.requests.lock().unwrap().len(), 2);
        } else {
            assert!(result.is_err());
            assert!(platform.requests.lock().unwrap().is_empty());
            assert_eq!(resource.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn limits_filter_native_offers_and_survive_request_overrides() {
    let (_, platform) = payment(Outcome::Ready);
    let limited = configured(
        &platform,
        HttpPaymentMethod::Inflow {
            instrument_id: None,
        },
        MethodLimits {
            max_amount: Some("0.50".into()),
            currencies: Some(vec!["USDC".into()]),
            ..Default::default()
        },
    );
    let expensive = challenge("inflow", "expensive");
    let mut affordable = challenge("inflow", "affordable");
    affordable.request =
        Base64UrlJson::from_value(&json!({"amount":"0.50","currency":"USDC"})).unwrap();
    let resource = Resource::new(vec![expensive.clone(), affordable], 200, None).await;
    let selected = limited
        .clone()
        .with_instrument_id(Some("12345678-1234-1234-1234-123456789abc".into()))
        .unwrap();
    assert_eq!(
        send(&selected, &resource, "inflow/charge")
            .await
            .unwrap()
            .status(),
        200
    );
    let body: Value = serde_json::from_slice(&platform.requests.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["challenge"]["id"], "affordable");
    let rejected = Resource::new(vec![expensive], 200, None).await;
    let _ = send(
        &selected.with_instrument_id(None).unwrap(),
        &rejected,
        "inflow/charge",
    )
    .await;
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
    let tempo = configured(&platform, HttpPaymentMethod::Tempo, MethodLimits::default());
    assert!(tempo.with_instrument_id(None).is_err());
}
