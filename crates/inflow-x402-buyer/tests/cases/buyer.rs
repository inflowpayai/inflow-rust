use http::{HeaderMap, Method};
use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_x402_buyer::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Platform {
    responses: Mutex<VecDeque<(u16, Value)>>,
    requests: Mutex<Vec<TransportRequest>>,
    cancel_balances: Option<CancellationToken>,
}
impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        if request.url.ends_with("/v1/balances")
            && let Some(token) = &self.cancel_balances
        {
            token.cancel();
        }
        self.requests.lock().unwrap().push(request);
        let (status, body) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected platform call");
        Box::pin(async move {
            Ok(TransportResponse {
                status,
                body: body.to_string().into_bytes(),
                headers: HeaderMap::new(),
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn input_failures_do_not_create_payments_and_read_failures_do_not_erase_capabilities() {
    let (buyer, platform) = setup(vec![(200, json!({})), (204, Value::Null)]).await;
    let token = CancellationToken::new();
    assert!(buyer.refresh_supported(&token).await.is_err());
    assert!(
        buyer
            .supports(&required().accepts[0], &token)
            .await
            .unwrap()
    );
    let malformed: OriginalJson = serde_json::from_value(json!({"scheme":"balance"})).unwrap();
    assert!(buyer.supports(&malformed, &token).await.is_err());
    let mut r = required();
    r.accepts = vec![malformed];
    assert!(buyer.select(&r, &token).await.is_err());
    r.accepts.clear();
    assert!(buyer.select(&r, &token).await.unwrap().is_none());
    let mut other: Value = serde_json::to_value(required()).unwrap();
    other["accepts"][0]["extra"]["assetTransferMethod"] = json!("permit2");
    let other: PaymentRequired<OriginalJson> = serde_json::from_value(other).unwrap();
    assert_eq!(
        buyer
            .prepare(&other.accepts[0], &other, SignOptions::default(), &token)
            .await
            .err()
            .unwrap()
            .code,
        "X402_ADAPTER_ROUTING_ERROR"
    );
    let r = required();
    assert_eq!(
        buyer
            .prepare(
                &r.accepts[0],
                &r,
                SignOptions {
                    payment_id: Some("short".into()),
                    ..Default::default()
                },
                &token
            )
            .await
            .err()
            .unwrap()
            .code,
        "X402_PAYMENT_ID_FORMAT"
    );
    buyer.cancel_approval("manual").await.unwrap();
    token.cancel();
    assert!(buyer.supported(&token).await.is_err());
    assert_eq!(platform.requests.lock().unwrap().len(), 3);
    assert!(
        Buyer::new(
            BuyerOptions {
                client: ClientOptions {
                    timeout: Duration::ZERO,
                    ..Default::default()
                },
                ..Default::default()
            },
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_and_duplicate_balances_follow_node_last_valid_currency_selection() {
    for balances in [
        json!({}),
        json!({"balances":[{"currency":"USDC","available":"bad"},{"currency":"USDT","available":"-1"}]}),
        json!({"balances":[{"currency":"other","available":"10"},{"currency":"USDC","available":"1.x"}]}),
        json!({"balances":[{"currency":"USDC","available":"2"},{"currency":"USDC","available":"0"},{"currency":"USDT","available":"2"}]}),
    ] {
        let expected = if balances["balances"]
            .as_array()
            .is_some_and(|a| a.len() == 3)
        {
            1
        } else {
            0
        };
        let (buyer, _) = setup(vec![(200, balances)]).await;
        let r = required();
        assert_eq!(
            buyer
                .select(&r, &CancellationToken::new())
                .await
                .unwrap()
                .unwrap()
                .0
                .get(),
            r.accepts[expected].0.get()
        );
    }
    for (field, value) in [
        ("amount", json!("bad")),
        ("amount", json!("")),
        ("extra", json!({})),
    ] {
        let (buyer, _) = setup(vec![(
            200,
            json!({"balances":[{"currency":"USDT","available":"2"}]}),
        )])
        .await;
        let mut raw = serde_json::to_value(required()).unwrap();
        raw["accepts"][0][field] = value;
        let r = serde_json::from_value(raw).unwrap();
        assert!(buyer.select(&r, &CancellationToken::new()).await.is_ok());
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_balance_read_cannot_fall_through_to_signing() {
    let token = CancellationToken::new();
    let platform = Arc::new(Platform {
        cancel_balances: Some(token.clone()),
        responses: Mutex::new(
            vec![
                (
                    200,
                    json!({"kinds":[{"scheme":"balance","network":"inflow:1"}]}),
                ),
                (200, json!({"balances":[]})),
            ]
            .into(),
        ),
        ..Default::default()
    });
    let buyer = Buyer::new(
        BuyerOptions {
            client: ClientOptions {
                transport: Some(platform.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        &token,
    )
    .await
    .unwrap();
    assert_eq!(
        buyer.select(&required(), &token).await.unwrap_err().code,
        "X402_APPROVAL_CANCELLED"
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn malformed_creation_and_poll_errors_cancel_known_approvals_without_recreating() {
    for created in [
        json!({}),
        json!({"approvalId":""}),
        json!({"approvalId":"a"}),
    ] {
        let (buyer, platform) = setup(vec![(200, created.clone()), (204, Value::Null)]).await;
        let r = required();
        assert!(
            buyer
                .prepare(
                    &r.accepts[0],
                    &r,
                    SignOptions::default(),
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        tokio::task::yield_now().await;
        assert_eq!(
            platform.requests.lock().unwrap().len(),
            if created["approvalId"] == "a" { 3 } else { 2 }
        );
    }
    let (buyer, platform) = setup(vec![(503, json!({}))]).await;
    let r = required();
    assert!(
        buyer
            .prepare(
                &r.accepts[0],
                &r,
                SignOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 2);
    let (buyer, platform) = setup(vec![
        (
            200,
            json!({"approvalId":"a","transactionId":"t","approvalStatus":"APPROVED"}),
        ),
        (200, json!({"status":"INITIATED"})),
        (429, json!({})),
        (403, json!({})),
        (204, Value::Null),
    ])
    .await;
    let payment = buyer
        .prepare(
            &r.accepts[0],
            &r,
            SignOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(payment.status().await.unwrap()["status"], "INITIATED");
    assert_eq!(
        payment
            .wait(WaitOptions::default())
            .await
            .err()
            .unwrap()
            .http_status,
        403
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 6);
}

fn required() -> PaymentRequired<OriginalJson> {
    serde_json::from_value(json!({"x402Version":2,"resource":{"url":"https://service.example/data"},"accepts":[
        {"scheme":"balance","network":"inflow:1","asset":"USD","amount":"1000000000000000000","payTo":"seller","maxTimeoutSeconds":60,"extra":{"assetName":"USDC"}},
        {"scheme":"balance","network":"inflow:1","asset":"USD","amount":"1000000000000000000","payTo":"seller","maxTimeoutSeconds":60,"extra":{"assetName":"USDT"}}
    ]})).unwrap()
}
async fn setup(responses: Vec<(u16, Value)>) -> (Buyer, Arc<Platform>) {
    let platform = Arc::new(Platform {
        responses: Mutex::new(
            [(
                200,
                json!({"kinds":[{"scheme":"balance","network":"inflow:1"}]}),
            )]
            .into_iter()
            .chain(responses)
            .collect(),
        ),
        ..Default::default()
    });
    let buyer = Buyer::new(
        BuyerOptions {
            client: ClientOptions {
                authentication: Authentication::ApiKey("platform-only".into()),
                transport: Some(platform.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (buyer, platform)
}

#[tokio::test(start_paused = true)]
async fn fresh_balances_prefer_affordable_asset_without_mutating_offers() {
    let (buyer, platform) = setup(vec![
        (200,json!({"balances":[{"currency":"USDC","available":"0"},{"currency":"USDT","available":"1.0000000000000000009"}]})),
        (200,json!({"balances":[{"currency":"USDC","available":"2"},{"currency":"USDT","available":"0"}]})),
        (403,json!({})),
    ]).await;
    let required = required();
    let original = serde_json::to_value(&required).unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        buyer
            .select(&required, &token)
            .await
            .unwrap()
            .unwrap()
            .0
            .get(),
        required.accepts[1].0.get()
    );
    assert_eq!(
        buyer
            .select(&required, &token)
            .await
            .unwrap()
            .unwrap()
            .0
            .get(),
        required.accepts[0].0.get()
    );
    assert_eq!(
        buyer
            .select(&required, &token)
            .await
            .unwrap()
            .unwrap()
            .0
            .get(),
        required.accepts[0].0.get()
    );
    assert_eq!(serde_json::to_value(required).unwrap(), original);
    assert_eq!(platform.requests.lock().unwrap().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn capabilities_refresh_and_failed_refresh_preserves_snapshot() {
    let (buyer, platform) = setup(vec![
        (200, json!({"kinds":[]})),
        (403, json!({})),
        (
            200,
            json!({"kinds":[{"scheme":"balance","network":"inflow:1"}]}),
        ),
    ])
    .await;
    let token = CancellationToken::new();
    assert!(
        buyer
            .supports(&required().accepts[0], &token)
            .await
            .unwrap()
    );
    buyer.refresh_supported(&token).await.unwrap();
    assert!(
        !buyer
            .supports(&required().accepts[0], &token)
            .await
            .unwrap()
    );
    assert!(buyer.refresh_supported(&token).await.is_err());
    assert!(
        !buyer
            .supports(&required().accepts[0], &token)
            .await
            .unwrap()
    );
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(
        buyer
            .supports(&required().accepts[0], &token)
            .await
            .unwrap()
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn creation_polling_and_completion_preserve_server_payload() {
    let payload = json!({"x402Version":2,"payload":{"transactionId":"signed"},"accepted":{},"extensions":{"unknown":1}});
    let encoded = inflow_x402::encode(&payload);
    let (buyer,platform)=setup(vec![
        (200,json!({"approvalId":"approval","transactionId":"transaction","approvalStatus":"PENDING"})),
        (503,json!({})),
        (200,json!({"status":"COMPLETED"})),
        (200,json!({"status":"COMPLETED","encodedPayload":encoded,"paymentPayload":payload})),
    ]).await;
    let required = required();
    let options = SignOptions {
        payment_id: Some("pay_0123456789abcdef".into()),
        transaction_fields:
            json!({"serviceId":"service","accept":"override","resource":"override","x402Version":1})
                .as_object()
                .unwrap()
                .clone(),
    };
    let payment = buyer
        .prepare(
            &required.accepts[0],
            &required,
            options,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(payment.approval_id(), "approval");
    assert_eq!(payment.transaction_id(), "transaction");
    let signed = payment.wait(WaitOptions::default()).await.unwrap();
    assert_eq!(signed.encoded_payload, encoded);
    assert_eq!(signed.payment_payload, payload);
    tokio::task::yield_now().await;
    let requests = platform.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[1].method, Method::POST);
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(body["accept"].is_object());
    assert_eq!(body["x402Version"], 2);
    assert_eq!(body["remotePaymentId"], "pay_0123456789abcdef");
    assert_eq!(body["serviceId"], "service");
}

#[tokio::test(start_paused = true)]
async fn failure_timeout_cancel_and_drop_cleanup_without_retrying_creation() {
    for status in ["DECLINED", "EXPIRED", "GENERAL_ERROR", "INSUFFICIENT_FUNDS"] {
        let (buyer, platform) = setup(vec![
            (200, json!({"approvalId":"a","transactionId":"t"})),
            (200, json!({"status":status})),
            (204, Value::Null),
        ])
        .await;
        let required = required();
        let error = buyer
            .sign(
                &required.accepts[0],
                &required,
                SignOptions::default(),
                WaitOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "X402_APPROVAL_FAILED");
        assert_eq!(error.body["status"], status);
        tokio::task::yield_now().await;
        assert_eq!(platform.requests.lock().unwrap().len(), 4);
    }
    for mode in 0..3 {
        let (buyer, platform) = setup(vec![
            (200, json!({"approvalId":"a","transactionId":"t"})),
            (204, Value::Null),
        ])
        .await;
        let required = required();
        let payment = buyer
            .prepare(
                &required.accepts[0],
                &required,
                SignOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        if mode == 0 {
            payment.cancel().await.unwrap();
            payment.cancel().await.unwrap();
        } else if mode == 1 {
            payment.cancellation().cancel();
            assert_eq!(
                payment
                    .wait(WaitOptions::default())
                    .await
                    .err()
                    .unwrap()
                    .code,
                "X402_APPROVAL_CANCELLED"
            );
        } else {
            drop(payment);
        }
        tokio::task::yield_now().await;
        assert_eq!(platform.requests.lock().unwrap().len(), 3);
    }
    let (buyer, platform) = setup(vec![
        (200, json!({"approvalId":"a","transactionId":"t"})),
        (200, json!({"status":"INITIATED"})),
        (204, Value::Null),
    ])
    .await;
    let required = required();
    let error = buyer
        .sign(
            &required.accepts[0],
            &required,
            SignOptions::default(),
            WaitOptions {
                timeout: Duration::from_secs(1),
                ..Default::default()
            },
            &CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "X402_APPROVAL_TIMEOUT");
    assert_eq!(platform.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn automatic_http_uses_affordable_asset_and_retains_request_without_leaking_platform_key() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/resource?q=search",
        listener.local_addr().unwrap()
    );
    let mut required = required();
    required.resource.as_mut().unwrap().url = url.clone();
    let challenge = inflow_x402::encode(&serde_json::to_value(&required).unwrap());
    let signed = json!({"x402Version":2,"accepted":{},"payload":{"transactionId":"signed"}});
    let signature = inflow_x402::encode(&signed);
    let expected = signature.clone();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 2048];
            while !request.windows(4).any(|w| w == b"\r\n\r\n")
                || !request.ends_with(b"search body")
            {
                let size = socket.read(&mut bytes).await.unwrap();
                assert!(size > 0);
                request.extend_from_slice(&bytes[..size]);
            }
            let text = String::from_utf8(request).unwrap();
            assert!(text.starts_with("POST /resource?q=search HTTP/1.1"));
            assert!(text.contains("authorization: Bearer service-only"));
            assert!(!text.contains("platform-only"));
            if attempt == 0 {
                assert!(!text.contains("payment-signature:"));
                socket.write_all(format!("HTTP/1.1 402 Payment Required\r\nPayment-Required: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            } else {
                assert!(text.contains(&format!("payment-signature: {expected}")));
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let (buyer, platform) = setup(vec![
        (200,json!({"balances":[{"currency":"USDC","available":"0"},{"currency":"USDT","available":"10"}]})),
        (200,json!({"approvalId":"a","transactionId":"t"})),
        (200,json!({"status":"COMPLETED","encodedPayload":signature,"paymentPayload":signed})),
    ]).await;
    let http = HttpBuyer::new(Some(buyer)).unwrap();
    let request = reqwest::Client::new()
        .post(url)
        .bearer_auth("service-only")
        .body("search body")
        .build()
        .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        http.execute(
            request,
            SignOptions::default(),
            WaitOptions::default(),
            &CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.text().await.unwrap(), "ok");
    server.await.unwrap();
    let requests = platform.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let created: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(created["accept"]["extra"]["assetName"], "USDT");
    assert!(
        requests
            .iter()
            .all(|request| request.headers["x-api-key"] == "platform-only")
    );
}

#[tokio::test]
async fn streaming_request_is_rejected_before_platform_or_merchant_payment_work() {
    let (buyer, platform) = setup(vec![]).await;
    let http = HttpBuyer::new(Some(buyer)).unwrap();
    let body = reqwest::Body::wrap_stream(futures_util::stream::once(async {
        Ok::<_, std::io::Error>("stream")
    }));
    let request = reqwest::Client::new()
        .post("http://127.0.0.1:1/")
        .body(body)
        .build()
        .unwrap();
    let error = http
        .execute(
            request,
            SignOptions::default(),
            WaitOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "X402_REQUEST_NOT_REPLAYABLE");
    assert_eq!(platform.requests.lock().unwrap().len(), 1); // Constructor capability lookup only.
}

#[tokio::test]
async fn invalid_hosted_header_and_failed_approval_never_send_paid_retry() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    for failed_approval in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let challenge = inflow_x402::encode(&serde_json::to_value(required()).unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let mut received = Vec::new();
            while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                let size = socket.read(&mut buf).await.unwrap();
                assert!(size > 0);
                received.extend_from_slice(&buf[..size]);
            }
            socket.write_all(format!("HTTP/1.1 402 Payment Required\r\nPayment-Required: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut responses = vec![
            (200, json!({"balances":[]})),
            (200, json!({"approvalId":"a","transactionId":"t"})),
        ];
        if failed_approval {
            responses.extend([(200, json!({"status":"DECLINED"})), (204, Value::Null)]);
        } else {
            responses.push((
                200,
                json!({"encodedPayload":"invalid\nheader","paymentPayload":{}}),
            ));
        }
        let (buyer, platform) = setup(responses).await;
        let http = HttpBuyer::new(Some(buyer)).unwrap();
        let request = reqwest::Client::new().get(url).build().unwrap();
        let error = http
            .execute(
                request,
                SignOptions::default(),
                WaitOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.code,
            if failed_approval {
                "X402_APPROVAL_FAILED"
            } else {
                "X402_INVALID_PAYMENT"
            }
        );
        server.await.unwrap();
        assert_eq!(
            platform.requests.lock().unwrap().len(),
            if failed_approval { 5 } else { 4 }
        );
    }
}

#[tokio::test]
async fn single_offer_skips_balance_read_and_malformed_offer_does_not_fall_through() {
    let (buyer, platform) = setup(vec![]).await;
    let token = CancellationToken::new();
    let mut r = required();
    r.accepts.truncate(1);
    assert!(buyer.select(&r, &token).await.unwrap().is_some());
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
    r.accepts = vec![serde_json::from_value(json!({})).unwrap()];
    let http = HttpBuyer::new(Some(buyer)).unwrap();
    assert_eq!(
        http.payment(&r, SignOptions::default(), WaitOptions::default(), &token)
            .await
            .err()
            .unwrap()
            .code,
        "X402_INVALID_PAYMENT"
    );
    assert_eq!(platform.requests.lock().unwrap().len(), 1);
    let platform = Arc::new(Platform {
        responses: Mutex::new(vec![(200, json!({}))].into()),
        ..Default::default()
    });
    assert!(
        Buyer::new(
            BuyerOptions {
                client: ClientOptions {
                    transport: Some(platform),
                    ..Default::default()
                },
                ..Default::default()
            },
            &token
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn decimal_balance_boundaries_do_not_promote_an_unaffordable_offer() {
    for value in ["1.", "-0", "-2", "0.999999999999999999", "0.1", " 2.0 "] {
        let (buyer,_)=setup(vec![(200,json!({"balances":[{"currency":"USDC","available":value},{"currency":"USDT","available":"1"}]}))]).await;
        let r = required();
        let selected = buyer
            .select(&r, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        let expected = if value == " 2.0 " { 0 } else { 1 };
        assert_eq!(selected.0.get(), r.accepts[expected].0.get());
    }
}
