use axum::{Router, body::Body, routing::get};
use inflow_core::{ClientOptions, Transport, TransportError, TransportRequest, TransportResponse};
use inflow_x402_axum::payment_layer;
use inflow_x402_seller::{Facilitator, Route};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

#[derive(Clone)]
struct Platform {
    calls: Arc<Mutex<Vec<TransportRequest>>>,
    valid: bool,
    settled: bool,
    status: u16,
}
impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let body = if request.url.ends_with("/verify") {
                json!({"isValid":self.valid,"payer":"buyer","invalidReason":"declined"})
            } else if request.url.ends_with("/settle") {
                json!({"success":self.settled,"transaction":"tx","network":"inflow:1","errorReason":"declined"})
            } else {
                json!({"kinds":[],"extensions":[],"signers":{}})
            };
            self.calls.lock().unwrap().push(request);
            Ok(TransportResponse {
                status: self.status,
                headers: Default::default(),
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}
fn route() -> Route {
    Route {accepts:vec![serde_json::from_value(json!({"scheme":"balance","network":"inflow:1","amount":"100","asset":"USDC","payTo":"merchant","maxTimeoutSeconds":300,"extra":{"assetName":"USDC"}})).unwrap()],extensions:Default::default()}
}
fn facilitator(platform: &Platform) -> Facilitator {
    Facilitator::new(ClientOptions {
        transport: Some(Arc::new(platform.clone())),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn real_axum_route_verifies_executes_then_settles_only_success() {
    for (valid, settled, handler_status, platform_status, expected, handlers, settlements) in [
        (true, true, 200, 200, 200, 1, 1),
        (false, true, 200, 200, 402, 0, 0),
        (true, false, 200, 200, 402, 1, 1),
        (true, true, 400, 200, 400, 1, 0),
        (true, true, 412, 200, 412, 1, 0),
        (true, true, 500, 200, 500, 1, 0),
        (true, true, 200, 403, 402, 0, 0),
    ] {
        let platform = Platform {
            calls: Default::default(),
            valid,
            settled,
            status: platform_status,
        };
        let layer = payment_layer(
            facilitator(&platform),
            route(),
            "https://merchant.example/report",
        )
        .unwrap();
        let count = Arc::new(Mutex::new(0));
        let called = count.clone();
        let app = Router::new().route(
            "/report",
            get(move || {
                let called = called.clone();
                async move {
                    *called.lock().unwrap() += 1;
                    (
                        http::StatusCode::from_u16(handler_status).unwrap(),
                        [("cache-control", "public, max-age=60")],
                        "resource",
                    )
                }
            })
            .route_layer(layer),
        );
        let payload = json!({"x402Version":2,"accepted":route().accepts[0],"payload":{"transactionId":"tx"},"extensions":{"future":{"preserve":true}}});
        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/report")
                    .header("Payment-Signature", inflow_x402::encode(&payload))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        if expected == 200 {
            assert_eq!(response.headers()["cache-control"], "public, max-age=60");
            assert_eq!(
                response
                    .headers()
                    .get_all("cache-control")
                    .iter()
                    .next_back()
                    .unwrap(),
                "private"
            );
        } else if expected == 402 || expected == 412 {
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        assert_eq!(*count.lock().unwrap(), handlers);
        let calls = platform.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|r| r.url.ends_with("/settle")).count(),
            settlements
        );
        for request in calls.iter().filter(|r| !r.body.is_empty()) {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body["paymentPayload"]["extensions"]["future"]["preserve"],
                true
            );
        }
        assert_eq!(
            response.headers().contains_key("Payment-Response"),
            expected == 200
        );
    }
}

#[tokio::test]
async fn unpaid_tampered_and_wrong_method_requests_do_not_execute_handler() {
    let platform = Platform {
        calls: Default::default(),
        valid: true,
        settled: true,
        status: 200,
    };
    let mut r = route();
    r.accepts.push(r.accepts[0].clone());
    r.extensions.insert(
        "eip2612GasSponsoring".into(),
        json!({"info":{"version":"1"}}),
    );
    r.extensions.insert(
        "inflowEip7702GasSponsoring".into(),
        json!({"info":{"version":"1"}}),
    );
    let app = Router::new().route(
        "/report",
        get(|| async { "unexpected handler execution" }).route_layer(
            payment_layer(facilitator(&platform), r, "https://merchant.example/report").unwrap(),
        ),
    );
    let response = app
        .clone()
        .oneshot(
            http::Request::builder()
                .uri("/report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 402);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let required =
        inflow_x402::decode(response.headers()["Payment-Required"].to_str().unwrap()).unwrap();
    assert_eq!(
        required["extensions"]["payment-identifier"]["info"]["required"],
        false
    );
    assert_eq!(
        required["extensions"]["eip2612GasSponsoring"]["info"]["version"],
        "1"
    );
    for field in ["amount", "asset", "payTo", "scheme", "network", "extra"] {
        let mut accepted = serde_json::to_value(&route().accepts[0]).unwrap();
        accepted[field] = json!("changed");
        let payload = json!({"x402Version":2,"accepted":accepted,"payload":{"transactionId":"tx"}});
        let response = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/report")
                    .header("Payment-Signature", inflow_x402::encode(&payload))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 402);
    }
    let response = app
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    assert!(!response.headers().contains_key("cache-control"));
    assert!(
        platform
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.url.ends_with("/supported"))
    );
}

#[tokio::test]
async fn paid_response_preserves_cache_directives_and_resource_customization() {
    #[derive(serde::Serialize)]
    struct Custom {
        enabled: bool,
    }
    impl x402_types::scheme::ExtensionKey for Custom {
        const EXTENSION_KEY: &'static str = "example";
    }
    for (headers, expected) in [
        (vec![], vec!["private"]),
        (vec!["max-age=60"], vec!["max-age=60", "private"]),
        (vec!["no-store"], vec!["no-store", "private"]),
        (
            vec!["max-age=60", " PrIvAtE "],
            vec!["max-age=60", " PrIvAtE "],
        ),
    ] {
        let platform = Platform {
            calls: Default::default(),
            valid: true,
            settled: true,
            status: 200,
        };
        let layer = payment_layer(
            facilitator(&platform),
            route(),
            "https://merchant.example/report",
        )
        .unwrap()
        .with_description("Report".into())
        .with_mime_type("text/plain".into())
        .with_extension(Custom { enabled: true });
        let app = Router::new().route(
            "/report",
            get(move || {
                let headers = headers.clone();
                async move {
                    let mut response = http::Response::new(Body::from("report"));
                    for value in headers {
                        response
                            .headers_mut()
                            .append("cache-control", value.parse().unwrap());
                    }
                    response
                }
            })
            .route_layer(layer),
        );
        let challenge = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .uri("/report")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let required =
            inflow_x402::decode(challenge.headers()["payment-required"].to_str().unwrap()).unwrap();
        assert_eq!(required["resource"]["description"], "Report");
        assert_eq!(required["resource"]["mimeType"], "text/plain");
        assert_eq!(required["extensions"]["example"]["enabled"], true);
        let payload =
            json!({"x402Version":2,"accepted":route().accepts[0],"payload":{"transactionId":"tx"}});
        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/report")
                    .header("payment-signature", inflow_x402::encode(&payload))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get_all("cache-control")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn rejects_unsafe_or_unsupported_route_configuration() {
    let p = Platform {
        calls: Default::default(),
        valid: true,
        settled: true,
        status: 200,
    };
    for url in [
        "relative",
        "ftp://host/path",
        "https://user:password@host/path",
        "mailto:test@example.com",
    ] {
        assert!(payment_layer(facilitator(&p), route(), url).is_err());
    }
    let mut r = route();
    r.accepts.clear();
    assert!(payment_layer(facilitator(&p), r, "https://example.com/").is_err());
    let mut r = route();
    r.accepts[0].scheme = "upto".into();
    assert!(payment_layer(facilitator(&p), r, "https://example.com/").is_err());
    let mut r = route();
    r.extensions.insert("future".into(), json!({}));
    assert!(payment_layer(facilitator(&p), r, "https://example.com/").is_err());
}
