use super::*;
use axum::{
    body::{Bytes, to_bytes},
    routing::post,
};
use http::{Request, request::Parts};
use inflow_x402_axum::RequestPaymentLayer;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FailedBody;
impl http_body::Body for FailedBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Ready(Some(Err(std::io::Error::other("private read failure"))))
    }
}

fn platform() -> Platform {
    Platform {
        calls: Default::default(),
        valid: true,
        settled: true,
        status: 200,
    }
}

async fn unexpected_handler() -> &'static str {
    panic!("unpaid handler ran")
}

fn priced(amount: &str, upfront: bool) -> Route {
    let mut route = route();
    let mut offer = serde_json::to_value(&route.accepts[0]).unwrap();
    offer["amount"] = json!(amount);
    if upfront {
        offer["extra"]["paymentFlow"] = json!("upfront");
    }
    route.accepts[0] = serde_json::from_value(offer).unwrap();
    route.extensions.insert(
        "eip2612GasSponsoring".into(),
        json!({"info":{"version":"1"}}),
    );
    route
}

fn request(amount: &str, paid: Option<&str>, upfront: bool, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/report?{amount}"))
        .header("x-test", "retained");
    if let Some(paid) = paid {
        let payload = json!({"x402Version":2,"accepted":priced(paid, upfront).accepts[0],"payload":{"transactionId":"tx"}});
        request = request.header("payment-signature", inflow_x402::encode(&payload));
    }
    request.body(body).unwrap()
}

#[tokio::test]
async fn concurrent_async_prices_are_isolated_and_keep_extensions_and_customization() {
    #[derive(serde::Serialize)]
    struct Custom {
        enabled: bool,
    }
    impl x402_types::scheme::ExtensionKey for Custom {
        const EXTENSION_KEY: &'static str = "example";
    }
    let platform = platform();
    let facilitator = facilitator(&platform);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let layer = RequestPaymentLayer::new(move |parts: Parts, body: Option<Bytes>| {
        let facilitator = facilitator.clone();
        let barrier = barrier.clone();
        count.fetch_add(1, Ordering::SeqCst);
        async move {
            barrier.wait().await;
            assert!(body.is_none());
            assert_eq!(parts.method, "POST");
            assert_eq!(parts.headers["x-test"], "retained");
            payment_layer(
                facilitator,
                priced(parts.uri.query().unwrap(), false),
                "https://merchant.example/report",
            )
            .map(|layer| {
                layer
                    .with_description("Report".into())
                    .with_mime_type("text/plain".into())
                    .with_extension(Custom { enabled: true })
            })
        }
    });
    let app = Router::new().route(
        "/report",
        post(unexpected_handler).route_layer(layer.clone()),
    );
    let (a, b) = tokio::join!(
        app.clone()
            .oneshot(request("100", None, false, Body::empty())),
        app.oneshot(request("200", None, false, Body::empty()))
    );
    for (response, amount) in [(a, "100"), (b, "200")] {
        let response = response.unwrap();
        assert_eq!(response.status(), 402);
        let required =
            inflow_x402::decode(response.headers()["payment-required"].to_str().unwrap()).unwrap();
        assert_eq!(required["accepts"][0]["amount"], amount);
        assert_eq!(
            required["extensions"]["eip2612GasSponsoring"]["info"]["version"],
            "1"
        );
        assert_eq!(required["extensions"]["example"]["enabled"], true);
        assert_eq!(required["resource"]["description"], "Report");
        assert_eq!(required["resource"]["mimeType"], "text/plain");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        platform
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.url.ends_with("/supported"))
    );
}

