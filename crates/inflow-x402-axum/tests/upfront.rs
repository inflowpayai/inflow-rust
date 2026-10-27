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
use tokio::sync::Semaphore;
use tower::ServiceExt;

#[derive(Clone)]
struct Platform {
    calls: Arc<Mutex<Vec<TransportRequest>>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    status: u16,
    body: Value,
    released: Arc<Semaphore>,
    entered: Arc<Semaphore>,
    succeed_on: usize,
    disconnect: bool,
}

impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            if request.url.ends_with("/supported") {
                return Ok(TransportResponse {
                    status: 200,
                    headers: Default::default(),
                    body: serde_json::to_vec(&json!({"kinds":[],"extensions":[],"signers":{}}))
                        .unwrap(),
                });
            }
            assert!(
                request.url.ends_with("/settle"),
                "unexpected payment endpoint: {}",
                request.url
            );
            self.calls.lock().unwrap().push(request);
            self.events.lock().unwrap().push("settle-start");
            self.entered.add_permits(1);
            self.released.acquire().await.unwrap().forget();
            self.events.lock().unwrap().push("settle-finish");
            if self.disconnect {
                return Err(TransportError::Network);
            }
            let recovered = self.calls.lock().unwrap().len() >= self.succeed_on;
            Ok(TransportResponse {
                status: if recovered { 200 } else { self.status },
                headers: [(
                    http::header::RETRY_AFTER,
                    http::HeaderValue::from_static("0"),
                )]
                .into_iter()
                .collect(),
                body: serde_json::to_vec(&if recovered {
                    json!({"success":true,"transaction":"tx","network":"inflow:1"})
                } else {
                    self.body.clone()
                })
                .unwrap(),
            })
        })
    }
}

fn platform(status: u16, body: Value) -> Platform {
    Platform {
        calls: Default::default(),
        events: Default::default(),
        status,
        body,
        released: Arc::new(Semaphore::new(0)),
        entered: Arc::new(Semaphore::new(0)),
        succeed_on: usize::MAX,
        disconnect: false,
    }
}

fn route(scheme: &str, network: &str, method: &str) -> Route {
    Route {accepts:vec![serde_json::from_value(json!({"scheme":scheme,"network":network,"amount":"100","asset":"USDC","payTo":"merchant","maxTimeoutSeconds":300,"extra":{"paymentFlow":"upfront","assetTransferMethod":method,"future":"preserved"}})).unwrap()],extensions:Default::default()}
}

fn app(platform: &Platform, route: Route, status: u16) -> Router {
    let facilitator = Facilitator::new(ClientOptions {
        transport: Some(Arc::new(platform.clone())),
        ..Default::default()
    })
    .unwrap();
    let layer = payment_layer(facilitator, route, "https://merchant.example/report").unwrap();
    let events = platform.events.clone();
    Router::new().route(
        "/report",
        get(move || {
            let events = events.clone();
            async move {
                events.lock().unwrap().push("handler");
                (http::StatusCode::from_u16(status).unwrap(), "resource")
            }
        })
        .route_layer(layer),
    )
}

fn request(route: &Route) -> http::Request<Body> {
    let identifier =
        inflow_x402::identifier_entry(&inflow_x402::identifier_declaration(), "pay_same-payment")
            .unwrap();
    let payload = json!({"x402Version":2,"accepted":route.accepts[0],"payload":{"transactionId":"tx"},"extensions":{"payment-identifier":identifier,"future":{"preserve":true}}});
    http::Request::builder()
        .uri("/report")
        .header("payment-signature", inflow_x402::encode(&payload))
        .body(Body::empty())
        .unwrap()
}

