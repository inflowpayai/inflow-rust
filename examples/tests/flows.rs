use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use inflow_core::{ClientOptions, Transport, TransportError, TransportRequest, TransportResponse};
use inflow_examples::{mpp_buyer, mpp_seller, x402_buyer, x402_seller};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

// Only the InFlow platform is scripted. Real SDKs, signed MPP challenges, Axum middleware,
// merchant HTTP requests, and receipt handling execute on both sides of each exchange.
#[derive(Default)]
struct Platform {
    calls: Mutex<Vec<TransportRequest>>,
    accepted: Mutex<Value>,
    mode: &'static str,
    cancellation: CancellationToken,
}
impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<TransportResponse, TransportError>> + Send + '_,
        >,
    > {
        Box::pin(async move {
            assert!(request.url.starts_with("https://sandbox.inflowpay.ai/"));
            assert_eq!(request.headers["x-api-key"], "platform-only-key");
            let path = url::Url::parse(&request.url).unwrap().path().to_owned();
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            self.calls.lock().unwrap().push(request);
            let mut status = 200;
            let result = match path.as_str() {
                "/v1/mpp/config" => {
                    if self.mode == "card" {
                        json!({"sellerId":"11111111-1111-4111-8111-111111111111","supportedMethods":[{"id":"card","supportedCurrencies":["USD"],"supportedIntents":["charge"],"methodDetails":{"recipient":"merchant","merchantName":"Shop","acceptedNetworks":["visa"],"encryptionJwk":{"kty":"RSA","alg":"RSA-OAEP-256","use":"enc","kid":"key","n":"abc","e":"AQAB"}}}]})
                    } else {
                        json!({"sellerId":"11111111-1111-4111-8111-111111111111","featureFlags":{"idempotencyKeyEnabled":true},"supportedMethods":[{"id":"inflow","methodDetails":{"currencyRails":{"USDC":{"rail":"balance"}},"intentCurrencyRails":{"charge":{"USDC":[{"rail":"balance"}]}}}}]})
                    }
                }
                "/v1/x402/config" => {
                    json!({"assets":[],"wallets":[],"paymentMethods":[{"scheme":"balance","network":"inflow:1","payTo":"seller","decimals":18}],"supported":[]})
                }
                "/v1/x402/supported" | "/v1/transactions/x402-supported" => {
                    json!({"kinds":[{"x402Version":2,"scheme":"balance","network":"inflow:1"}],"extensions":[],"signers":{}})
                }
                "/v1/transactions/mpp" => {
                    if self.mode == "pending" {
                        json!({"state":"pending","approvalId":"approval","transactionId":"transaction","retryAfterSeconds":0})
                    } else {
                        let payload = if self.mode == "card" {
                            json!({"encryptedPayload":"synthetic","network":"visa","panLastFour":"1234","panExpirationMonth":"12","panExpirationYear":"2030"})
                        } else {
                            json!({"proof":"synthetic"})
                        };
                        let credential = json!({"challenge":body["challenge"],"payload":payload});
                        json!({"state":"ready","credential":inflow_mpp::encode(&credential).unwrap()})
                    }
                }
                "/v1/transactions/x402" => {
                    *self.accepted.lock().unwrap() = body["accept"].clone();
                    json!({"approvalId":"approval","transactionId":"transaction","approvalStatus":"APPROVED"})
                }
                "/v1/transactions/transaction/mpp" | "/v1/transactions/transaction/x402"
                    if self.mode == "pending" =>
                {
                    self.cancellation.cancel();
                    std::future::pending().await
                }
                "/v1/transactions/transaction/x402" => {
                    let payload = json!({"x402Version":2,"accepted":*self.accepted.lock().unwrap(),"payload":{"transactionId":"transaction"}});
                    json!({"paymentPayload":payload,"encodedPayload":inflow_x402::encode(&payload)})
                }
                "/v1/approvals/approval/cancel" => {
                    status = 204;
                    Value::Null
                }
                "/v1/mpp/validate" if self.mode == "platform-failure" => {
                    status = 403;
                    json!({"errors":[{"code":"DENIED","message":"denied"}]})
                }
                "/v1/mpp/validate" => {
                    let c = &body["credential"];
                    let mut result = json!({"success":true,"credential":c,"challenge":c["challenge"],"request":inflow_mpp::decode(c["challenge"]["request"].as_str().unwrap()).unwrap(),"method":c["challenge"]["method"],"intent":"charge","details":{}});
                    if let Some(source) = c.get("source") {
                        result["source"] = source.clone();
                    }
                    result
                }
                "/v1/mpp/broadcast" if self.mode == "settlement-failure" => {
                    json!({"problem":{"detail":"Settlement unavailable","status":402}})
                }
                "/v1/mpp/broadcast" => {
                    json!({"receipt":{"method":body["credential"]["challenge"]["method"],"challengeId":body["credential"]["challenge"]["id"],"reference":"transaction","status":"success","timestamp":"2026-10-03T00:00:00Z"}})
                }
                "/v1/x402/verify" => {
                    json!({"isValid":self.mode != "platform-failure","payer":"buyer","invalidReason":"declined"})
                }
                "/v1/x402/settle" => {
                    json!({"success":self.mode != "settlement-failure","transaction":"transaction","network":"inflow:1","errorReason":"declined"})
                }
                _ => panic!("unexpected platform request: {path}"),
            };
            Ok(TransportResponse {
                status,
                headers: HeaderMap::new(),
                body: serde_json::to_vec(&result).unwrap(),
            })
        })
    }
}
fn setup(mode: &'static str) -> (ClientOptions, Arc<Platform>) {
    let platform = Arc::new(Platform {
        mode,
        ..Default::default()
    });
    let mut options = inflow_examples::sandbox("platform-only-key".into());
    options.transport = Some(platform.clone());
    (options, platform)
}
async fn serve(app: Router) -> (url::Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/widgets", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn card_example_buyer_pays_example_seller_without_platform_key_leakage() {
    let (options, platform) = setup("card");
    let token = CancellationToken::new();
    let app = mpp_seller::router_with_method(
        options.clone(),
        "test-secret-at-least-32-characters".into(),
        inflow_mpp_seller::Method::Card,
        json!({"amount":"1.25"}),
        &token,
    )
    .await
    .unwrap()
    .layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            assert!(request.headers().get("x-api-key").is_none());
            next.run(request).await
        },
    ));
    let (url, server) = serve(app).await;
    let mut output = Vec::new();
    let result = mpp_buyer::run_with_card(
        options,
        url,
        Some(inflow_mpp_buyer::CardPaymentOptions {
            merchant: inflow_mpp_buyer::Merchant {
                name: "Shop".into(),
                url: "https://shop.example".into(),
                country_code: "US".into(),
            },
            instrument_id: None,
        }),
        &token,
        &mut output,
    )
    .await;
    server.abort();
    result.unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("widgets"));
    assert!(!output.contains("synthetic"));
    assert!(!output.contains("platform-only-key"));
    let calls = platform.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|r| r.url.ends_with("/transactions/mpp"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|r| r.url.ends_with("/broadcast"))
            .count(),
        1
    );
}