#[tokio::test]
async fn resolved_price_is_used_for_verification_and_settlement_in_both_flows() {
    for upfront in [false, true] {
        for buffered in [false, true] {
            for (paid, expected) in [
                ("100", 200),
                ("200", 402),
                ("resolver-error", 500),
                ("large", 413),
                ("read-error", 400),
            ] {
                if !buffered && matches!(paid, "large" | "read-error") {
                    continue;
                }
                let platform = platform();
                let facilitator = facilitator(&platform);
                let count = Arc::new(AtomicUsize::new(0));
                let calls = count.clone();
                let mut layer =
                    RequestPaymentLayer::new(move |parts: Parts, body: Option<Bytes>| {
                        let facilitator = facilitator.clone();
                        calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if paid == "resolver-error" {
                                return Err(inflow_x402_seller::Error::new(
                                    "PRICING",
                                    "private secret",
                                ));
                            }
                            assert_eq!(body.as_deref(), buffered.then_some(b"100".as_slice()));
                            let amount = body
                                .as_deref()
                                .map(|bytes| std::str::from_utf8(bytes).unwrap())
                                .unwrap_or_else(|| parts.uri.query().unwrap());
                            payment_layer(
                                facilitator,
                                priced(amount, upfront),
                                "https://merchant.example/report",
                            )
                        }
                    });
                if buffered {
                    layer = layer.with_body_limit(3);
                }
                let observed = platform.calls.clone();
                let app = Router::new().route(
                    "/report",
                    post(move |request: Request<Body>| {
                        let observed = observed.clone();
                        async move {
                            assert_eq!(expected, 200);
                            assert_eq!(request.headers()["x-test"], "retained");
                            assert_eq!(to_bytes(request.into_body(), 3).await.unwrap(), "100");
                            let calls = observed.lock().unwrap();
                            assert_eq!(
                                calls
                                    .iter()
                                    .filter(|call| call.url.ends_with("/settle"))
                                    .count(),
                                usize::from(upfront)
                            );
                            "paid"
                        }
                    })
                    .route_layer(layer),
                );
                let body = match paid {
                    "large" => Body::from("1234"),
                    "read-error" => Body::new(FailedBody),
                    _ => Body::from("100"),
                };
                let response = app
                    .oneshot(request(
                        "100",
                        Some(if expected == 402 { "200" } else { "100" }),
                        upfront,
                        body,
                    ))
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected);
                assert_eq!(
                    response
                        .extensions()
                        .get::<inflow_x402_seller::Error>()
                        .map(|error| (error.code.as_str(), error.message.as_str())),
                    (paid == "resolver-error").then_some(("PRICING", "private secret"))
                );
                assert_eq!(
                    response.headers().contains_key("payment-response"),
                    expected == 200
                );
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    usize::from(!matches!(paid, "large" | "read-error"))
                );
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                assert!(
                    !std::str::from_utf8(&body)
                        .unwrap()
                        .contains("private secret")
                );
                let calls = platform.calls.lock().unwrap();
                let payment_calls: Vec<_> = calls
                    .iter()
                    .filter(|call| !call.url.ends_with("/supported"))
                    .collect();
                if expected == 200 {
                    assert_eq!(payment_calls.len(), if upfront { 1 } else { 2 });
                    for call in payment_calls {
                        let value: Value = serde_json::from_slice(&call.body).unwrap();
                        assert_eq!(value["paymentRequirements"]["amount"], "100");
                    }
                } else {
                    assert!(payment_calls.is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn invalid_routes_and_resolver_errors_cannot_bypass_payment() {
    for case in ["error", "empty", "upto", "mixed"] {
        let platform = platform();
        let facilitator = facilitator(&platform);
        let layer = RequestPaymentLayer::new(move |_: Parts, _: Option<Bytes>| {
            let facilitator = facilitator.clone();
            async move {
                if case == "error" {
                    return Err(inflow_x402_seller::Error::new("PRICING", "private secret"));
                }
                let mut route = priced("100", false);
                match case {
                    "empty" => route.accepts.clear(),
                    "upto" => route.accepts[0].scheme = "upto".into(),
                    _ => route.accepts.extend(priced("100", true).accepts),
                }
                payment_layer(facilitator, route, "https://merchant.example/report")
            }
        });
        let app = Router::new().route("/report", post(unexpected_handler).route_layer(layer));
        let response = app
            .oneshot(request("100", None, false, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.headers().contains_key("payment-required"));
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(!std::str::from_utf8(&body).unwrap().contains("private"));
        assert!(platform.calls.lock().unwrap().is_empty());
    }
}