#[test]
fn middleware_rejects_ambiguous_or_unsupported_execution_flows() {
    let platform = platform(200, json!({}));
    for flow in [json!("authorization"), json!("future"), Value::Null] {
        let mut route = route("balance", "inflow:1", "default");
        let mut other = route.accepts[0].clone();
        other.extra.as_mut().unwrap()["paymentFlow"] = flow;
        route.accepts.push(other);
        let facilitator = Facilitator::new(ClientOptions {
            transport: Some(Arc::new(platform.clone())),
            ..Default::default()
        })
        .unwrap();
        assert!(payment_layer(facilitator, route, "https://merchant.example/report").is_err());
    }
    assert!(platform.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn real_upstream_settles_before_handler_and_keeps_receipts_on_handler_failure() {
    for (scheme, network, method) in [
        ("balance", "inflow:1", "default"),
        ("instrument", "inflow:1", "default"),
        ("exact", "eip155:8453", "eip3009"),
        ("exact", "eip155:8453", "permit2"),
        ("exact", "solana:mainnet", "solana"),
    ] {
        for status in [200, 400, 402, 412, 500] {
            let platform = platform(
                200,
                json!({"success":true,"transaction":"tx","network":network}),
            );
            let route = route(scheme, network, method);
            let task = tokio::spawn(app(&platform, route.clone(), status).oneshot(request(&route)));
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                platform.entered.acquire(),
            )
            .await
            .unwrap()
            .unwrap()
            .forget();
            assert_eq!(*platform.events.lock().unwrap(), ["settle-start"]);
            platform.released.add_permits(1);
            let response = task.await.unwrap().unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(
                *platform.events.lock().unwrap(),
                ["settle-start", "settle-finish", "handler"]
            );
            let receipt =
                inflow_x402::decode(response.headers()["payment-response"].to_str().unwrap())
                    .unwrap();
            assert_eq!(receipt["success"], true);
            assert_eq!(receipt["transaction"], "tx");
            let calls = platform.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
            assert_eq!(
                body["paymentRequirements"]["extra"]["paymentFlow"],
                "upfront"
            );
            assert_eq!(
                body["paymentPayload"]["extensions"]["future"]["preserve"],
                true
            );
            assert_eq!(
                body["paymentPayload"]["extensions"]["payment-identifier"]["info"]["id"],
                "pay_same-payment"
            );
        }
    }
}

#[tokio::test]
async fn settlement_rejection_uncertainty_and_malformed_results_never_execute_handler() {
    for (status, body) in [
        (
            200,
            json!({"success":false,"transaction":"","network":"inflow:1"}),
        ),
        (200, json!({})),
        (200, json!({"success":"true"})),
        (503, json!({"message":"outcome unknown"})),
        (403, json!({"message":"rejected"})),
        (409, json!({"code":"unrelated_conflict"})),
    ] {
        let platform = platform(status, body);
        platform.released.add_permits(10);
        let route = route("balance", "inflow:1", "default");
        let response = app(&platform, route.clone(), 200)
            .oneshot(request(&route))
            .await
            .unwrap();
        assert_eq!(response.status(), 402);
        assert!(!platform.events.lock().unwrap().contains(&"handler"));
        assert!(!response.headers().contains_key("payment-response"));
    }
}

#[tokio::test]
async fn pending_retries_preserve_payment_and_only_final_success_runs_handler() {
    for succeed_on in [3, usize::MAX] {
        let mut platform = platform(409, json!({"errorReason":"idempotency_pending"}));
        platform.succeed_on = succeed_on;
        platform.released.add_permits(5);
        let route = route("balance", "inflow:1", "default");
        let response = app(&platform, route.clone(), 200)
            .oneshot(request(&route))
            .await
            .unwrap();
        let success = succeed_on == 3;
        assert_eq!(response.status(), if success { 200 } else { 402 });
        assert_eq!(
            platform
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "handler")
                .count(),
            usize::from(success)
        );
        let calls = platform.calls.lock().unwrap();
        assert_eq!(calls.len(), if success { 3 } else { 5 });
        assert!(calls.iter().all(|call| call.body == calls[0].body));
    }
    let mut platform = platform(
        200,
        json!({"success":true,"transaction":"tx","network":"inflow:1"}),
    );
    platform.disconnect = true;
    platform.released.add_permits(1);
    let route = route("balance", "inflow:1", "default");
    assert_eq!(
        app(&platform, route.clone(), 200)
            .oneshot(request(&route))
            .await
            .unwrap()
            .status(),
        402
    );
    assert!(!platform.events.lock().unwrap().contains(&"handler"));
    assert_eq!(platform.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_or_modified_payment_never_settles_or_runs_handler() {
    let platform = platform(
        200,
        json!({"success":true,"transaction":"tx","network":"inflow:1"}),
    );
    let route = route("balance", "inflow:1", "default");
    let app = app(&platform, route.clone(), 200);
    let unpaid = app
        .clone()
        .oneshot(
            http::Request::builder()
                .uri("/report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unpaid.status(), 402);
    for field in ["paymentFlow", "amount"] {
        let mut changed = route.clone();
        if field == "paymentFlow" {
            changed.accepts[0].extra.as_mut().unwrap()[field] = json!("authorization");
        } else {
            changed.accepts[0].amount = "1".into();
        }
        assert_eq!(
            app.clone()
                .oneshot(request(&changed))
                .await
                .unwrap()
                .status(),
            402
        );
    }
    assert!(platform.calls.lock().unwrap().is_empty());
    assert!(platform.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn repeated_and_concurrent_requests_keep_the_same_payment_identifier_but_repeat_fulfillment()
{
    let platform = platform(
        200,
        json!({"success":true,"transaction":"tx","network":"inflow:1"}),
    );
    platform.released.add_permits(3);
    let route = route("balance", "inflow:1", "default");
    let app = app(&platform, route.clone(), 200);
    let (a, b) = tokio::join!(
        app.clone().oneshot(request(&route)),
        app.clone().oneshot(request(&route))
    );
    let c = app.oneshot(request(&route)).await.unwrap();
    for response in [a.unwrap(), b.unwrap(), c] {
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        platform
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|v| **v == "handler")
            .count(),
        3
    );
    let calls = platform.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].body, calls[1].body);
    assert_eq!(calls[1].body, calls[2].body);
}