#[tokio::test]
async fn both_buyers_pay_the_actual_example_sellers_and_preserve_failures() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        for mpp in [true, false] {
            for mode in [
                "success",
                "platform-failure",
                "settlement-failure",
                "pending",
            ] {
                let (options, platform) = setup(mode);
                let token = platform.cancellation.clone();
                let app = if mpp {
                    mpp_seller::router(
                        options.clone(),
                        "test-challenge-key-at-least-32-bytes".into(),
                        &token,
                    )
                    .await
                    .unwrap()
                } else {
                    x402_seller::router(
                        options.clone(),
                        "http://127.0.0.1:3001/api/widgets",
                        &token,
                    )
                    .await
                    .unwrap()
                };
                // A merchant must never receive the InFlow API key.
                let app = app.layer(axum::middleware::from_fn(
                    |request: axum::extract::Request, next: axum::middleware::Next| async move {
                        assert!(request.headers().get("x-api-key").is_none());
                        next.run(request).await
                    },
                ));
                let (url, server) = serve(app).await;
                let mut out = Vec::new();
                let result = if mpp {
                    mpp_buyer::run(options, url, &token, &mut out).await
                } else {
                    x402_buyer::run(options, url, &token, &mut out).await
                };
                server.abort();
                assert_eq!(
                    result.is_ok(),
                    mode == "success",
                    "{mpp} {mode}: {result:?}; {}",
                    String::from_utf8_lossy(&out)
                );
                let output = String::from_utf8(out).unwrap();
                assert!(output.contains("Approval:"));
                assert!(!output.contains("synthetic"));
                assert!(!output.contains("platform-only-key"));
                if mode == "success" {
                    assert!(output.contains("Receipt:"));
                    assert!(output.contains("widgets"));
                }
                let calls = platform.calls.lock().unwrap();
                assert_eq!(
                    calls.iter().filter(|r| r.url.ends_with("/cancel")).count(),
                    usize::from(mode == "pending")
                );
                assert_eq!(
                    calls
                        .iter()
                        .filter(|r| r.url.ends_with("/transactions/mpp")
                            || r.url.ends_with("/transactions/x402"))
                        .count(),
                    1
                );
                if mode == "platform-failure" || mode == "pending" {
                    assert!(
                        !calls
                            .iter()
                            .any(|r| r.url.ends_with("/broadcast") || r.url.ends_with("/settle"))
                    );
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn free_redirect_http_errors_and_malformed_challenges_never_create_a_payment() {
    for mpp in [true, false] {
        for status in [200, 307, 401, 402, 503] {
            let (options, platform) = setup("success");
            let app = Router::new().route(
                "/api/widgets",
                get(move || async move {
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [("location", "http://127.0.0.1:1/never")],
                        "resource",
                    )
                }),
            );
            let (url, server) = serve(app).await;
            let mut out = Vec::new();
            let token = CancellationToken::new();
            let result = if mpp {
                mpp_buyer::run(options, url, &token, &mut out).await
            } else {
                x402_buyer::run(options, url, &token, &mut out).await
            };
            server.abort();
            assert_eq!(result.is_ok(), status == 200);
            assert!(platform.calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn seller_free_wrong_method_and_malformed_credential_paths() {
    use tower::ServiceExt;
    for mpp in [true, false] {
        let (options, platform) = setup("success");
        let app = if mpp {
            mpp_seller::router(
                options,
                "test-challenge-key-at-least-32-bytes".into(),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        } else {
            x402_seller::router(
                options,
                "http://127.0.0.1:3001/api/widgets",
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        };
        for (path, method, status) in [("/free", "GET", 200), ("/api/widgets", "POST", 405)] {
            let response = app
                .clone()
                .oneshot(
                    http::Request::builder()
                        .uri(path)
                        .method(method)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
        }
        if mpp {
            for headers in [
                vec!["Bearer session"],
                vec!["Payment invalid"],
                vec!["Payment one", "Payment two"],
            ] {
                let mut request = http::Request::builder().uri("/api/widgets");
                for header in headers {
                    request = request.header("authorization", header);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), 400);
                assert_eq!(response.headers()["cache-control"], "no-store");
                assert!(response.headers().get("www-authenticate").is_none());
            }
        }
        assert!(
            platform
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.url.ends_with("/config") || r.url.ends_with("/supported"))
        );
    }
}

#[tokio::test]
async fn paid_redirect_repeated_challenge_and_missing_receipt_never_start_another_payment() {
    for mpp in [true, false] {
        for status in [200, 307, 402] {
            let (options, platform) = setup("success");
            let token = CancellationToken::new();
            let app = if mpp {
                mpp_seller::router(
                    options.clone(),
                    "test-challenge-key-at-least-32-bytes".into(),
                    &token,
                )
                .await
                .unwrap()
            } else {
                x402_seller::router(options.clone(), "http://127.0.0.1:3001/api/widgets", &token)
                    .await
                    .unwrap()
            };
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let location = format!("http://{}/never", target.local_addr().unwrap());
            let app = app.layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let location = location.clone();
                    async move {
                        if request.headers().contains_key("authorization")
                            || request.headers().contains_key("payment-signature")
                        {
                            (
                                StatusCode::from_u16(status).unwrap(),
                                [("location", location)],
                                "uncertain resource result",
                            )
                                .into_response()
                        } else {
                            next.run(request).await
                        }
                    }
                },
            ));
            let (url, server) = serve(app).await;
            let mut out = Vec::new();
            let result = if mpp {
                mpp_buyer::run(options, url, &token, &mut out).await
            } else {
                x402_buyer::run(options, url, &token, &mut out).await
            };
            server.abort();
            assert!(result.is_err());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), target.accept())
                    .await
                    .is_err()
            );
            let calls = platform.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|r| r.url.ends_with("/transactions/mpp")
                        || r.url.ends_with("/transactions/x402"))
                    .count(),
                1
            );
            assert!(!calls.iter().any(|r| r.url.ends_with("/cancel")));
        }
    }
}

#[tokio::test]
async fn receipts_and_bounded_body_reading_do_not_confuse_delivery_with_payment() {
    for (header, encoded, paid, status, ok) in [
        ("payment-receipt", None, false, 200, true),
        ("payment-receipt", None, true, 200, false),
        ("payment-receipt", Some("bad".into()), true, 200, false),
        (
            "payment-response",
            Some(inflow_x402::encode(&json!({"success":false}))),
            true,
            200,
            false,
        ),
        (
            "payment-response",
            Some(inflow_x402::encode(&json!({"success":true}))),
            true,
            402,
            false,
        ),
    ] {
        let app = Router::new().route(
            "/api/widgets",
            get(move || {
                let encoded = encoded.clone();
                async move {
                    let mut response =
                        (StatusCode::from_u16(status).unwrap(), "result").into_response();
                    if let Some(encoded) = encoded {
                        response
                            .headers_mut()
                            .insert(header, encoded.parse().unwrap());
                    }
                    response
                }
            }),
        );
        let (url, server) = serve(app).await;
        let response = inflow_examples::http_client()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        assert_eq!(
            inflow_examples::finish(response, header, paid, &mut Vec::new())
                .await
                .is_ok(),
            ok
        );
        server.abort();
    }
    let (url, server) = serve(Router::new().route(
        "/api/widgets",
        get(|| async { vec![b'x'; 1024 * 1024 + 1] }),
    ))
    .await;
    let response = inflow_examples::http_client()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap();
    assert!(
        inflow_examples::finish(response, "payment-response", false, &mut Vec::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    server.abort();
}

#[tokio::test]
async fn interrupt_waits_for_operation_cleanup_and_send_observes_cancellation() {
    let token = CancellationToken::new();
    let cleaned = std::sync::atomic::AtomicBool::new(false);
    let operation = async {
        token.cancelled().await;
        tokio::task::yield_now().await;
        cleaned.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    };
    inflow_examples::interruptible(&token, operation, async { Ok(()) })
        .await
        .unwrap();
    assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        inflow_examples::send(
            inflow_examples::http_client()
                .unwrap()
                .get("http://127.0.0.1:1/"),
            &token
        )
        .await
        .is_err()
    );
    assert!(
        inflow_examples::interruptible(
            &token,
            std::future::pending::<inflow_examples::Result<()>>(),
            async { Err(std::io::Error::other("signal unavailable")) }
        )
        .await
        .is_err()
    );
    inflow_examples::interruptible(&token, async { Ok(()) }, std::future::pending())
        .await
        .unwrap();
}

#[test]
fn configuration_rejects_missing_secrets_and_unsafe_urls() {
    assert!(inflow_examples::required("INFLOW_EXAMPLE_TEST_ABSENT_KEY").is_err());
    for value in [
        "not a url",
        "file:///secret",
        "https://user:secret@example.com/",
    ] {
        assert!(inflow_examples::merchant_url(value).is_err());
    }
    assert!(inflow_examples::merchant_url("https://example.com/resource").is_ok());
}
